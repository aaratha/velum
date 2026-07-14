// crates/app/src/main.rs
use iced::widget::{container, image};
use iced::window;
use iced::{Background, Color, ContentFit, Element, Length, Size, Subscription, Task, Theme};
use iced::time::{Duration, Instant};
use pdfium::{set_library_location, PdfiumDocument, PdfiumPage, PdfiumRenderConfig};

/// Backdrop shown around the page when the window's aspect ratio doesn't match the page's.
const BACKDROP: Color = Color::from_rgb(0x78 as f32 / 255.0, 0x78 as f32 / 255.0, 0x78 as f32 / 255.0);

/// How long to let the window sit still after a resize before doing a full,
/// crisp pdfium re-render at the new physical size.
const RESIZE_SETTLE: Duration = Duration::from_millis(120);

/// Initial window width, in logical pixels. Height is derived from the page's aspect ratio.
const INITIAL_WIDTH: f32 = 900.0;

/// Computes the largest `page_w` x `page_h` box (rounded to pixels) that fits inside
/// `box_w` x `box_h` while preserving aspect ratio, along with the scale factor used.
fn fit_size(page_w: f32, page_h: f32, box_w: u32, box_h: u32) -> (u32, u32, f32) {
    let scale = (box_w as f32 / page_w).min(box_h as f32 / page_h);
    let w = ((page_w * scale).round() as u32).max(1);
    let h = ((page_h * scale).round() as u32).max(1);
    (w, h, scale)
}

struct PdfViewer {
    page: PdfiumPage,
    handle: image::Handle,
    /// The window's own id, captured once it opens. This is what you'd pass to
    /// `iced::window::run(id, |window| { ... })` to reach the raw `NSWindow` (via
    /// `raw_window_handle::HasWindowHandle`) and apply vibrancy with the `window-vibrancy`
    /// crate, e.g. `window_vibrancy::apply_vibrancy(window, NSVisualEffectMaterial::Sidebar, ..)`.
    /// Left unused for now; this is groundwork for that follow-up.
    window_id: Option<window::Id>,
    scale_factor: f32,
    logical_size: Size,
    last_rendered_physical: (u32, u32),
    pending_resize_at: Option<Instant>,
}

#[derive(Debug, Clone)]
enum Message {
    WindowEvent(window::Id, window::Event),
    Tick(Instant),
}

impl PdfViewer {
    /// Builds the initial state from an already-opened page and the logical window size that
    /// was used to size the window itself (see `main`, which needs the page's aspect ratio
    /// before the window exists).
    fn boot(page: PdfiumPage, initial_size: Size) -> Self {
        let mut state = Self {
            page,
            handle: image::Handle::from_rgba(1, 1, vec![0, 0, 0, 0]),
            window_id: None,
            scale_factor: 1.0,
            logical_size: initial_size,
            last_rendered_physical: (0, 0),
            pending_resize_at: None,
        };
        state.render_full();
        state
    }

    /// Full pdfium rasterization at the window's current physical size. Expensive; only call
    /// once a resize has settled (or on startup/rescale).
    fn render_full(&mut self) {
        let target_w = (self.logical_size.width * self.scale_factor).round().max(1.0) as u32;
        let target_h = (self.logical_size.height * self.scale_factor).round().max(1.0) as u32;
        if (target_w, target_h) == self.last_rendered_physical {
            return;
        }

        let (render_w, render_h, scale) =
            fit_size(self.page.width(), self.page.height(), target_w, target_h);

        let bitmap = self
            .page
            .render(
                &PdfiumRenderConfig::new()
                    .with_size(render_w as i32, render_h as i32)
                    .with_scale(scale),
            )
            .expect("failed to render page");
        let pixels = bitmap.as_rgba_bytes().expect("failed to read bitmap");

        self.handle = image::Handle::from_rgba(render_w, render_h, pixels);
        self.last_rendered_physical = (target_w, target_h);
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowEvent(id, event) => {
                match event {
                    window::Event::Opened { size, .. } => {
                        self.window_id = Some(id);
                        self.logical_size = size;
                        self.render_full();
                    }
                    window::Event::Resized(size) => {
                        self.logical_size = size;
                        self.pending_resize_at = Some(Instant::now());
                    }
                    window::Event::Rescaled(factor) => {
                        self.scale_factor = factor;
                        self.pending_resize_at = Some(Instant::now());
                    }
                    _ => {}
                }
                Task::none()
            }
            Message::Tick(_now) => {
                if let Some(at) = self.pending_resize_at {
                    if at.elapsed() >= RESIZE_SETTLE {
                        self.pending_resize_at = None;
                        self.render_full();
                    }
                }
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        container(
            image(self.handle.clone())
                .width(Length::Fill)
                .height(Length::Fill)
                .content_fit(ContentFit::Contain),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(|_theme: &Theme| container::Style {
            background: Some(Background::Color(BACKDROP)),
            ..container::Style::default()
        })
        .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            window::events().map(|(id, event)| Message::WindowEvent(id, event)),
            iced::time::every(Duration::from_millis(50)).map(Message::Tick),
        ])
    }
}

fn main() -> iced::Result {
    let exe_dir = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    set_library_location(exe_dir.to_str().unwrap());

    let doc =
        PdfiumDocument::new_from_path("example.pdf", None).expect("failed to open example.pdf");
    let page = doc.page(0).expect("failed to load first page");
    let initial_size = Size::new(INITIAL_WIDTH, INITIAL_WIDTH * page.height() / page.width());

    // `BootFn` requires `Fn() -> State`, but `PdfViewer::boot` is only ever called once, so we
    // stash the already-opened page behind a `RefCell` to move it in on that first call.
    let page_cell = std::cell::RefCell::new(Some(page));
    let boot = move || PdfViewer::boot(page_cell.borrow_mut().take().expect("boot called once"), initial_size);

    iced::application(boot, PdfViewer::update, PdfViewer::view)
        .title("PDF Viewer")
        .subscription(PdfViewer::subscription)
        .window(window::Settings {
            size: initial_size,
            ..window::Settings::default()
        })
        .run()
}
