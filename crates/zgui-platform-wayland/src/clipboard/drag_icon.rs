//! Wayland `wl_surface` drag-icon upload, premultiplication, scaling, and cleanup.

use smithay_client_toolkit::compositor::CompositorState;
use smithay_client_toolkit::reexports::client::protocol::wl_shm::{self, WlShm};
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Proxy, QueueHandle};
use smithay_client_toolkit::shm::Shm;
use smithay_client_toolkit::shm::slot::{Buffer, SlotPool};
use wayland_protocols::wp::viewporter::client::wp_viewport::WpViewport;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use zgui_geom::{Css, CssPx, Point};
use zgui_platform::{DragIcon, PlatformError};

use crate::driver::WaylandState;

/// How a drag icon's buffer maps into logical surface coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScalePlan {
    /// Display the buffer at 1:1 buffer-to-logical pixels.
    Unscaled,
    /// Use core `wl_surface.set_buffer_scale(scale)` (buffer dimensions evenly divisible).
    Integer(i32),
    /// Use `wp_viewport.set_destination(width, height)` for non-divisible scaled buffers.
    Viewport {
        /// Logical surface width in CSS pixels.
        width: i32,
        /// Logical surface height in CSS pixels.
        height: i32,
    },
}

/// Owned Wayland drag-icon surface and backing shared-memory buffer.
///
/// Kept alive inside the active outgoing drag session until `dnd_finished`, `cancelled`, or
/// replacement, then torn down in strict protocol order (`wp_viewport` before `wl_surface`).
pub(crate) struct OutgoingDragIcon {
    surface: WlSurface,
    viewport: Option<WpViewport>,
    buffer: Buffer,
    _pool: SlotPool,
    width: i32,
    height: i32,
    offset: (i32, i32),
    scale: ScalePlan,
}

impl core::fmt::Debug for OutgoingDragIcon {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("OutgoingDragIcon")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("offset", &self.offset)
            .field("scale", &self.scale)
            .finish_non_exhaustive()
    }
}

impl Drop for OutgoingDragIcon {
    fn drop(&mut self) {
        // `wp_viewport` must be destroyed before the `wl_surface` it modifies.
        if let Some(viewport) = self.viewport.take() {
            viewport.destroy();
        }
        self.surface.destroy();
    }
}

impl OutgoingDragIcon {
    /// Allocates and fills the Wayland surface and SHM buffer for `icon`, ready to pass to
    /// `wl_data_device.start_drag` and commit immediately afterwards.
    ///
    /// Returns `Ok(None)` when `icon` has zero width or zero height (Wayland forbids 0-sized SHM
    /// buffers).
    pub(crate) fn prepare(
        compositor: &CompositorState,
        shm: &WlShm,
        viewporter: Option<&WpViewporter>,
        qh: &QueueHandle<WaylandState>,
        icon: &DragIcon,
    ) -> Result<Option<Self>, PlatformError> {
        let raw_w = icon.image.width();
        let raw_h = icon.image.height();
        if raw_w == 0 || raw_h == 0 {
            return Ok(None);
        }

        let width = i32::try_from(raw_w)
            .map_err(|_| PlatformError::Backend("drag icon width exceeds i32::MAX".to_owned()))?;
        let height = i32::try_from(raw_h)
            .map_err(|_| PlatformError::Backend("drag icon height exceeds i32::MAX".to_owned()))?;
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| PlatformError::Backend("drag icon stride overflowed".to_owned()))?;
        let byte_len = usize::try_from(stride)
            .ok()
            .and_then(|s| s.checked_mul(usize::try_from(height).ok()?))
            .ok_or_else(|| PlatformError::Backend("drag icon buffer size overflowed".to_owned()))?;
        if icon.image.rgba().len() != byte_len {
            return Err(PlatformError::Backend(
                "drag icon pixel buffer length did not match dimensions".to_owned(),
            ));
        }

        let shm_wrapper = Shm::from(shm.clone());
        let mut pool = SlotPool::new(byte_len, &shm_wrapper)
            .map_err(|error| PlatformError::Backend(format!("drag icon shm pool: {error}")))?;
        let (buffer, canvas) = pool
            .create_buffer(width, height, stride, wl_shm::Format::Argb8888)
            .map_err(|error| PlatformError::Backend(format!("drag icon shm buffer: {error}")))?;
        straight_rgba_to_premultiplied_argb8888(icon.image.rgba(), &mut canvas[..byte_len]);

        let surface = compositor.create_surface(qh);
        // Empty input region ensures the drag preview surface never steals pointer events.
        let region = compositor.wl_compositor().create_region(qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();

        let scale = plan_scale(
            width,
            height,
            icon.scale,
            surface.version(),
            viewporter.is_some(),
        );
        let viewport = match scale {
            ScalePlan::Viewport { .. } => viewporter.map(|vp| vp.get_viewport(&surface, qh, ())),
            ScalePlan::Unscaled | ScalePlan::Integer(_) => None,
        };

        Ok(Some(Self {
            surface,
            viewport,
            buffer,
            _pool: pool,
            width,
            height,
            offset: hotspot_offset(icon.hotspot),
            scale,
        }))
    }

    /// The underlying `wl_surface` to pass into `wl_data_device.start_drag`.
    pub(crate) const fn wl_surface(&self) -> &WlSurface {
        &self.surface
    }

    /// Attaches the buffer, applies scaling and hotspot offset, damages, and commits the surface.
    ///
    /// Called immediately after `wl_data_device.start_drag` so the compositor has already assigned
    /// the drag-icon role before processing the commit and surface offset.
    pub(crate) fn commit(&self) {
        match self.scale {
            ScalePlan::Unscaled => {}
            ScalePlan::Integer(scale) => {
                self.surface.set_buffer_scale(scale);
            }
            ScalePlan::Viewport { width, height } => {
                if let Some(viewport) = &self.viewport {
                    viewport.set_destination(width, height);
                }
            }
        }

        let (dx, dy) = self.offset;
        if self.surface.version() >= 5 {
            let _ = self.buffer.attach_to(&self.surface);
            if dx != 0 || dy != 0 {
                self.surface.offset(dx, dy);
            }
        } else {
            let _ = self.buffer.activate();
            self.surface.attach(Some(self.buffer.wl_buffer()), dx, dy);
        }

        if self.surface.version() >= 4 {
            self.surface.damage_buffer(0, 0, self.width, self.height);
        } else {
            self.surface.damage(0, 0, self.width, self.height);
        }
        self.surface.commit();
    }
}

/// Converts straight (non-premultiplied) RGBA8888 pixels into Wayland premultiplied `Argb8888`.
pub(crate) fn straight_rgba_to_premultiplied_argb8888(rgba: &[u8], dst: &mut [u8]) {
    for (src, out) in rgba.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        let r = u32::from(src[0]);
        let g = u32::from(src[1]);
        let b = u32::from(src[2]);
        let a = u32::from(src[3]);
        let (rp, gp, bp) = if a == 255 {
            (r, g, b)
        } else if a == 0 {
            (0, 0, 0)
        } else {
            (
                (r * a + 127) / 255,
                (g * a + 127) / 255,
                (b * a + 127) / 255,
            )
        };
        let pixel = (a << 24) | (rp << 16) | (gp << 8) | bp;
        out.copy_from_slice(&pixel.to_ne_bytes());
    }
}

/// Converts a top-left-relative hotspot into the Wayland surface offset (`-hotspot`).
pub(crate) fn hotspot_offset(hotspot: Point<CssPx, Css>) -> (i32, i32) {
    let x = if hotspot.x.0.is_finite() {
        -(hotspot.x.0.round() as i32)
    } else {
        0
    };
    let y = if hotspot.y.0.is_finite() {
        -(hotspot.y.0.round() as i32)
    } else {
        0
    };
    (x, y)
}

/// Selects a protocol-valid scaling strategy so `wl_surface.set_buffer_scale` is never called with
/// non-divisible buffer dimensions (which would trigger a Wayland `INVALID_SIZE` protocol error).
pub(crate) fn plan_scale(
    width: i32,
    height: i32,
    scale: u32,
    surface_version: u32,
    has_viewporter: bool,
) -> ScalePlan {
    let scale = i32::try_from(scale.max(1)).unwrap_or(1);
    if scale <= 1 {
        return ScalePlan::Unscaled;
    }
    if surface_version >= 3 && width % scale == 0 && height % scale == 0 {
        return ScalePlan::Integer(scale);
    }
    if has_viewporter {
        let logical_w = (f64::from(width) / f64::from(scale)).round().max(1.0) as i32;
        let logical_h = (f64::from(height) / f64::from(scale)).round().max(1.0) as i32;
        return ScalePlan::Viewport {
            width: logical_w,
            height: logical_h,
        };
    }
    ScalePlan::Unscaled
}

#[cfg(test)]
mod tests {
    use super::{ScalePlan, hotspot_offset, plan_scale, straight_rgba_to_premultiplied_argb8888};
    use zgui_geom::{CssPx, Point};

    #[test]
    fn straight_rgba_premultiplies_and_packs_argb8888() {
        let src = [
            255, 0, 0, 255, // opaque red
            255, 128, 64, 0, // fully transparent
            200, 100, 50, 128, // ~50% translucent
        ];
        let mut dst = [0_u8; 12];
        straight_rgba_to_premultiplied_argb8888(&src, &mut dst);

        let p0 = u32::from_ne_bytes(dst[0..4].try_into().expect("4 bytes"));
        let p1 = u32::from_ne_bytes(dst[4..8].try_into().expect("4 bytes"));
        let p2 = u32::from_ne_bytes(dst[8..12].try_into().expect("4 bytes"));

        assert_eq!(p0, 0xFFFF_0000);
        assert_eq!(p1, 0x0000_0000);
        // (200 * 128 + 127) / 255 = 100, (100 * 128 + 127) / 255 = 50, (50 * 128 + 127) / 255 = 25
        assert_eq!(p2, (128 << 24) | (100 << 16) | (50 << 8) | 25);
    }

    #[test]
    fn hotspot_offset_negates_and_sanitizes_non_finite_values() {
        assert_eq!(
            hotspot_offset(Point::new(CssPx(12.4), CssPx(7.6))),
            (-12, -8)
        );
        assert_eq!(
            hotspot_offset(Point::new(CssPx(f32::NAN), CssPx(f32::INFINITY))),
            (0, 0)
        );
    }

    #[test]
    fn scale_plan_avoids_invalid_buffer_scale_protocol_errors() {
        assert_eq!(plan_scale(32, 32, 1, 6, true), ScalePlan::Unscaled);
        assert_eq!(plan_scale(32, 32, 2, 6, true), ScalePlan::Integer(2));
        // Odd size at 2x must use viewport when available rather than invalid set_buffer_scale.
        assert_eq!(
            plan_scale(31, 19, 2, 6, true),
            ScalePlan::Viewport {
                width: 16,
                height: 10
            }
        );
        // Without viewporter, degrade to unscaled instead of crashing the Wayland connection.
        assert_eq!(plan_scale(31, 19, 2, 6, false), ScalePlan::Unscaled);
        // Old wl_surface < v3 without viewporter stays unscaled.
        assert_eq!(plan_scale(32, 32, 2, 2, false), ScalePlan::Unscaled);
    }
}
