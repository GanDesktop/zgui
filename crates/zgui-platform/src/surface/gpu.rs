//! The graphics-capable view of a surface.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle};

use crate::surface::Surface;

/// A surface a graphics API can draw into directly.
///
/// This is a separate trait, and the separation is the point. The framework's core describes what
/// to draw and never touches a native handle; a backend with no native handle at all — one that
/// renders offscreen, one that targets a document rather than a screen — simply does not implement
/// this, and nothing above notices. Only the renderer asks for it, through
/// [`Surface::gpu`](crate::Surface::gpu), and only to hand the handles to the graphics API.
///
/// The two handle traits are supertraits rather than methods so that a graphics API which already
/// accepts anything carrying them accepts this without an adapter.
pub trait GpuSurface: Surface + HasWindowHandle + HasDisplayHandle {}

impl<T> GpuSurface for T where T: Surface + HasWindowHandle + HasDisplayHandle {}

/// Compositor DRM render-device identity reported for a GPU surface.
///
/// Carried as an optional hint so the graphics backend can identify which GPU
/// the compositor itself uses without forcing unrelated GPUs out of runtime
/// suspension during instance initialization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct CompositorGpuHint {
    /// Kernel `dev_t` of the compositor's primary DRM device node, when known.
    pub drm_render_dev: Option<u64>,
    /// PCI vendor identifier of the compositor's GPU, when known.
    pub vendor_id: Option<u32>,
    /// PCI device identifier of the compositor's GPU, when known.
    pub device_id: Option<u32>,
}
