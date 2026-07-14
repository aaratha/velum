// crates/app/src/main.rs
use std::{num::NonZeroU32, rc::Rc};

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

struct App {
    page: PdfiumPage,
    width: u32,
    height: u32,
    pixels: Vec<u32>,
    window: Option<Rc<Window>>,
    context: Option<softbuffer::Context<Rc<Window>>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
}

impl App {
    /// Re-renders the page to fit inside a `width` x `height` box, preserving aspect ratio and
    /// centering the result over a backdrop, then stores the resulting pixel buffer.
    fn render_to_fit(&mut self, width: u32, height: u32) {
        let page_w = self.page.width();
        let page_h = self.page.height();
        let scale = (width as f32 / page_w).min(height as f32 / page_h);
        let render_w = ((page_w * scale).round() as i32).max(1);
        let render_h = ((page_h * scale).round() as i32).max(1);

        let bitmap = self
            .page
            .render(
                &PdfiumRenderConfig::new()
                    .with_size(render_w, render_h)
                    .with_scale(scale),
            )
            .expect("failed to render page");
        let rgba = bitmap.as_rgba_bytes().expect("failed to read bitmap");
        let bw = bitmap.width() as u32;
        let bh = bitmap.height() as u32;

        let mut pixels = vec![BACKDROP; (width * height) as usize];
        let x_off = (width.saturating_sub(bw)) / 2;
        let y_off = (height.saturating_sub(bh)) / 2;

        for y in 0..bh {
            let src_row = &rgba[(y * bw * 4) as usize..((y * bw * 4) + bw * 4) as usize];
            let dst_start = ((y + y_off) * width + x_off) as usize;
            for x in 0..bw as usize {
                let p = &src_row[x * 4..x * 4 + 4];
                pixels[dst_start + x] = (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32;
            }
        }

        self.width = width;
        self.height = height;
        self.pixels = pixels;
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
                self.render_to_fit(new_size.width, new_size.height);
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
        window: None,
        context: None,
        surface: None,
    };
    app.render_to_fit(width, height);

    event_loop.run_app(&mut app)?;
    Ok(())
}
