//! Offscreen view snapshots (`WindowHandle::render_view`) inherit window stylesheets, auto-size
//! with shadow padding, compensate drag-icon hotspots, demultiply premultiplied pixels, and
//! recycle document identities cleanly.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use zgui_atlas::MemorySink;
use zgui_bits::DamageSet;
use zgui_geom::{CssPx, Edges, Point};
use zgui_platform_headless::Harness;
use zgui_render::{
    ExternalTexture, FrameOutcome, FrameStats, MemoryReport, RenderCapabilities, RenderTarget,
    RenderedPixels, Renderer, TextureHandle,
};
use zgui_runtime::{App, ViewSnapshotOptions, WindowHandle, windows::use_window};
use zgui_scene::Scene;
use zgui_view::{BuildCx, IntoView, View, install_stylesheet};

struct RecordedFrame {
    target: RenderTarget,
    quad_count: usize,
}

struct ReadbackCapture {
    target: RenderTarget,
    atlas: MemorySink,
    recorded: Rc<RefCell<Vec<RecordedFrame>>>,
}

impl Renderer for ReadbackCapture {
    fn capabilities(&self) -> RenderCapabilities {
        RenderCapabilities::MINIMAL
    }

    fn configure(&mut self, target: RenderTarget) {
        self.target = target;
    }

    fn target(&self) -> Option<RenderTarget> {
        Some(self.target)
    }

    fn draw(&mut self, scene: &Scene, _damage: &DamageSet) -> FrameOutcome {
        self.recorded.borrow_mut().push(RecordedFrame {
            target: self.target,
            quad_count: scene.primitives.quads.len(),
        });
        FrameOutcome::Presented(FrameStats::default())
    }

    fn register_external(&mut self, _: ExternalTexture) -> TextureHandle {
        TextureHandle(0)
    }

    fn release_external(&mut self, _: TextureHandle) {}

    fn memory(&self) -> MemoryReport {
        MemoryReport::default()
    }

    fn texture_sink(&mut self) -> &mut dyn zgui_atlas::TextureSink {
        &mut self.atlas
    }

    fn read_rgba(&self) -> Option<RenderedPixels> {
        // Fill with a premultiplied semi-transparent pixel: straight (200, 100, 50, 128)
        // premultiplied is roughly (100, 50, 25, 128).
        let count = (self.target.size.width.max(0) * self.target.size.height.max(0)) as usize;
        let mut bytes = Vec::with_capacity(count * 4);
        for _ in 0..count {
            bytes.extend_from_slice(&[100, 50, 25, 128]);
        }
        Some(RenderedPixels {
            bytes,
            size: self.target.size,
            premultiplied: true,
        })
    }
}

#[test]
fn render_view_auto_sizes_inherits_sheets_compensates_hotspot_and_recycles_documents() {
    let handle_slot: Rc<RefCell<Option<WindowHandle>>> = Rc::default();
    let handle_in_view = Rc::clone(&handle_slot);
    let recorded: Rc<RefCell<Vec<RecordedFrame>>> = Rc::default();
    let recorded_offscreen = Rc::clone(&recorded);

    let css = "\
        :root { background: #101014; --card-w: 248px; }
        .card { width: var(--card-w); height: 42px; background: #22252b; border-radius: 8px; }
    ";

    let handler = App::new()
        .with_title("snapshot-test")
        .with_size(400.0, 300.0)
        .with_stylesheet(css)
        .with_renderer(Box::new(|_, target| {
            Ok(Box::new(ReadbackCapture {
                target,
                atlas: MemorySink::default(),
                recorded: Rc::default(),
            }))
        }))
        .with_offscreen_renderer(Box::new(move |target| {
            Ok(Box::new(ReadbackCapture {
                target,
                atlas: MemorySink::default(),
                recorded: Rc::clone(&recorded_offscreen),
            }))
        }))
        .into_handler(move |cx: &mut BuildCx<'_>| {
            *handle_in_view.borrow_mut() = Some(use_window());
            install_stylesheet("live-window-theme", ".live-pill { height: 56px; }");
            Box::new(zgui_elements::column().into_view().build(cx))
        })
        .expect("runtime installs");

    let mut harness = Harness::new(handler);
    harness.settle(4);

    let window = handle_slot
        .borrow()
        .clone()
        .expect("window handle captured");

    let options = ViewSnapshotOptions::new()
        .with_padding(Edges::new(
            CssPx(8.0),
            CssPx(12.0),
            CssPx(16.0),
            CssPx(12.0),
        ))
        .with_scale(2);

    let rendered = window
        .render_view(options, || {
            // Dynamically installed sheets inside the snapshot view must apply to the snapshot.
            install_stylesheet(
                "snapshot-only",
                ".snap-inner { background: #334455; border: 1px solid #556677; }",
            );
            zgui_elements::column()
                .class("card")
                .class("live-pill")
                .class("snap-inner")
        })
        .expect("offscreen snapshot succeeds");

    // Inherited `--card-w: 248px` + `live-window-theme` height `56px` + padding (12+12, 8+16).
    assert_eq!(rendered.logical_size.width.0, 272.0);
    assert_eq!(rendered.logical_size.height.0, 80.0);
    assert_eq!(rendered.content_origin, Point::new(CssPx(12.0), CssPx(8.0)));
    assert_eq!(rendered.scale, 2);
    assert_eq!(rendered.image.width(), 544);
    assert_eq!(rendered.image.height(), 160);

    // Premultiplied (100, 50, 25, 128) demultiplies cleanly back to straight alpha.
    assert_eq!(&rendered.image.rgba()[..4], &[199, 100, 50, 128]);

    let frame = recorded
        .borrow()
        .last()
        .map(|f| (f.target, f.quad_count))
        .expect("offscreen renderer drew a frame");
    assert!(
        !frame.0.opaque,
        "offscreen snapshot target must be translucent"
    );
    assert_eq!(frame.0.size.width, 544);
    assert_eq!(frame.0.size.height, 160);
    assert!(
        frame.1 > 0,
        "styled snapshot card emitted at least one quad"
    );

    // `into_drag_icon` compensates for `content_origin` automatically.
    let icon = rendered.into_drag_icon(Point::new(CssPx(-12.0), CssPx(-12.0)));
    assert_eq!(icon.scale, 2);
    assert_eq!(icon.hotspot, Point::new(CssPx(0.0), CssPx(-4.0)));

    // Recycling check: rendering more than `DOCUMENT_COUNT` snapshots must never exhaust IDs.
    for _ in 0..40 {
        let again = window
            .render_view(ViewSnapshotOptions::new().with_scale(1), || {
                zgui_elements::column().class("card")
            })
            .expect("document identity recycled cleanly");
        assert_eq!(again.image.width(), 248);
        assert_eq!(again.image.height(), 42);
    }

    harness.shut_down();
}
