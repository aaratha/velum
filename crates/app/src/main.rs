// crates/app/src/main.rs
use iced::widget::{column, container, image, row, slider, text, toggler};
use iced::window;
use iced::{Background, Color, ContentFit, Element, Length, Size, Subscription, Task};
use iced::time::{Duration, Instant};
use pdfium::{
    set_library_location, PdfiumDocument, PdfiumPage, PdfiumRenderConfig, PdfiumRenderFlags,
};
use window_vibrancy::{NSVisualEffectMaterial, NSVisualEffectState};

/// Applies macOS window vibrancy behind our rendered content.
///
/// We can't use `window_vibrancy::apply_vibrancy` directly: it inserts its blur view as a
/// *subview* of the target view, which is fine for ordinary AppKit content, but our target view
/// is also the one wgpu renders into (its `layer` is a `CAMetalLayer`). In AppKit, subviews
/// always draw in front of their parent's own layer content, regardless of subview ordering --
/// so the blur would end up covering our rendered page instead of sitting behind it. Inserting
/// the blur into the content view's *superview* instead makes it a true sibling, where z-order
/// (and thus our opaque page winning over the blur) is respected normally.
#[cfg(target_os = "macos")]
mod vibrancy {
    use objc2_app_kit::{
        NSAutoresizingMaskOptions, NSVisualEffectBlendingMode, NSView, NSWindowOrderingMode,
    };
    use objc2_foundation::{MainThreadMarker, NSInteger};
    use raw_window_handle::RawWindowHandle;
    use window_vibrancy::{NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectViewTagged};

    /// NSView tag applied to the inserted blur view, matching `window-vibrancy`'s own convention.
    const VIBRANCY_TAG: NSInteger = 91376254;

    pub fn apply(
        window: &dyn iced::window::Window,
        material: NSVisualEffectMaterial,
        state: NSVisualEffectState,
    ) {
        let Ok(handle) = window.window_handle() else {
            return;
        };
        let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
            return;
        };
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };

        // Safety: `ns_view` is a live `NSView*` for the duration of this call -- we're running
        // inside a callback dispatched by iced's runtime specifically for this open window.
        let view: &NSView = unsafe { handle.ns_view.cast().as_ref() };
        let Some(parent) = (unsafe { view.superview() }) else {
            return;
        };

        let blurred_view =
            unsafe { NSVisualEffectViewTagged::initWithFrame(mtm.alloc(), view.frame(), VIBRANCY_TAG) };
        unsafe {
            blurred_view.setMaterial(objc2_app_kit::NSVisualEffectMaterial(material as NSInteger));
            blurred_view.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
            blurred_view.setState(objc2_app_kit::NSVisualEffectState(state as NSInteger));
            blurred_view.setAutoresizingMask(
                NSAutoresizingMaskOptions::ViewWidthSizable
                    | NSAutoresizingMaskOptions::ViewHeightSizable,
            );
        }

        parent.addSubview_positioned_relativeTo(
            &blurred_view,
            NSWindowOrderingMode::Below,
            Some(view),
        );
    }
}

#[cfg(not(target_os = "macos"))]
mod vibrancy {
    pub fn apply(
        _window: &dyn iced::window::Window,
        _material: window_vibrancy::NSVisualEffectMaterial,
        _state: window_vibrancy::NSVisualEffectState,
    ) {
    }
}

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
    title: String,
    handle: image::Handle,
    /// The window's own id, captured once it opens, kept around for any future window-level
    /// operations (vibrancy itself is applied once, right after `Opened`, in `update`).
    window_id: Option<window::Id>,
    scale_factor: f32,
    logical_size: Size,
    last_rendered_physical: (u32, u32),
    pending_resize_at: Option<Instant>,
    /// How opaque the white backdrop drawn behind the page (and the letterboxed area around it)
    /// is, from `0.0` (fully see-through to the vibrancy blur) to `1.0` (solid white).
    background_opacity: f32,
    /// Whether page content (text/graphics) is recolored to white instead of its default black.
    text_white: bool,
}

#[derive(Debug, Clone)]
enum Message {
    WindowEvent(window::Id, window::Event),
    Tick(Instant),
    BackgroundOpacityChanged(f32),
    TextColorToggled(bool),
}

impl PdfViewer {
    /// Builds the initial state from an already-opened page and the logical window size that
    /// was used to size the window itself (see `main`, which needs the page's aspect ratio
    /// before the window exists).
    fn boot(page: PdfiumPage, title: String, initial_size: Size) -> Self {
        let mut state = Self {
            page,
            title,
            handle: image::Handle::from_rgba(1, 1, vec![0, 0, 0, 0]),
            window_id: None,
            scale_factor: 1.0,
            logical_size: initial_size,
            last_rendered_physical: (0, 0),
            pending_resize_at: None,
            background_opacity: 0.0,
            text_white: false,
        };
        state.render_full();
        state
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    /// Full pdfium rasterization at the window's current physical size. Expensive; only call
    /// once a resize has settled (or on startup/rescale). Skipped if the physical size hasn't
    /// actually changed since the last render -- use `rasterize` directly to force a fresh one
    /// (e.g. when only the text color changed).
    fn render_full(&mut self) {
        let target_w = (self.logical_size.width * self.scale_factor).round().max(1.0) as u32;
        let target_h = (self.logical_size.height * self.scale_factor).round().max(1.0) as u32;
        if (target_w, target_h) == self.last_rendered_physical {
            return;
        }

        self.rasterize(target_w, target_h);
    }

    /// Rasterizes the page to fit inside a `width` x `height` box, recoloring page content to
    /// white or black per `text_white`, and always updates `handle`/`last_rendered_physical`.
    fn rasterize(&mut self, width: u32, height: u32) {
        let (render_w, render_h, scale) = fit_size(self.page.width(), self.page.height(), width, height);

        let bitmap = self
            .page
            .render(
                &PdfiumRenderConfig::new()
                    .with_size(render_w as i32, render_h as i32)
                    .with_scale(scale)
                    .with_transparent_background()
                    // LCD-optimized subpixel text AA assumes compositing onto a known, fixed
                    // background color, which no longer holds now that blank areas are
                    // transparent over a blurred backdrop. Plain (grayscale) AA blends correctly
                    // over anything.
                    .with_flags(PdfiumRenderFlags::ANNOT),
            )
            .expect("failed to render page");
        let mut pixels = bitmap.as_rgba_bytes().expect("failed to read bitmap");

        // Recolor content pixels (anything pdfium actually painted, i.e. non-transparent) to a
        // flat white or black, keeping their original alpha so anti-aliased edges still blend
        // correctly.
        let ink = if self.text_white { 255 } else { 0 };
        for pixel in pixels.chunks_exact_mut(4) {
            if pixel[3] > 0 {
                pixel[0] = ink;
                pixel[1] = ink;
                pixel[2] = ink;
            }
        }

        self.handle = image::Handle::from_rgba(render_w, render_h, pixels);
        self.last_rendered_physical = (width, height);
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowEvent(id, event) => {
                match event {
                    window::Event::Opened { size, .. } => {
                        self.window_id = Some(id);
                        self.logical_size = size;
                        self.render_full();
                        return window::run(id, |window| {
                            vibrancy::apply(
                                window,
                                NSVisualEffectMaterial::HudWindow,
                                NSVisualEffectState::FollowsWindowActiveState,
                            );
                        })
                        .discard();
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
            Message::BackgroundOpacityChanged(opacity) => {
                self.background_opacity = opacity;
                Task::none()
            }
            Message::TextColorToggled(white) => {
                self.text_white = white;
                let (width, height) = self.last_rendered_physical;
                self.rasterize(width, height);
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        // The container's own background sits behind the image, so it shows through both the
        // letterboxed area *and* the page's own blank areas (rendered transparent in
        // `render_full`). At opacity 0 it's fully see-through to the vibrancy blur; raising the
        // slider fades in a solid backdrop -- black when the text is white and vice versa, so
        // content stays legible. Actual text/graphics content stays opaque either way, since the
        // page bitmap is drawn on top of this background.
        let backdrop_color = if self.text_white { Color::BLACK } else { Color::WHITE };
        let page = container(
            image(self.handle.clone())
                .width(Length::Fill)
                .height(Length::Fill)
                .content_fit(ContentFit::Contain),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .center_x(Length::Fill)
        .center_y(Length::Fill)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(
                backdrop_color.scale_alpha(self.background_opacity),
            )),
            ..container::Style::default()
        });

        let sidebar = container(
            column![
                text("Background opacity"),
                slider(
                    0.0..=1.0,
                    self.background_opacity,
                    Message::BackgroundOpacityChanged
                )
                .step(0.01),
                toggler(self.text_white)
                    .label("White text")
                    .on_toggle(Message::TextColorToggled),
            ]
            .spacing(16),
        )
        .width(Length::Fixed(180.0))
        .height(Length::Fill)
        .padding(16);

        row![sidebar, page].into()
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

    let path = std::env::args().nth(1).unwrap_or_else(|| "example.pdf".to_string());

    let doc = PdfiumDocument::new_from_path(&path, None)
        .unwrap_or_else(|err| panic!("failed to open {path}: {err}"));
    let page = doc.page(0).expect("failed to load first page");
    let initial_size = Size::new(INITIAL_WIDTH, INITIAL_WIDTH * page.height() / page.width());

    let title = std::path::Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or(path);

    // `BootFn` requires `Fn() -> State`, but `PdfViewer::boot` is only ever called once, so we
    // stash the already-opened page behind a `RefCell` to move it in on that first call.
    let page_cell = std::cell::RefCell::new(Some(page));
    let boot = move || {
        PdfViewer::boot(
            page_cell.borrow_mut().take().expect("boot called once"),
            title.clone(),
            initial_size,
        )
    };

    iced::application(boot, PdfViewer::update, PdfViewer::view)
        .title(PdfViewer::title)
        .subscription(PdfViewer::subscription)
        .style(|_state, theme| {
            use iced::theme::Base;

            iced::theme::Style {
                background_color: Color::TRANSPARENT,
                ..theme.base()
            }
        })
        .window(window::Settings {
            size: initial_size,
            transparent: true,
            ..window::Settings::default()
        })
        .run()
}
