//! The device, the surface, and everything that decides which of them is used.

pub mod adapter;
#[cfg(target_arch = "wasm32")]
pub mod canvas;
pub mod device;
pub mod direct_vulkan;
pub(crate) mod extensions;
pub mod formats;

pub use direct_vulkan::CompositorGpuHint;
pub mod loader;
pub mod loss;
pub mod pipeline_cache;
pub mod surface;
pub mod timing;
