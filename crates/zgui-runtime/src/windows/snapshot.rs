//! One-shot offscreen rendering of a view into an RGBA bitmap ([`RenderedView`]).
//!
//! Renders an arbitrary [`IntoView`] inside an isolated temporary document that inherits the
//! source window's stylesheets, theme colour scheme, font collection and graphics device, then
//! reads back tightly packed straight-alpha `RGBA8` pixels suitable for a
//! [`WindowIcon`] or [`DragIcon`].

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use rustc_hash::FxHashMap;
use zgui_bits::DamageSet;
use zgui_dom::Document;
use zgui_geom::{Css, CssPx, DevicePx, Edges, Point, Rect, Scale, Size};
use zgui_interned::ElementName;
use zgui_layout::HitIndex;
use zgui_layout::style::DeviceStyle;
use zgui_layout::tree::LayoutTree;
use zgui_layout::tree::store::LayoutStore;
use zgui_paint::{ContentCache, PaintInput, Painter};
use zgui_platform::{Clock, DragIcon, PlatformError, Unsupported, WakeReason, Waker, WindowIcon};
use zgui_reactive::{Mounted, Owner};
use zgui_render::RenderTarget;
use zgui_scene::Scene;
use zgui_style::{ColorScheme, SheetHandle, SheetOrigin, SheetSource, StyleEngine, Viewport};
use zgui_view::{Anchor, BuildCxOwned, Dom, DomHandle, HostHandle, IntoView, View};
use zgui_view_dom::DocumentDom;

use crate::app::{
    CustomFactory, MetricsFactory, OffscreenRendererFactory, RasterFactory, TextFactory,
};
use crate::documents::Documents;
use crate::error::AppError;
use crate::host::{Command, RuntimeHost};
use crate::timer::Timers;
use crate::wake::{FrameGate, RuntimeWaker};
use crate::window::ATLAS_SOFT_BYTES;
use crate::window::brushes::{self, TextSlots};
use crate::windows::WindowHandle;

/// Default maximum logical width and height for measuring a snapshot view, in CSS pixels.
const DEFAULT_MAX_EXTENT: f32 = 512.0;

/// Maximum logical extent permitted for a single snapshot axis, in CSS pixels.
const HARD_MAX_EXTENT: f32 = 4096.0;

/// Final override sheet keeping the offscreen document root transparent and shrink-friendly so
/// application `:root { background: ... }` rules do not paint an opaque box behind rounded cards
/// or soft shadows.
const SNAPSHOT_ROOT_CSS: &str = "\
:root {\
    display: flex !important;\
    flex-direction: column !important;\
    align-items: flex-start !important;\
    justify-content: flex-start !important;\
    background: transparent !important;\
    background-color: transparent !important;\
    border: none !important;\
    box-shadow: none !important;\
    margin: 0px !important;\
    padding: 0px !important;\
    overflow: visible !important;\
}";

/// Configuration for offscreen rendering of a view through [`WindowHandle::render_view`].
///
/// The view is measured inside [`Self::max_size`], wrapped in transparent [`Self::padding`] (to
/// preserve `box-shadow` or outlines painted outside the element's border box), and rasterised
/// at [`Self::scale`] (or the window's current HiDPI scale when `None`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewSnapshotOptions {
    /// Maximum logical extent available while measuring the view, in CSS pixels.
    pub max_size: Size<CssPx, Css>,
    /// Transparent margin added around the view's border box, in CSS pixels.
    pub padding: Edges<CssPx>,
    /// Optional integer buffer scale override (clamped to `1..=4`).
    ///
    /// When `None`, defaults to the source window's current scale rounded up to an integer.
    pub scale: Option<u32>,
}

impl Default for ViewSnapshotOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl ViewSnapshotOptions {
    /// Creates default snapshot options (`512 × 512` max measure viewport, zero padding, window
    /// scale).
    #[must_use]
    pub const fn new() -> Self {
        Self {
            max_size: Size::new(CssPx(DEFAULT_MAX_EXTENT), CssPx(DEFAULT_MAX_EXTENT)),
            padding: Edges::new(CssPx(0.0), CssPx(0.0), CssPx(0.0), CssPx(0.0)),
            scale: None,
        }
    }

    /// Sets the maximum logical viewport size used while measuring the view, in CSS pixels.
    #[must_use]
    pub const fn with_max_size(mut self, width: f32, height: f32) -> Self {
        self.max_size = Size::new(CssPx(width), CssPx(height));
        self
    }

    /// Sets transparent margin around the view's layout box, in CSS pixels.
    ///
    /// Use this to reserve room for `box-shadow` or outlines so they are not clipped by the
    /// rendered bitmap edges.
    #[must_use]
    pub const fn with_padding(mut self, padding: Edges<CssPx>) -> Self {
        self.padding = padding;
        self
    }

    /// Overrides the integer HiDPI buffer scale factor (clamped to `1..=4`).
    #[must_use]
    pub const fn with_scale(mut self, scale: u32) -> Self {
        self.scale = Some(scale);
        self
    }
}

/// An offscreen-rendered view rasterised into straight-alpha `RGBA8` pixels.
#[derive(Clone, Debug)]
pub struct RenderedView {
    /// Rendered image in straight-alpha `RGBA8`, sized `logical_size * scale`.
    pub image: WindowIcon,
    /// Logical extent of [`Self::image`] (including snapshot padding), in CSS pixels.
    pub logical_size: Size<CssPx, Css>,
    /// Offset of the rendered view's own border box inside [`Self::image`], in CSS pixels.
    pub content_origin: Point<CssPx, Css>,
    /// Integer buffer scale factor used to rasterise [`Self::image`].
    pub scale: u32,
}

impl RenderedView {
    /// Converts this rendered view into a [`DragIcon`] whose `hotspot` is expressed relative to
    /// the view's own content origin (automatically compensating for snapshot padding).
    #[must_use]
    pub fn into_drag_icon(self, hotspot: Point<CssPx, Css>) -> DragIcon {
        DragIcon::new(self.image)
            .with_scale(self.scale)
            .with_hotspot(Point::new(
                CssPx(hotspot.x.0 + self.content_origin.x.0),
                CssPx(hotspot.y.0 + self.content_origin.y.0),
            ))
    }
}

/// Mirror of a window's installed stylesheets and active colour scheme, read by offscreen
/// snapshots without borrowing the live [`crate::Window`].
#[derive(Clone, Debug)]
pub(crate) struct SheetSnapshot {
    /// Static application and window stylesheets, in cascade order.
    pub(crate) base: Vec<String>,
    /// Named stylesheets installed dynamically by views, in first-insertion order.
    pub(crate) named: Vec<(String, String)>,
    /// Active `prefers-color-scheme` matched by the window's style engine.
    pub(crate) scheme: ColorScheme,
}

impl Default for SheetSnapshot {
    fn default() -> Self {
        Self {
            base: Vec::new(),
            named: Vec::new(),
            scheme: ColorScheme::Light,
        }
    }
}

impl SheetSnapshot {
    /// Records the static base stylesheets installed when the window opened.
    pub(crate) fn seed_base(&mut self, sheets: impl IntoIterator<Item = String>) {
        self.base = sheets.into_iter().collect();
    }

    /// Records a view-installed stylesheet, replacing in place when `name` already exists so
    /// cascade ordering matches [`crate::Window::install_view_sheet`].
    pub(crate) fn install_named(&mut self, name: &str, css: &str) {
        if let Some((_, existing)) = self.named.iter_mut().find(|(key, _)| key == name) {
            existing.clear();
            existing.push_str(css);
        } else {
            self.named.push((name.to_owned(), css.to_owned()));
        }
    }

    /// Removes a view-installed stylesheet by `name`.
    pub(crate) fn remove_named(&mut self, name: &str) {
        self.named.retain(|(key, _)| key != name);
    }
}

/// Process-level factories shared between [`crate::Runtime`] and every [`WindowHandle`] so a
/// handle can render an offscreen snapshot even while its live window is mutably borrowed inside
/// an event handler.
pub(crate) struct SnapshotEnv {
    /// Document-identity allocator shared with the runtime so snapshot IDs are recycled cleanly.
    pub(crate) documents: Rc<RefCell<Documents>>,
    /// Builds an offscreen renderer when configured.
    pub(crate) offscreen_renderer: Option<Rc<RefCell<OffscreenRendererFactory>>>,
    /// Builds a text engine over the application's font collection.
    pub(crate) text: Rc<RefCell<TextFactory>>,
    /// Supplies font metrics to the offscreen style cascade.
    pub(crate) metrics: Rc<RefCell<MetricsFactory>>,
    /// Supplies the glyph rasteriser to the offscreen paint stage.
    pub(crate) raster: Rc<RefCell<RasterFactory>>,
    /// Builds custom-element layout and paint sources, when configured.
    pub(crate) custom: Option<Rc<RefCell<CustomFactory>>>,
    /// Shared clock installed once the platform resumes.
    pub(crate) clock: Rc<RefCell<Option<Arc<dyn Clock>>>>,
}

/// No-op wake target so building an offscreen snapshot never wakes the live window's frame loop.
struct NoopWaker;

impl Waker for NoopWaker {
    fn wake(&self, _reason: WakeReason) {}
}

/// Fallback clock used only if a snapshot is requested before a platform clock is installed.
struct FallbackClock {
    origin: web_time::Instant,
}

impl FallbackClock {
    fn new() -> Self {
        Self {
            origin: web_time::Instant::now(),
        }
    }
}

impl Clock for FallbackClock {
    fn now(&self) -> web_time::Instant {
        web_time::Instant::now()
    }

    fn origin(&self) -> web_time::Instant {
        self.origin
    }
}

/// Renders `view` into an offscreen RGBA bitmap inheriting `sheets` and `parent_owner`.
pub(crate) fn render_view_snapshot<V: IntoView>(
    env: &SnapshotEnv,
    handle: &WindowHandle,
    parent_owner: Option<&Owner>,
    sheets: &SheetSnapshot,
    window_scale: f32,
    options: ViewSnapshotOptions,
    view: impl FnOnce() -> V,
) -> Result<RenderedView, AppError> {
    let Some(offscreen_factory) = env.offscreen_renderer.as_ref() else {
        return Err(PlatformError::Unsupported(Unsupported).into());
    };

    let scale_u32 = resolve_scale(options.scale, window_scale);
    let scale_f = scale_u32 as f32;
    let max_w = sanitize_extent(options.max_size.width.0);
    let max_h = sanitize_extent(options.max_size.height.0);
    let pad_top = sanitize_pad(options.padding.top.0);
    let pad_right = sanitize_pad(options.padding.right.0);
    let pad_bottom = sanitize_pad(options.padding.bottom.0);
    let pad_left = sanitize_pad(options.padding.left.0);

    let document = Rc::new(RefCell::new(env.documents.borrow_mut().allocate()?));
    let result = render_into_document(
        env,
        offscreen_factory,
        handle,
        parent_owner,
        sheets,
        Rc::clone(&document),
        scale_u32,
        scale_f,
        (max_w, max_h),
        (pad_top, pad_right, pad_bottom, pad_left),
        view,
    );
    env.documents.borrow_mut().give_back(&document);
    result
}

#[allow(clippy::too_many_arguments)]
fn render_into_document<V: IntoView>(
    env: &SnapshotEnv,
    offscreen_factory: &Rc<RefCell<OffscreenRendererFactory>>,
    handle: &WindowHandle,
    parent_owner: Option<&Owner>,
    sheets: &SheetSnapshot,
    document: Rc<RefCell<Document>>,
    scale_u32: u32,
    scale_f: f32,
    max_extent: (f32, f32),
    padding: (f32, f32, f32, f32),
    view: impl FnOnce() -> V,
) -> Result<RenderedView, AppError> {
    let (max_w, max_h) = max_extent;
    let (pad_top, pad_right, pad_bottom, pad_left) = padding;

    let dom = Rc::new(DocumentDom::new(Rc::clone(&document)));
    let dom_handle = DomHandle::from_rc(Rc::clone(&dom) as Rc<dyn Dom>);
    let document_id = dom.document_id();
    let layout = Rc::new(RefCell::new(LayoutStore::new(
        document.borrow().store().document(),
    )));
    let scroll = Rc::new(RefCell::new(zgui_scroll::Scroller::new()));
    let timers = Rc::new(RefCell::new(Timers::new()));
    let gate = Arc::new(FrameGate::new());
    let waker = Arc::new(RuntimeWaker::new(Arc::new(NoopWaker), gate));
    let clock: Arc<dyn Clock> = env
        .clock
        .borrow()
        .clone()
        .unwrap_or_else(|| Arc::new(FallbackClock::new()));

    let metrics = (env.metrics.borrow_mut())();
    let mut text = (env.text.borrow_mut())();
    let raster = (env.raster.borrow_mut())();
    let (custom_layout, custom_paint) = match env.custom.as_ref() {
        Some(factory) => {
            let (l, p) = (factory.borrow_mut())(&document);
            (Some(l), Some(p))
        }
        None => (None, None),
    };

    let viewport = Viewport::new(CssPx(max_w), CssPx(max_h))
        .at_scale(scale_f)
        .in_scheme(sheets.scheme);
    let mut engine = {
        let borrowed = document.borrow();
        StyleEngine::new(&borrowed, metrics, viewport)
    };
    let mut held_sheets: Vec<SheetHandle> = Vec::new();
    let mut named_sheets: FxHashMap<String, SheetHandle> = FxHashMap::default();
    for css in &sheets.base {
        let borrowed = document.borrow();
        let (sheet, _) = engine.add_sheet(&borrowed, SheetOrigin::Author, SheetSource::Text(css));
        held_sheets.push(sheet);
    }
    for (name, css) in &sheets.named {
        let borrowed = document.borrow();
        let (sheet, _) = engine.add_sheet(&borrowed, SheetOrigin::Author, SheetSource::Text(css));
        named_sheets.insert(name.clone(), sheet);
    }

    // Build the snapshot view inside a temporary child reactive scope so it can read app/window
    // contexts (`Gds`, `Fonts`, `WindowHandle`) while any dynamic `install_stylesheet` targets
    // only this offscreen document and all reactive nodes are disposed before returning.
    let scope = match parent_owner {
        Some(owner) => owner.with(Mounted::new),
        None => Mounted::new(),
    };
    let host = scope.with(|| {
        Rc::new(RuntimeHost::new(
            document_id,
            Rc::clone(&document),
            Rc::clone(&layout),
            Rc::clone(&scroll),
            Rc::clone(&timers),
            Arc::clone(&waker),
            Arc::clone(&clock),
        ))
    });
    host.set_scale(scale_f);
    let host_handle = HostHandle::from_rc(Rc::clone(&host) as Rc<dyn zgui_view::ViewHost>);

    let cx = BuildCxOwned::new(
        dom_handle.clone(),
        host_handle.clone(),
        scope.owner().clone(),
        document_id,
    );
    let wrapper = dom_handle.create_element(ElementName::new("box"));
    let wrapper_style = format!(
        "display: flex; flex-direction: column; align-items: flex-start; \
         justify-content: flex-start; flex: 0 0 auto; box-sizing: border-box; \
         padding: {pad_top:.2}px {pad_right:.2}px {pad_bottom:.2}px {pad_left:.2}px; \
         background: transparent; overflow: visible;"
    );
    dom_handle.set_style_text(wrapper, Some(&wrapper_style));
    dom_handle.insert(dom.root_node(), wrapper, None);

    let mut built: Box<dyn Anchor> = scope.with(|| {
        zgui_view::provide_host(host_handle.clone());
        zgui_reactive::provide_local_context(handle.clone());
        zgui_reactive::provide_task_set();
        // A one-shot snapshot renders once and tears down immediately, so reading signals
        // directly while constructing the view is intentional rather than a lost subscription.
        let _zone = zgui_reactive::enter_non_reactive_zone();
        Box::new(view().into_view().build(&mut cx.cx()))
    });
    built.mount(&dom_handle, wrapper, None);

    // Apply any stylesheets registered by the view during construction, then enforce transparent
    // root geometry as the final cascade rule.
    for command in host.take_commands() {
        match command {
            Command::InstallStylesheet { name, css } => {
                let borrowed = document.borrow();
                if let Some(existing) = named_sheets.get(&name) {
                    let _ = engine.replace_sheet(&borrowed, existing, SheetSource::Text(&css));
                    engine.force_author_rules_dirty();
                } else {
                    let (sheet, _) =
                        engine.add_sheet(&borrowed, SheetOrigin::Author, SheetSource::Text(&css));
                    named_sheets.insert(name, sheet);
                }
            }
            Command::RemoveStylesheet { name } => {
                drop(named_sheets.remove(&name));
            }
            _ => {}
        }
    }
    {
        let borrowed = document.borrow();
        let (root_override, _) = engine.add_sheet(
            &borrowed,
            SheetOrigin::Author,
            SheetSource::Text(SNAPSHOT_ROOT_CSS),
        );
        held_sheets.push(root_override);
    }

    // 1. Style cascade & initial text brush publishing.
    {
        let mut borrowed = document.borrow_mut();
        let _ = engine.restyle(&mut borrowed, None);
    }
    let mut text_slots = TextSlots::default();
    let updates = engine.text_paint_updates();
    if !updates.is_empty() {
        brushes::apply(&mut text_slots, text.text_paints(), updates);
        brushes::publish(&text_slots, text.text_paints());
    }

    // 2. Box tree build & layout inside the measurement viewport.
    {
        let borrowed = document.borrow();
        let mut layout_mut = layout.borrow_mut();
        zgui_layout::boxtree::build(&mut layout_mut, &borrowed);
    }
    let device = DeviceStyle {
        scale: scale_f,
        ..DeviceStyle::default()
    };
    let measure_surface = Size::new(DevicePx(max_w * scale_f), DevicePx(max_h * scale_f));
    {
        let mut layout_mut = layout.borrow_mut();
        let mut tree = LayoutTree::new(&mut layout_mut, &mut text, device);
        if let Some(custom) = custom_layout.as_deref() {
            tree = tree.with_custom(custom);
        }
        let outcome = tree.relayout_root(zgui_layout::tree::viewport_of(measure_surface));
        if !outcome.had_a_root() {
            cleanup_view(built, &dom_handle, scope);
            return Err(
                PlatformError::Backend("snapshot view produced no layout root".to_owned()).into(),
            );
        }
    }

    // 3. Measure the wrapper's laid-out extent to size the offscreen target tightly.
    let measured_css = {
        let layout_ref = layout.borrow();
        let wrapper_key = zgui_view_dom::id::to_document(wrapper).ok_or_else(|| {
            PlatformError::Backend("snapshot wrapper disappeared before layout".to_owned())
        })?;
        let first_box = layout_ref.boxes_of(wrapper_key).first().copied();
        first_box
            .and_then(|box_key| layout_ref.layout_of(box_key))
            .map(|resolved| {
                let rect = resolved.border_box();
                (rect.size.width.0 / scale_f, rect.size.height.0 / scale_f)
            })
    };
    let Some((measured_w, measured_h)) = measured_css else {
        cleanup_view(built, &dom_handle, scope);
        return Err(
            PlatformError::Backend("snapshot view did not lay out any box".to_owned()).into(),
        );
    };
    if !measured_w.is_finite() || !measured_h.is_finite() || measured_w <= 0.0 || measured_h <= 0.0
    {
        cleanup_view(built, &dom_handle, scope);
        return Err(
            PlatformError::Backend("snapshot view laid out to an empty extent".to_owned()).into(),
        );
    }

    let logical_w = measured_w.ceil().clamp(1.0, max_w) as u32;
    let logical_h = measured_h.ceil().clamp(1.0, max_h) as u32;
    let Some(pixel_w) = logical_w.checked_mul(scale_u32) else {
        cleanup_view(built, &dom_handle, scope);
        return Err(PlatformError::Backend("snapshot width overflowed u32".to_owned()).into());
    };
    let Some(pixel_h) = logical_h.checked_mul(scale_u32) else {
        cleanup_view(built, &dom_handle, scope);
        return Err(PlatformError::Backend("snapshot height overflowed u32".to_owned()).into());
    };
    let viewport_dev = Size::new(pixel_w as i32, pixel_h as i32);

    // 4. Build fragment tree & publish spatial placements + text brushes.
    let mut scene = Scene::new();
    let mut hit = HitIndex::new();
    let mut diff_scratch = zgui_layout::fragment::diff::DiffScratch::default();
    let animator = zgui_anim::Animator::new();
    let mut damage = DamageSet::full();
    {
        let mut layout_mut = layout.borrow_mut();
        let Some(root_box) = layout_mut.root() else {
            drop(layout_mut);
            cleanup_view(built, &dom_handle, scope);
            return Err(
                PlatformError::Backend("snapshot layout lost its root box".to_owned()).into(),
            );
        };
        let mut doc_mut = document.borrow_mut();
        let scroll_ref = scroll.borrow();
        let drawn = host.placements();
        let mut tables = zgui_layout::fragment::build::Tables {
            clips: &mut scene.clips,
            spatial: &mut scene.spatial,
            drawn: &drawn,
            device,
            scroll: scroll_ref.composed(),
            placements: animator.placements(),
        };
        let mut marks = zgui_layout::fragment::diff::DocumentMarks::for_document(&mut doc_mut);
        damage.set_bound(Some(Rect::new(Point::new(0, 0), viewport_dev)));
        let _ = zgui_layout::fragment::diff::rebuild_in(
            &mut diff_scratch,
            &mut layout_mut,
            &mut hit,
            &mut tables,
            &mut marks,
            root_box,
            &mut damage,
        );
        drop(drawn);
        host.publish_placements(&scene.spatial, None);
    }
    scene.text_paints = text.text_paints().clone();

    // 5. Create an exact-sized translucent offscreen renderer, emit paint, draw & read back.
    let target = RenderTarget::new(viewport_dev, Scale::new(scale_f)).translucent();
    let mut renderer = match (offscreen_factory.borrow_mut())(target) {
        Ok(renderer) => renderer,
        Err(error) => {
            cleanup_view(built, &dom_handle, scope);
            return Err(error);
        }
    };

    let atlas_limits = zgui_atlas::AtlasLimits {
        max_texture_size: renderer.capabilities().max_texture_size,
        ..zgui_atlas::AtlasLimits::default()
    };
    let mut content_cache = ContentCache::new(atlas_limits.with_soft_bytes(ATLAS_SOFT_BYTES));
    content_cache.set_vector_raster_ready(renderer.vector_status().initialized);
    content_cache.begin_frame();
    let vectors = zgui_paint::VectorCache::new();
    let mut painter = Painter::new();
    let mut capabilities = renderer.capabilities();
    // Translucent targets cannot use per-channel LCD subpixel blending.
    capabilities.subpixel_text = false;

    let mut draw_damage = DamageSet::full();
    draw_damage.clip_to(Rect::new(Point::new(0, 0), viewport_dev));
    scene.begin_frame(viewport_dev);
    {
        let layout_ref = layout.borrow();
        let placements = host.placements();
        let content_frame = content_cache.frame(&layout_ref, &text, raster.as_ref());
        let doc_ref = document.borrow();
        let vector_frame = vectors.frame(&doc_ref);
        let input = PaintInput {
            scale: scale_f,
            capabilities,
            glyphs: &content_frame,
            glyph_placements: &content_frame,
            placements: Some(&placements),
            replaced: &content_frame,
            vectors: &vector_frame,
            vector_masks: &content_frame,
            custom: custom_paint
                .as_deref()
                .unwrap_or(&zgui_paint::content::custom::NoCustom),
            shaders: &zgui_paint::content::shader::DeclaredShaders::new(
                capabilities.custom_shaders,
            ),
            pointer: None,
            resources: &content_frame,
            verify_replays: false,
            opaque_surface: false,
            anim: &*doc_ref,
            scrollbars: zgui_paint::emit::scrollbar::paint_for(sheets.scheme == ColorScheme::Dark),
            surface: Some(Rect::new(Point::new(0, 0), viewport_dev)),
            ..PaintInput::new(&layout_ref, &draw_damage)
        };
        let _ = painter.emit(&input, &mut scene);
    }

    scene.resolve_resources(content_cache.registry());
    scene.finish(&draw_damage);
    if let Err(error) = content_cache.flush(renderer.texture_sink()) {
        tracing::warn!(target: "zgui::paint", %error, "offscreen snapshot atlas upload refused");
    }
    let _ = renderer.draw(&scene, &draw_damage);
    let readback = renderer.read_rgba();

    // Tear down reactive scope and view nodes cleanly before returning.
    drop(named_sheets);
    drop(held_sheets);
    cleanup_view(built, &dom_handle, scope);

    let Some(pixels) = readback else {
        return Err(PlatformError::Unsupported(Unsupported).into());
    };
    let read_w = u32::try_from(pixels.size.width).unwrap_or(0);
    let read_h = u32::try_from(pixels.size.height).unwrap_or(0);
    let rgba = if pixels.premultiplied {
        demultiply_rgba(pixels.bytes)
    } else {
        pixels.bytes
    };
    let image = WindowIcon::from_rgba(rgba, read_w, read_h)
        .map_err(|error| PlatformError::Backend(format!("invalid snapshot bitmap: {error}")))?;
    Ok(RenderedView {
        image,
        logical_size: Size::new(CssPx(logical_w as f32), CssPx(logical_h as f32)),
        content_origin: Point::new(CssPx(pad_left), CssPx(pad_top)),
        scale: scale_u32,
    })
}

fn cleanup_view(mut built: Box<dyn Anchor>, dom_handle: &DomHandle, scope: Mounted) {
    built.unmount(dom_handle);
    drop(built);
    scope.unmount();
}

fn resolve_scale(override_scale: Option<u32>, window_scale: f32) -> u32 {
    if let Some(scale) = override_scale {
        return scale.clamp(1, 4);
    }
    if window_scale.is_finite() {
        window_scale.ceil().clamp(1.0, 4.0) as u32
    } else {
        1
    }
}

fn sanitize_extent(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(1.0, HARD_MAX_EXTENT).ceil()
    } else {
        DEFAULT_MAX_EXTENT
    }
}

fn sanitize_pad(value: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 256.0).ceil()
    } else {
        0.0
    }
}

/// Converts premultiplied gamma-encoded `RGBA8` pixels into straight-alpha `RGBA8` expected by
/// [`WindowIcon`].
fn demultiply_rgba(mut bytes: Vec<u8>) -> Vec<u8> {
    for px in bytes.as_chunks_mut::<4>().0 {
        let a = px[3];
        if a == 0 {
            px[0] = 0;
            px[1] = 0;
            px[2] = 0;
        } else if a < 255 {
            let alpha = u32::from(a);
            let half = alpha / 2;
            px[0] = ((u32::from(px[0]) * 255 + half) / alpha).min(255) as u8;
            px[1] = ((u32::from(px[1]) * 255 + half) / alpha).min(255) as u8;
            px[2] = ((u32::from(px[2]) * 255 + half) / alpha).min(255) as u8;
        }
    }
    bytes
}
