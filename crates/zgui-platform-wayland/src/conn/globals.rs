//! The globals this backend binds itself, beside the ones the toolkit owns.

use smithay_client_toolkit::globals::GlobalData;
use smithay_client_toolkit::reexports::client::globals::GlobalList;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1;
use wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1;
use wayland_protocols::wp::viewporter::client::wp_viewporter::WpViewporter;
use wayland_protocols::xdg::dialog::v1::client::xdg_wm_dialog_v1::XdgWmDialogV1;
use wayland_protocols::xdg::foreign::zv2::client::zxdg_importer_v2::ZxdgImporterV2;

use crate::driver::WaylandState;

/// The optional protocols the toolkit has no wrapper for.
#[derive(Debug, Default)]
pub struct Extras {
    /// `wp_fractional_scale_manager_v1`: what scale the compositor wants for a surface.
    pub fractional_scale: Option<WpFractionalScaleManagerV1>,
    /// `wp_viewporter`: the mapping from a buffer's extent to the extent it appears at.
    pub viewporter: Option<WpViewporter>,
    /// `zxdg_importer_v2`: imports external toplevel handles so dialogs can parent to them.
    pub importer: Option<ZxdgImporterV2>,
    /// `xdg_wm_dialog_v1`: marks a toplevel as a modal dialog of its parent.
    pub wm_dialog: Option<XdgWmDialogV1>,
    /// `zwp_linux_dmabuf_v1` (v4..=5): reports the compositor's primary DRM device.
    pub dmabuf: Option<ZwpLinuxDmabufV1>,
}

impl Extras {
    /// Binds whichever of these the compositor advertised.
    ///
    /// A missing global is not an error. Every one here is an improvement on a path that already
    /// works, so a compositor without them gets whole-number scaling rather than no window.
    pub fn bind(globals: &GlobalList, qh: &QueueHandle<WaylandState>) -> Self {
        let fractional_scale = bind(globals, qh);
        let viewporter = bind(globals, qh);
        let importer = bind(globals, qh);
        let wm_dialog = bind(globals, qh);
        // Bind v4..=5 so get_default_feedback is available and main_device continues to emit.
        let dmabuf = globals.bind(qh, 4..=5, GlobalData).ok();
        if fractional_scale.is_some() != viewporter.is_some() {
            tracing::debug!(
                fractional_scale = fractional_scale.is_some(),
                viewporter = viewporter.is_some(),
                "fractional scaling needs both globals; falling back to whole-number scaling"
            );
        }
        Self {
            fractional_scale,
            viewporter,
            importer,
            wm_dialog,
            dmabuf,
        }
    }

    /// Whether this compositor can scale a surface by something other than a whole number.
    pub const fn scales_fractionally(&self) -> bool {
        self.fractional_scale.is_some() && self.viewporter.is_some()
    }
}

/// Binds one optional global at version one.
fn bind<I>(globals: &GlobalList, qh: &QueueHandle<WaylandState>) -> Option<I>
where
    I: wayland_client::Proxy + 'static,
    WaylandState: Dispatch<I, GlobalData>,
{
    globals.bind(qh, 1..=1, GlobalData).ok()
}

impl Dispatch<ZxdgImporterV2, GlobalData> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &ZxdgImporterV2,
        _event: <ZxdgImporterV2 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<XdgWmDialogV1, GlobalData> for WaylandState {
    fn event(
        _state: &mut Self,
        _proxy: &XdgWmDialogV1,
        _event: <XdgWmDialogV1 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}
