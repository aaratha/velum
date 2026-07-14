// crates/app/src/main.rs
use std::{num::NonZeroU32, rc::Rc};

use pdfium::{set_library_location, PdfiumDocument, PdfiumRenderConfig};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::WindowEvent,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowAttributes, WindowId},
};

struct App {
    pixels: Vec<u32>,
    width: u32,
    height: u32,
    window: Option<Rc<Window>>,
    context: Option<softbuffer::Context<Rc<Window>>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
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
    let pixels: Vec<u32> = bitmap
        .as_rgba_bytes()?
        .chunks_exact(4)
        .map(|p| (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32)
        .collect();

    let event_loop = EventLoop::new()?;
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = App {
        pixels,
        width,
        height,
        window: None,
        context: None,
        surface: None,
    };

    event_loop.run_app(&mut app)?;
    Ok(())
}
