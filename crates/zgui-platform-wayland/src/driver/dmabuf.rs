//! Compositor DRM render-device identity via `zwp_linux_dmabuf_v1`.
//!
//! Binds `zwp_linux_dmabuf_v1` at v4..=5 (because `main_device` stops being emitted at v6+) and
//! reads the default feedback on the existing registry roundtrip so window creation can hand the
//! renderer a non-destructive hint identifying the compositor's GPU.

use std::path::Path;

use smithay_client_toolkit::globals::GlobalData;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_feedback_v1::{
    self, ZwpLinuxDmabufFeedbackV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::{
    self, ZwpLinuxDmabufV1,
};
use zgui_platform::CompositorGpuHint;

use crate::driver::WaylandState;

impl Dispatch<ZwpLinuxDmabufV1, GlobalData> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpLinuxDmabufV1,
        _event: zwp_linux_dmabuf_v1::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        // v4+ clients receive formats via zwp_linux_dmabuf_feedback_v1 instead of format/modifier.
    }
}

impl Dispatch<ZwpLinuxDmabufFeedbackV1, ()> for WaylandState {
    fn event(
        state: &mut Self,
        feedback: &ZwpLinuxDmabufFeedbackV1,
        event: zwp_linux_dmabuf_feedback_v1::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            zwp_linux_dmabuf_feedback_v1::Event::MainDevice { device } => {
                if let Some(dev) = parse_dev_t(&device) {
                    state.compositor_gpu_hint = Some(hint_from_dev_t(dev));
                }
            }
            zwp_linux_dmabuf_feedback_v1::Event::Done => {
                feedback.destroy();
            }
            _ => {}
        }
    }
}

/// Decodes a raw `dev_t` payload emitted by `zwp_linux_dmabuf_feedback_v1.main_device`.
fn parse_dev_t(bytes: &[u8]) -> Option<u64> {
    match bytes.len() {
        8 => Some(u64::from_ne_bytes(bytes.try_into().ok()?)),
        4 => Some(u64::from(u32::from_ne_bytes(bytes.try_into().ok()?))),
        _ => None,
    }
}

/// Splits a 64-bit Linux `dev_t` into `(major, minor)` per `<sys/sysmacros.h>`.
#[cfg(any(target_os = "linux", test))]
fn decode_dev_t(dev: u64) -> (u32, u32) {
    let major =
        (((dev & 0x0000_0000_000f_ff00) >> 8) | ((dev & 0xffff_f000_0000_0000) >> 32)) as u32;
    let minor = ((dev & 0x0000_0000_0000_00ff) | ((dev & 0x0000_0fff_fff0_0000) >> 12)) as u32;
    (major, minor)
}

#[cfg(any(target_os = "linux", test))]
fn parse_pci_hex(raw: &str) -> Option<u32> {
    let trimmed = raw.trim();
    let hex = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))?;
    u32::from_str_radix(hex, 16).ok()
}

fn hint_from_dev_t(dev: u64) -> CompositorGpuHint {
    #[cfg(target_os = "linux")]
    let (vendor_id, device_id) = read_sysfs_pci_ids(dev);
    #[cfg(not(target_os = "linux"))]
    let (vendor_id, device_id) = (None, None);

    CompositorGpuHint {
        drm_render_dev: Some(dev),
        vendor_id,
        device_id,
    }
}

#[cfg(target_os = "linux")]
fn read_sysfs_pci_ids(dev: u64) -> (Option<u32>, Option<u32>) {
    let (major, minor) = decode_dev_t(dev);
    let device_dir = Path::new("/sys/dev/char")
        .join(format!("{major}:{minor}"))
        .join("device");
    read_pci_ids_from_dir(&device_dir)
}

#[cfg(any(target_os = "linux", test))]
fn read_pci_ids_from_dir(device_dir: &Path) -> (Option<u32>, Option<u32>) {
    let vendor_id = std::fs::read_to_string(device_dir.join("vendor"))
        .ok()
        .and_then(|s| parse_pci_hex(&s));
    let device_id = std::fs::read_to_string(device_dir.join("device"))
        .ok()
        .and_then(|s| parse_pci_hex(&s));
    (vendor_id, device_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_native_8_and_4_byte_dev_t() {
        let dev64: u64 = 0x0000_0000_0000_e280;
        assert_eq!(parse_dev_t(&dev64.to_ne_bytes()), Some(dev64));
        let dev32: u32 = 0x0000_e280;
        assert_eq!(parse_dev_t(&dev32.to_ne_bytes()), Some(u64::from(dev32)));
        assert_eq!(parse_dev_t(&[]), None);
        assert_eq!(parse_dev_t(&[1, 2, 3]), None);
    }

    #[test]
    fn decodes_linux_dev_t_major_minor() {
        // 226:128 -> /dev/dri/renderD128
        assert_eq!(decode_dev_t(0xe280), (226, 128));
        // 226:256 -> extended minor bit in 0x100000
        assert_eq!(decode_dev_t(0x0010_e200), (226, 256));
        // 5000:300 -> extended major + extended minor
        assert_eq!(decode_dev_t(0x0000_1000_0013_882c), (5000, 300));
    }

    #[test]
    fn parses_sysfs_pci_hex_values() {
        assert_eq!(parse_pci_hex("0x8086\n"), Some(0x8086));
        assert_eq!(parse_pci_hex(" 0X1002 "), Some(0x1002));
        assert_eq!(parse_pci_hex("8086"), None);
        assert_eq!(parse_pci_hex("0xzzzz"), None);
    }
}
