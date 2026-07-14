// crates/app/src/main.rs
use std::{
    num::NonZeroU32,
    rc::Rc,
    time::{Duration, Instant},
};

use image::{imageops::FilterType, RgbaImage};
use pdfium::{set_library_location, PdfiumDocument, PdfiumPage, PdfiumRenderConfig};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowAttributes, WindowId},
};

/// Backdrop shown around the page when the window's aspect ratio doesn't match the page's.
const BACKDROP: u32 = 0x00787878;

/// How long to wait after the last resize event before doing a full, crisp pdfium re-render.
/// During the drag itself we just cheaply rescale the last full render instead.
const RESIZE_SETTLE: Duration = Duration::from_millis(120);

/// Computes the largest `page_w` x `page_h` box (rounded to pixels) that fits inside
/// `box_w` x `box_h` while preserving aspect ratio, along with the scale factor used.
fn fit_size(page_w: f32, page_h: f32, box_w: u32, box_h: u32) -> (u32, u32, f32) {
    let scale = (box_w as f32 / page_w).min(box_h as f32 / page_h);
    let w = ((page_w * scale).round() as u32).max(1);
    let h = ((page_h * scale).round() as u32).max(1);
    (w, h, scale)
}

/// Centers `img` over a `canvas_w` x `canvas_h` backdrop and returns the composed pixel buffer.
fn compose(canvas_w: u32, canvas_h: u32, img: &RgbaImage) -> Vec<u32> {
    let (iw, ih) = img.dimensions();
    let raw = img.as_raw();
    let mut pixels = vec![BACKDROP; (canvas_w * canvas_h) as usize];
    let x_off = (canvas_w.saturating_sub(iw)) / 2;
    let y_off = (canvas_h.saturating_sub(ih)) / 2;

    for y in 0..ih {
        let src_row = &raw[(y * iw * 4) as usize..((y * iw * 4) + iw * 4) as usize];
        let dst_start = ((y + y_off) * canvas_w + x_off) as usize;
        for x in 0..iw as usize {
            let p = &src_row[x * 4..x * 4 + 4];
            pixels[dst_start + x] = (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32;
        }
    }

    pixels
}

struct App {
    page: PdfiumPage,
    width: u32,
    height: u32,
    pixels: Vec<u32>,
    /// Last full-quality pdfium render, used as the source for cheap live-resize previews.
    base: Option<RgbaImage>,
    resize_deadline: Option<Instant>,
    window: Option<Rc<Window>>,
    context: Option<softbuffer::Context<Rc<Window>>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl App {
    /// Does a full pdfium rasterization at the given box size. Expensive; only call once a
    /// resize has settled (or on startup).
    fn render_full(&mut self, width: u32, height: u32) {
        let (render_w, render_h, scale) =
            fit_size(self.page.width(), self.page.height(), width, height);

        let bitmap = self
            .page
            .render(
                &PdfiumRenderConfig::new()
                    .with_size(render_w as i32, render_h as i32)
                    .with_scale(scale),
            )
            .expect("failed to render page");
        let image = bitmap.as_rgba8_image().expect("failed to read bitmap").into_rgba8();

        self.pixels = compose(width, height, &image);
        self.base = Some(image);
        self.width = width;
        self.height = height;
    }

    /// Cheaply rescales the last full render to fit the new box. No pdfium call, so this stays
    /// fast even for large pages during an interactive drag.
    fn render_preview(&mut self, width: u32, height: u32) {
        let Some(base) = &self.base else {
            self.render_full(width, height);
            return;
        };
        let (target_w, target_h, _) = fit_size(self.page.width(), self.page.height(), width, height);
        let scaled = image::imageops::resize(base, target_w, target_h, FilterType::Triangle);

        self.pixels = compose(width, height, &scaled);
        self.width = width;
        self.height = height;
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = WindowAttributes::default()
            .with_title("PDF Viewer")
            .with_inner_size(PhysicalSize::new(self.width, self.height));
        let window = Rc::new(
            event_loop
                .create_window(attrs)
                .expect("failed to create window"),
        );

        let context =
            softbuffer::Context::new(window.clone()).expect("failed to create softbuffer context");
        let mut surface = softbuffer::Surface::new(&context, window.clone())
            .expect("failed to create softbuffer surface");
        surface
            .resize(
                NonZeroU32::new(self.width).unwrap(),
                NonZeroU32::new(self.height).unwrap(),
            )
            .expect("failed to resize softbuffer surface");

        window.request_redraw();

        self.window = Some(window);
        self.context = Some(context);
        self.surface = Some(surface);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(new_size) => {
                if new_size.width == 0 || new_size.height == 0 {
                    return;
                }
                self.render_preview(new_size.width, new_size.height);
                if let Some(surface) = self.surface.as_mut() {
                    surface
                        .resize(
                            NonZeroU32::new(new_size.width).unwrap(),
                            NonZeroU32::new(new_size.height).unwrap(),
                        )
                        .expect("failed to resize softbuffer surface");
                }
                if let Some(window) = self.window.as_ref() {
                    window.request_redraw();
                }

                let deadline = Instant::now() + RESIZE_SETTLE;
                self.resize_deadline = Some(deadline);
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
            }
            WindowEvent::RedrawRequested => {
                if let Some(surface) = self.surface.as_mut() {
                    let mut buffer = surface.buffer_mut().expect("failed to lock softbuffer buffer");
                    buffer.copy_from_slice(&self.pixels);
                    buffer.present().expect("failed to present softbuffer buffer");
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(deadline) = self.resize_deadline else {
            return;
        };
        if Instant::now() < deadline {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
            return;
        }

        self.resize_deadline = None;
        event_loop.set_control_flow(ControlFlow::Wait);
        self.render_full(self.width, self.height);
        if let Some(window) = self.window.as_ref() {
            window.request_redraw();
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let exe_dir = std::env::current_exe()?.parent().unwrap().to_path_buf();
    set_library_location(exe_dir.to_str().unwrap());

    let doc = PdfiumDocument::new_from_path("example.pdf", None)?;
    let page = doc.page(0)?;
    let bitmap = page.render(&PdfiumRenderConfig::new().with_width(900))?;
    let width = bitmap.width() as u32;
    let height = bitmap.height() as u32;

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App {
        page,
        width,
        height,
        pixels: Vec::new(),
        base: None,
        resize_deadline: None,
        window: None,
        context: None,
        surface: None,
    };
    app.render_full(width, height);

    event_loop.run_app(&mut app)?;
    Ok(())
}
