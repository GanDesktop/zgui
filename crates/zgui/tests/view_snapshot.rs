//! Real-GPU offscreen view snapshot test (`WindowHandle::render_view` -> `WgpuRenderer::read_rgba`).

use std::cell::RefCell;
use std::rc::Rc;

use zgui::geom::{CssPx, Edges, Point};
use zgui::platform::{AppHandler, PlatformError, SurfaceEvent};
use zgui::prelude::*;
use zgui::render::{RenderTarget, Renderer};
use zgui_platform_headless::Harness;
use zgui_render_wgpu::{SharedGraphics, wgpu};

#[test]
fn render_view_on_real_wgpu_offscreen_produces_transparent_corners_and_opaque_card_pixels() {
    let graphics = SharedGraphics::new();
    // Skip cleanly on headless CI machines without any Vulkan/GL/software GPU adapter.
    if graphics.open_gpu().is_err() {
        return;
    }

    let handle_slot: Rc<RefCell<Option<WindowHandle>>> = Rc::default();
    let handle_view = Rc::clone(&handle_slot);
    let handle_driver = Rc::clone(&handle_slot);

    let window_graphics = graphics.clone();
    let offscreen_graphics = graphics;

    let css = r#"
        :root {
            background: #ff00ff;
            font-family: sans-serif;
        }
        .snap-card {
            display: flex;
            flex-direction: column;
            width: 248px;
            box-sizing: border-box;
            padding: 8px 10px;
            gap: 4px;
            border-radius: 8px;
            border: 1px solid #44444d;
            background: #242428;
            color: #f2f2f5;
            box-shadow: 0 8px 24px rgba(0, 0, 0, 0.28);
        }
        .snap-title {
            display: block;
            width: 100%;
            min-width: 0px;
            overflow: hidden;
            text-overflow: ellipsis;
            white-space: nowrap;
            font-size: 14px;
            line-height: 20px;
        }
    "#;

    let driver = move |handler: Box<dyn AppHandler>| -> Result<(), PlatformError> {
        let mut harness = Harness::new(handler);
        harness.deliver_to_first(SurfaceEvent::Resized(zgui::geom::Size::new(
            zgui::geom::DevicePx(320.0),
            zgui::geom::DevicePx(200.0),
        )));
        harness.settle(6);

        let window = handle_driver
            .borrow()
            .clone()
            .expect("window handle captured");

        let rendered = window
            .render_view(
                ViewSnapshotOptions::new()
                    .with_padding(Edges::new(
                        CssPx(16.0),
                        CssPx(24.0),
                        CssPx(32.0),
                        CssPx(24.0),
                    ))
                    .with_scale(2),
                || {
                    view! {
                        column(class = "snap-card") {
                            text(class = "snap-title") {
                                "rongxiaogou-1.0-pc-super-long-name-overflow-ellipsis-test.zip"
                            }
                        }
                    }
                },
            )
            .expect("GPU offscreen view snapshot succeeded");

        assert_eq!(rendered.scale, 2);
        assert_eq!(rendered.logical_size.width.0, 296.0);
        assert!(
            rendered.logical_size.height.0 >= 76.0,
            "unexpected height {:?}",
            rendered.logical_size
        );
        assert_eq!(rendered.image.width(), 592);

        let w = rendered.image.width();
        let rgba = rendered.image.rgba();
        let pixel = |x: u32, y: u32| -> [u8; 4] {
            let idx = ((y * w + x) * 4) as usize;
            [rgba[idx], rgba[idx + 1], rgba[idx + 2], rgba[idx + 3]]
        };

        // Top-left corner of the shadow padding must be transparent, never the magenta `:root`
        // background (#ff00ff).
        let corner = pixel(0, 0);
        assert_eq!(corner[3], 0, "expected transparent corner, got {corner:?}");

        // Interior of the card (content_origin + (40, 18) CSS px, scaled 2x) must be opaque
        // dark panel (#242428 = 36, 36, 40) or bright glyph ink, never magenta or transparent.
        let inner = pixel((24 + 40) * 2, (16 + 18) * 2);
        assert_eq!(
            inner[3], 255,
            "expected opaque card interior, got {inner:?}"
        );
        assert!(
            !(inner[0] > 200 && inner[1] < 20 && inner[2] > 200),
            "magenta :root leaked into card: {inner:?}"
        );

        // Verify bright text pixels were actually rasterised inside the text band.
        let mut bright_text_pixels = 0usize;
        for y in ((16 + 8) * 2)..((16 + 28) * 2) {
            for x in ((24 + 10) * 2)..((24 + 238) * 2) {
                let px = pixel(x, y);
                if px[3] == 255 && px[0] > 120 && px[1] > 120 && px[2] > 120 {
                    bright_text_pixels += 1;
                }
            }
        }
        assert!(
            bright_text_pixels > 50,
            "expected rasterised text glyphs inside card, found {bright_text_pixels} bright pixels"
        );

        let icon = rendered.into_drag_icon(Point::new(CssPx(-12.0), CssPx(-12.0)));
        assert_eq!(icon.hotspot, Point::new(CssPx(12.0), CssPx(4.0)));

        harness.shut_down();
        Ok(())
    };

    zgui::app()
        .with_fonts(Fonts::system())
        .with_size(320.0, 200.0)
        .with_stylesheet(css)
        .with_renderer(Box::new(move |_, target: RenderTarget| {
            let mut r = window_graphics.renderer_offscreen(
                target,
                wgpu::TextureFormat::Rgba8Unorm,
                false,
            )?;
            zgui_render_vector_vello::attach(&mut r, target.size);
            Ok(Box::new(r) as Box<dyn Renderer>)
        }))
        .with_offscreen_renderer(Box::new(move |target: RenderTarget| {
            let mut r = offscreen_graphics.renderer_offscreen(
                target,
                wgpu::TextureFormat::Rgba8Unorm,
                false,
            )?;
            zgui_render_vector_vello::attach(&mut r, target.size);
            Ok(Box::new(r) as Box<dyn Renderer>)
        }))
        .run_on(driver, move || {
            *handle_view.borrow_mut() = Some(use_window());
            view! { column() }
        })
        .expect("real GPU view snapshot test ran");
}
