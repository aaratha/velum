// crates/app/src/main.rs
use iced::time::{Duration, Instant};
use iced::widget::{Space, Stack, column, container, image, mouse_area, row, slider, text};
use iced_color_picker::{Spectrum, color_picker};
use iced::window;
use iced::{
    Background, Color, ContentFit, Element, Length, Padding, Size, Subscription, Task, mouse,
};
use pdfium::{
    PdfiumDocument, PdfiumPage, PdfiumRenderConfig, PdfiumRenderFlags, set_library_location,
};
use window_vibrancy::{NSVisualEffectMaterial, NSVisualEffectState};

/// A native vibrancy material applied to one rectangular region of the window, in points
/// relative to the window's own content area (`x`/`y` from the top-left... in practice, from
/// whichever corner `region.height`/`region.width` happen to span the full content area, since
/// `vibrancy::sync` only ever offsets by the content view's own origin -- see its doc comment).
///
/// Multiple regions can coexist -- one per widget/container that wants its own material -- and
/// are kept in sync by calling `vibrancy::sync` again whenever the layout changes. Regions are
/// matched across calls by `name`, so re-syncing resizes/restyles existing native views in place
/// instead of recreating them, which is what makes this cheap enough to call on every resize.
struct VibrancyRegion {
    name: &'static str,
    material: NSVisualEffectMaterial,
    state: NSVisualEffectState,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

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
    use objc2_app_kit::{NSView, NSVisualEffectBlendingMode, NSWindowOrderingMode};
    use objc2_foundation::{MainThreadMarker, NSInteger, NSPoint, NSRect, NSSize};
    use raw_window_handle::RawWindowHandle;
    use window_vibrancy::NSVisualEffectViewTagged;

    use super::VibrancyRegion;

    /// Derives a stable NSView tag from a region's name, so a later call can find (and update)
    /// a previously-inserted view instead of creating a duplicate.
    fn tag_for(name: &str) -> NSInteger {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        name.hash(&mut hasher);
        hasher.finish() as NSInteger
    }

    /// Creates or repositions a blur view for each given region, behind `window`'s content.
    /// Safe and cheap to call repeatedly (e.g. on every resize) to keep regions in sync with
    /// layout.
    pub fn sync(window: &dyn iced::window::Window, regions: &[VibrancyRegion]) {
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
        // Regions are specified relative to the content view's own origin within `parent`, so
        // this is the only place that needs to know about that offset.
        let content_frame = view.frame();

        for region in regions {
            let tag = tag_for(region.name);
            let rect = NSRect::new(
                NSPoint::new(
                    content_frame.origin.x + region.x,
                    content_frame.origin.y + region.y,
                ),
                NSSize::new(region.width, region.height),
            );

            let blurred_view = parent
                .viewWithTag(tag)
                .and_then(|existing| existing.downcast::<NSVisualEffectViewTagged>().ok())
                .unwrap_or_else(|| unsafe {
                    let created = NSVisualEffectViewTagged::initWithFrame(mtm.alloc(), rect, tag);
                    created.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
                    parent.addSubview_positioned_relativeTo(
                        &created,
                        NSWindowOrderingMode::Below,
                        Some(view),
                    );
                    created
                });

            unsafe {
                blurred_view.setFrame(rect);
                blurred_view.setMaterial(objc2_app_kit::NSVisualEffectMaterial(
                    region.material as NSInteger,
                ));
                blurred_view.setState(objc2_app_kit::NSVisualEffectState(
                    region.state as NSInteger,
                ));
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod vibrancy {
    pub fn sync(_window: &dyn iced::window::Window, _regions: &[super::VibrancyRegion]) {}
}

/// How long to let the window sit still after a resize before doing a full,
/// crisp pdfium re-render at the new physical size.
const RESIZE_SETTLE: Duration = Duration::from_millis(120);

/// Initial window width, in logical pixels. Height is derived from the page's aspect ratio.
const INITIAL_WIDTH: f32 = 900.0;

/// Width of the sidebar, in logical points. Shared between the iced layout (`view`) and the
/// native vibrancy region geometry (`PdfViewer::vibrancy_regions`) so they can't drift apart.
const SIDEBAR_WIDTH: f32 = 282.0;

/// Sidebar row metrics, in logical points. `view` gives every label/slider/swatch an *explicit*
/// height using these constants (rather than whatever their default sizing happens to be), so
/// that a color popup's vertical position can be computed exactly to line up with the swatch
/// that opened it, instead of guessed. Keep these in sync with the rows built in `view`.
const SIDEBAR_PADDING: f32 = 16.0;
const ROW_SPACING: f32 = 16.0;
const LABEL_HEIGHT: f32 = 18.0;
const SLIDER_HEIGHT: f32 = 18.0;
const SWATCH_ROW_HEIGHT: f32 = 20.0;

/// Vertical offset (from the sidebar's top) of the "Text color" swatch's row.
const TEXT_SWATCH_TOP: f32 =
    SIDEBAR_PADDING + 2.0 * (LABEL_HEIGHT + ROW_SPACING + SLIDER_HEIGHT + ROW_SPACING);

/// Vertical offset (from the sidebar's top) of the "Backdrop color" swatch's row.
const BACKDROP_SWATCH_TOP: f32 = TEXT_SWATCH_TOP + SWATCH_ROW_HEIGHT + ROW_SPACING;

/// How far left of the sidebar's right edge a color popup's card starts, so it visibly overlaps
/// the sidebar (per its swatch) instead of floating disconnected from it in the page area.
const POPUP_SIDEBAR_OVERLAP: f32 = 24.0;

/// Computes the largest `page_w` x `page_h` box (rounded to pixels) that fits inside
/// `box_w` x `box_h` while preserving aspect ratio, along with the scale factor used.
fn fit_size(page_w: f32, page_h: f32, box_w: u32, box_h: u32) -> (u32, u32, f32) {
    let scale = (box_w as f32 / page_w).min(box_h as f32 / page_h);
    let w = ((page_w * scale).round() as u32).max(1);
    let h = ((page_h * scale).round() as u32).max(1);
    (w, h, scale)
}

/// Which color the currently-open picker popup (if any) is editing. Drives both which swatch
/// toggled it open and which field `view` reads the color/message-constructor pair from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorField {
    Text,
    Backdrop,
}

/// A clickable label + color swatch. Pressing it sends `TogglePicker(field)`, which `update`
/// treats as an open/close toggle (see its match arm), so clicking an already-open swatch closes
/// its popup.
fn color_swatch(label: &str, color: Color, field: ColorField) -> Element<'_, Message> {
    mouse_area(
        row![
            text(label).height(SWATCH_ROW_HEIGHT),
            container(Space::new())
                .style(move |_theme| container::Style {
                    background: Some(color.into()),
                    ..container::Style::default()
                })
                .width(28)
                .height(18),
        ]
        .spacing(8)
        .align_y(iced::Alignment::Center)
        .height(SWATCH_ROW_HEIGHT),
    )
    .interaction(mouse::Interaction::Pointer)
    .on_press(Message::TogglePicker(field))
    .into()
}

/// The floating popup for a swatch: a saturation/value square and a hue strip, both driving the
/// same `on_change` message. `on_change` is called once per widget it's wired into, so it needs
/// to be cheaply reusable -- bare message-variant constructors (e.g. `Message::TextColorChanged`)
/// satisfy this for free, being zero-sized `Copy` function items.
///
/// Wrapped in its own `mouse_area` (reporting a constant, non-`None` interaction over its whole
/// card, not just over the pickers themselves) so that the window-covering "click off to close"
/// catcher in `view` yields to it -- iced's `Stack` skips lower layers under the cursor wherever
/// a higher layer already reports mouse interaction there, which is what actually suppresses the
/// close click; `on_press` isn't needed for that.
fn color_picker_popup<'a>(
    color: Color,
    on_change: impl Fn(Color) -> Message + Copy + 'a,
) -> Element<'a, Message> {
    mouse_area(
        container(
            column![
                color_picker(color, on_change).width(220).height(160),
                color_picker(color, on_change)
                    .spectrum(Spectrum::HueHorizontal)
                    .width(220)
                    .height(24),
            ]
            .spacing(8),
        )
        .padding(12)
        .style(|_theme| container::Style {
            background: Some(Color::from_rgb8(38, 38, 38).into()),
            border: iced::Border {
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.08),
                width: 1.0,
                radius: 8.0.into(),
            },
            ..container::Style::default()
        }),
    )
    .interaction(mouse::Interaction::Idle)
    .into()
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
    /// How opaque the backdrop drawn behind the page (and the letterboxed area around it) is,
    /// from `0.0` (fully see-through to the vibrancy blur) to `1.0` (solid color).
    background_opacity: f32,
    /// Same as `background_opacity`, but for the sidebar's own backdrop.
    sidebar_opacity: f32,
    /// Whether page content (text/graphics) is recolored to white instead of its default black.
    text_color: Color,
    backdrop_color: Color,
    /// Which color swatch's picker popup is currently showing, if any.
    open_picker: Option<ColorField>,
}

#[derive(Debug, Clone)]
enum Message {
    WindowEvent(window::Id, window::Event),
    Tick(Instant),
    BackgroundOpacityChanged(f32),
    SidebarOpacityChanged(f32),
    TextColorChanged(Color),
    BackdropColorChanged(Color),
    TogglePicker(ColorField),
    ClosePicker,
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
            sidebar_opacity: 0.0,
            text_color: Color::BLACK,
            backdrop_color: Color::WHITE,
            open_picker: None,
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
        let target_w = (self.logical_size.width * self.scale_factor)
            .round()
            .max(1.0) as u32;
        let target_h = (self.logical_size.height * self.scale_factor)
            .round()
            .max(1.0) as u32;
        if (target_w, target_h) == self.last_rendered_physical {
            return;
        }

        self.rasterize(target_w, target_h);
    }

    /// Rasterizes the page to fit inside a `width` x `height` box, recoloring page content to
    /// `text_color`, and always updates `handle`/`last_rendered_physical`.
    fn rasterize(&mut self, width: u32, height: u32) {
        let (render_w, render_h, scale) =
            fit_size(self.page.width(), self.page.height(), width, height);

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

        // Recolor content pixels (anything pdfium actually painted, i.e. non-transparent) to
        // `text_color`, keeping their original alpha so anti-aliased edges still blend
        // correctly.
        let [ink_r, ink_g, ink_b, _] = self.text_color.into_rgba8();
        for pixel in pixels.chunks_exact_mut(4) {
            if pixel[3] > 0 {
                pixel[0] = ink_r;
                pixel[1] = ink_g;
                pixel[2] = ink_b;
            }
        }

        self.handle = image::Handle::from_rgba(render_w, render_h, pixels);
        self.last_rendered_physical = (width, height);
    }

    /// The native vibrancy regions matching the current layout: a `Sidebar`-material strip on
    /// the left (`SIDEBAR_WIDTH` wide, matching the sidebar in `view`) and `HudWindow` behind
    /// the rest. Recomputed from `logical_size`, so calling `vibrancy::sync` with this after any
    /// layout change keeps the native views in sync.
    fn vibrancy_regions(&self) -> Vec<VibrancyRegion> {
        let width = self.logical_size.width as f64;
        let height = self.logical_size.height as f64;
        let sidebar_width = (SIDEBAR_WIDTH as f64).min(width);

        vec![
            VibrancyRegion {
                name: "sidebar",
                material: NSVisualEffectMaterial::Sidebar,
                state: NSVisualEffectState::FollowsWindowActiveState,
                x: 0.0,
                y: 0.0,
                width: sidebar_width,
                height,
            },
            VibrancyRegion {
                name: "content",
                material: NSVisualEffectMaterial::HudWindow,
                state: NSVisualEffectState::FollowsWindowActiveState,
                x: sidebar_width,
                y: 0.0,
                width: (width - sidebar_width).max(0.0),
                height,
            },
        ]
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowEvent(id, event) => {
                let mut resync_vibrancy = false;
                match event {
                    window::Event::Opened { size, .. } => {
                        self.window_id = Some(id);
                        self.logical_size = size;
                        self.render_full();
                        resync_vibrancy = true;
                    }
                    window::Event::Resized(size) => {
                        self.logical_size = size;
                        self.pending_resize_at = Some(Instant::now());
                        resync_vibrancy = true;
                    }
                    window::Event::Rescaled(factor) => {
                        self.scale_factor = factor;
                        self.pending_resize_at = Some(Instant::now());
                    }
                    _ => {}
                }
                if resync_vibrancy {
                    let regions = self.vibrancy_regions();
                    return window::run(id, move |window| vibrancy::sync(window, &regions))
                        .discard();
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
            Message::SidebarOpacityChanged(opacity) => {
                self.sidebar_opacity = opacity;
                Task::none()
            }
            Message::TextColorChanged(color) => {
                self.text_color = color;
                let (width, height) = self.last_rendered_physical;
                self.rasterize(width, height);
                Task::none()
            }
            Message::BackdropColorChanged(color) => {
                self.backdrop_color = color;
                Task::none()
            }
            Message::TogglePicker(field) => {
                self.open_picker = if self.open_picker == Some(field) {
                    None
                } else {
                    Some(field)
                };
                Task::none()
            }
            Message::ClosePicker => {
                self.open_picker = None;
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
                self.backdrop_color.scale_alpha(self.background_opacity),
            )),
            ..container::Style::default()
        });

        let color_customizer = column![
            color_swatch("Text color", self.text_color, ColorField::Text),
            color_swatch("Backdrop color", self.backdrop_color, ColorField::Backdrop),
        ]
        .spacing(16);

        let sidebar = container(
            column![
                text("Background opacity").height(LABEL_HEIGHT),
                slider(
                    0.0..=1.0,
                    self.background_opacity,
                    Message::BackgroundOpacityChanged
                )
                .step(0.01)
                .height(SLIDER_HEIGHT),
                text("Sidebar opacity").height(LABEL_HEIGHT),
                slider(
                    0.0..=1.0,
                    self.sidebar_opacity,
                    Message::SidebarOpacityChanged
                )
                .step(0.01)
                .height(SLIDER_HEIGHT),
                color_customizer,
            ]
            .spacing(ROW_SPACING),
        )
        .width(Length::Fixed(SIDEBAR_WIDTH))
        .height(Length::Fill)
        .padding(SIDEBAR_PADDING)
        .style(move |_theme| container::Style {
            background: Some(Background::Color(
                self.backdrop_color.scale_alpha(self.sidebar_opacity),
            )),
            ..container::Style::default()
        });

        let content: Element<'_, Message> = row![sidebar, page].into();

        let Some(field) = self.open_picker else {
            return content;
        };

        let (color, on_change, top_offset): (Color, fn(Color) -> Message, f32) = match field {
            ColorField::Text => (self.text_color, Message::TextColorChanged, TEXT_SWATCH_TOP),
            ColorField::Backdrop => (
                self.backdrop_color,
                Message::BackdropColorChanged,
                BACKDROP_SWATCH_TOP,
            ),
        };

        // Clicking anywhere outside the popup closes it -- including elsewhere in the sidebar,
        // not just the page. This sits *below* both the sidebar/page content and the popup in
        // the stack, so it only ever fires where neither of those already reports mouse
        // interaction: `Stack` skips lower layers under the cursor wherever a higher layer does,
        // which is what lets normal sidebar clicks (the swatch that opened this popup, other
        // swatches, sliders) and clicks on the popup itself all take priority over closing.
        let closer = mouse_area(Space::new().width(Length::Fill).height(Length::Fill))
            .on_press(Message::ClosePicker);

        let popup = container(color_picker_popup(color, on_change)).padding(
            Padding::ZERO
                .left(SIDEBAR_WIDTH - POPUP_SIDEBAR_OVERLAP)
                .top(top_offset),
        );

        Stack::new().push(closer).push(content).push(popup).into()
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

    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "example.pdf".to_string());

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
