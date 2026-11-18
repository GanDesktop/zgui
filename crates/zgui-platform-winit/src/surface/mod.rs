//! One window: what is drawn into it, what it looks like, and who is listening to it.

pub(crate) mod a11y;
mod attributes;
mod chrome;
mod handles;

pub(crate) use crate::surface::attributes::window as window_attributes;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use accesskit::TreeUpdate;
use raw_window_handle::{HasWindowHandle as _, RawWindowHandle};
use winit::dpi::{LogicalPosition, LogicalSize};
use winit::window::{Window, WindowButtons};
use zgui_geom::{Css, CssPx, Device, DevicePx, Point, Size};
use zgui_platform::{
    ColorScheme, CursorStyle, DecorationSource, Decorations, FullscreenMode, GpuSurface,
    PlatformError, ResizeEdge, Surface, SurfaceId, TextInput, TitleButtons, Unsupported,
    WakeReason, Waker as _, WindowIcon, WindowLevel,
};

use crate::waker::ProxyWaker;

/// A window, seen as something to draw into and interact with.
///
/// It is shared and thread-safe because the one operation that has to work from anywhere is asking
/// for a frame: work finishing on a worker thread is one of the four things that can want one, and
/// making it hop to the main thread first would put a scheduling delay in front of every frame the
/// application ever draws.
///
/// That is also why the accessibility adapter is not in here. It cannot leave the thread that made
/// it, so it stays on that thread and this names it by the surface's own number.
///
/// # Two spaces, and they are not interchangeable
///
/// [`Surface::size`] answers in physical pixels because that is what a swap chain is allocated in,
/// and a rounding error there is a visibly stretched frame. Everything a *caller* asks for is in
/// CSS pixels because that is the space a stylesheet is written in. The bridge is this window's own
/// scale — never the monitor's, which can differ, and which is the difference that only shows up on
/// the mixed-density arrangements nobody has to hand.
pub struct WinitSurface {
    /// Which surface this is, in the contract's numbering.
    id: SurfaceId,
    /// The window itself.
    window: Arc<Window>,
    /// Where the window buttons sit, when the application placed them.
    title_buttons: Option<TitleButtons>,
    /// How a frame request reaches the loop when the platform does not deliver it.
    waker: Arc<ProxyWaker>,
    /// Whether the window was shown at least once.
    shown: AtomicBool,
    /// Whether the window asked for platform decorations.
    requested_decorated: AtomicBool,
    /// Whether the window has received at least one configure on Wayland (always `true` elsewhere).
    configured: AtomicBool,
    /// Last effective decoration state observed, used to detect configure-time SSD overrides.
    last_decorated: AtomicBool,
}

impl WinitSurface {
    /// A surface over `window`, numbered `id`, with nothing listening to it yet.
    pub(crate) fn new(
        id: SurfaceId,
        window: Arc<Window>,
        decorations: Decorations,
        title_buttons: Option<TitleButtons>,
        waker: Arc<ProxyWaker>,
    ) -> Self {
        let is_wayland = window
            .window_handle()
            .is_ok_and(|handle| matches!(handle.as_raw(), RawWindowHandle::Wayland(_)));
        let requested_decorated = chrome::decorated(decorations);
        let initial_decorated = if is_wayland {
            requested_decorated
        } else {
            window.is_decorated()
        };
        Self {
            id,
            window,
            title_buttons,
            waker,
            shown: AtomicBool::new(false),
            requested_decorated: AtomicBool::new(requested_decorated),
            configured: AtomicBool::new(!is_wayland),
            last_decorated: AtomicBool::new(initial_decorated),
        }
    }

    /// Marks the window configured and reports whether negotiated decorations flipped.
    pub(crate) fn note_configured(&self) -> bool {
        self.configured.store(true, Ordering::Release);
        let current = self.window.is_decorated();
        self.last_decorated.swap(current, Ordering::AcqRel) != current
    }

    /// Puts the window buttons where the application placed them.
    ///
    /// The desktop lays its title bar out again after a resize, a change of focus and a change of
    /// full screen, so the loop calls this after each.
    pub(crate) fn place_title_buttons(&self) {
        #[cfg(target_os = "macos")]
        if let Some(buttons) = self.title_buttons {
            crate::macos::place_title_buttons(&self.window, buttons);
        }
        // The other desktops draw their buttons in a frame of their own.
        #[cfg(not(target_os = "macos"))]
        let _ = self.title_buttons;
    }

    /// The window underneath, for the parts of the loop that have to name it.
    pub(crate) fn window(&self) -> &Arc<Window> {
        &self.window
    }

    /// The scale this window is presented at, never zero.
    ///
    /// A compositor is allowed to answer with nonsense while a window is being mapped, and every
    /// conversion out of physical pixels divides by this. One is the only answer that leaves the
    /// numbers finite.
    pub(crate) fn scale(&self) -> f64 {
        let scale = self.window.scale_factor();
        if scale > 0.0 { scale } else { 1.0 }
    }
}

impl core::fmt::Debug for WinitSurface {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("WinitSurface")
            .field("id", &self.id)
            .field("a11y", &a11y::is_attached(self.id))
            .finish_non_exhaustive()
    }
}

impl Surface for WinitSurface {
    fn id(&self) -> SurfaceId {
        self.id
    }

    fn size(&self) -> Size<DevicePx, Device> {
        let size = self.window.inner_size();
        Size::new(DevicePx(size.width as f32), DevicePx(size.height as f32))
    }

    fn scale_factor(&self) -> f64 {
        self.scale()
    }

    fn subpixel_layout(&self) -> zgui_platform::SubpixelLayout {
        crate::subpixel::layout()
    }

    fn refresh_rate_millihertz(&self) -> Option<u32> {
        self.window
            .current_monitor()
            .and_then(|monitor| monitor.refresh_rate_millihertz())
            .filter(|rate| *rate > 0)
    }

    fn request_size(&self, size: Size<CssPx, Css>) -> Option<Size<DevicePx, Device>> {
        self.window
            .request_inner_size(LogicalSize::new(size.width.0, size.height.0))
            .map(|taken| Size::new(DevicePx(taken.width as f32), DevicePx(taken.height as f32)))
    }

    fn set_min_size(&self, size: Option<Size<CssPx, Css>>) {
        self.window
            .set_min_inner_size(size.map(|size| LogicalSize::new(size.width.0, size.height.0)));
    }

    fn set_max_size(&self, size: Option<Size<CssPx, Css>>) {
        self.window
            .set_max_inner_size(size.map(|size| LogicalSize::new(size.width.0, size.height.0)));
    }

    fn request_redraw(&self) {
        // Windows paints only visible windows, and a surface stays hidden until its first frame
        // presents. Until the window is shown, the loop delivers the request itself. A window that
        // is hidden later waits for the paint that shows it again.
        if cfg!(windows) && !self.shown.load(Ordering::Relaxed) {
            self.waker.redraw(self.window.id());
        } else {
            self.window.request_redraw();
        }
    }

    fn pre_present_notify(&self) {
        self.window.pre_present_notify();
    }

    fn set_title(&self, title: &str) {
        self.window.set_title(title);
    }

    fn set_visible(&self, visible: bool) {
        if visible {
            self.shown.store(true, Ordering::Relaxed);
        }
        self.window.set_visible(visible);
    }

    fn set_decorations(&self, decorations: Decorations) {
        let decorated = chrome::decorated(decorations);
        self.requested_decorated.store(decorated, Ordering::Relaxed);
        self.window.set_decorations(decorated);
        let effective = if self.configured.load(Ordering::Acquire) {
            self.window.is_decorated()
        } else {
            decorated
        };
        if self.last_decorated.swap(effective, Ordering::AcqRel) != effective {
            self.waker.wake(WakeReason::SurfaceStateChanged(self.id));
        }
    }

    fn set_resizable(&self, resizable: bool) {
        self.window.set_resizable(resizable);
    }

    fn set_maximized(&self, maximized: bool) {
        self.window.set_maximized(maximized);
    }

    fn set_minimized(&self, minimized: bool) {
        self.window.set_minimized(minimized);
    }

    fn set_fullscreen(&self, mode: Option<FullscreenMode>) {
        self.window
            .set_fullscreen(mode.map(|mode| chrome::fullscreen(&self.window, mode)));
    }

    fn set_position(&self, position: Point<CssPx, Css>) {
        // Winit answers a compositor that places windows itself by doing nothing here, which is the
        // no-op this contract asks for; nothing extra is needed to make it silent.
        self.window
            .set_outer_position(LogicalPosition::new(position.x.0, position.y.0));
    }

    fn position(&self) -> Option<Point<CssPx, Css>> {
        let scale = self.window.scale_factor();
        self.window.outer_position().ok().map(|position| {
            let position: LogicalPosition<f64> = position.to_logical(scale);
            Point::new(CssPx(position.x as f32), CssPx(position.y as f32))
        })
    }

    fn is_maximized(&self) -> bool {
        self.window.is_maximized()
    }

    fn is_minimized(&self) -> Option<bool> {
        self.window.is_minimized()
    }

    fn fullscreen(&self) -> Option<FullscreenMode> {
        self.window.fullscreen().map(|mode| match mode {
            winit::window::Fullscreen::Exclusive(_) => FullscreenMode::Exclusive,
            winit::window::Fullscreen::Borderless(_) => FullscreenMode::Borderless,
        })
    }

    fn decoration_source(&self) -> DecorationSource {
        let decorated = if self.configured.load(Ordering::Acquire) {
            self.window.is_decorated()
        } else {
            self.requested_decorated.load(Ordering::Relaxed)
        };
        if decorated {
            DecorationSource::Platform
        } else {
            DecorationSource::Application
        }
    }

    fn can_minimize(&self) -> Option<bool> {
        Some(
            self.window
                .enabled_buttons()
                .contains(WindowButtons::MINIMIZE),
        )
    }

    fn can_maximize(&self) -> Option<bool> {
        Some(
            self.window
                .enabled_buttons()
                .contains(WindowButtons::MAXIMIZE),
        )
    }

    fn set_window_level(&self, level: WindowLevel) {
        self.window.set_window_level(chrome::level(level));
    }

    fn set_icon(&self, icon: Option<&WindowIcon>) {
        self.window.set_window_icon(icon.and_then(chrome::icon));
    }

    fn set_theme(&self, theme: Option<ColorScheme>) {
        self.window.set_theme(theme.map(crate::theme::theme));
    }

    fn focus(&self) {
        self.window.focus_window();
    }

    fn request_attention(&self, urgent: bool) {
        // Critical rather than informational, because the only caller is an application saying
        // something needs the user now; a desktop that draws only one kind of attention draws that.
        self.window
            .request_user_attention(urgent.then_some(winit::window::UserAttentionType::Critical));
    }

    fn set_external_parent(&self, parent: &str, modal: bool) -> Result<(), PlatformError> {
        set_external_parent_impl(&self.window, parent, modal)
    }

    fn activate_with_token(&self, token: &str) {
        if token.is_empty() {
            return;
        }
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        set_x11_startup_id(&self.window, token);
        self.window.focus_window();
    }

    fn show_window_menu(&self, position: Point<CssPx, Css>) -> Result<(), Unsupported> {
        let supported = self.window.window_handle().is_ok_and(|handle| {
            matches!(
                handle.as_raw(),
                RawWindowHandle::Wayland(_) | RawWindowHandle::Win32(_)
            )
        });
        if !supported {
            return Err(Unsupported);
        }
        self.window.show_window_menu(LogicalPosition::new(
            f64::from(position.x.0),
            f64::from(position.y.0),
        ));
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn title_bar_double_click(&self) {
        crate::macos::title_bar_double_click(&self.window);
    }

    fn begin_move_drag(&self) -> Result<(), Unsupported> {
        self.window.drag_window().map_err(|_| Unsupported)
    }

    fn begin_resize_drag(&self, edge: ResizeEdge) -> Result<(), Unsupported> {
        self.window
            .drag_resize_window(chrome::resize(edge))
            .map_err(|_| Unsupported)
    }

    fn set_cursor(&self, cursor: CursorStyle) {
        match chrome::cursor(cursor) {
            Some(icon) => {
                self.window.set_cursor(icon);
                self.window.set_cursor_visible(true);
            }
            None => self.window.set_cursor_visible(false),
        }
    }

    fn set_pointer_passthrough(&self, passthrough: bool) -> Result<(), Unsupported> {
        self.window
            .set_cursor_hittest(!passthrough)
            .map_err(|_| Unsupported)
    }

    fn set_text_input(&self, state: Option<TextInput>) {
        // The order matters in one direction only: an input method told where the caret is while
        // it believes the field is closed places its candidate window over the text being composed.
        // Setting the area first and the flag second is the order that cannot produce that.
        if let Some(state) = state {
            self.window.set_ime_cursor_area(
                winit::dpi::LogicalPosition::new(state.caret_origin.x.0, state.caret_origin.y.0),
                LogicalSize::new(state.caret_size.width.0, state.caret_size.height.0),
            );
            self.window
                .set_ime_purpose(crate::input::ime::purpose(state.purpose));
        }
        self.window.set_ime_allowed(state.is_some());
    }

    fn reset_dead_keys(&self) {
        self.window.reset_dead_keys();
    }

    /// Publishes through the adapter this window's loop holds for it.
    ///
    /// Nothing is published, and nothing is built, from any other thread: the adapters belong to
    /// the thread the loop runs on, and a caller elsewhere finds none. That is the same answer a
    /// machine with no assistive technology gives, and the frame's publishing phase — the only
    /// caller — runs on the loop's thread anyway.
    fn push_a11y_update(&self, build: &mut dyn FnMut() -> TreeUpdate) {
        a11y::publish(self.id, build);
    }

    fn gpu(&self) -> Option<&dyn GpuSurface> {
        Some(self)
    }

    fn gpu_shared(self: Arc<Self>) -> Option<Arc<dyn GpuSurface>> {
        Some(self)
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn parse_x11_parent(parent: &str) -> Result<u32, PlatformError> {
    let Some(raw) = parent.strip_prefix("x11:") else {
        if parent.starts_with("wayland:") {
            return Err(PlatformError::Unsupported(Unsupported));
        }
        return Err(PlatformError::Backend(format!(
            "unsupported external parent handle: {parent}"
        )));
    };
    let trimmed = raw.trim();
    let xid = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        u32::from_str_radix(hex, 16)
    } else {
        trimmed.parse::<u32>()
    }
    .map_err(|_| PlatformError::Backend(format!("invalid X11 parent XID: {parent}")))?;
    if xid == 0 {
        return Err(PlatformError::Backend(
            "X11 parent XID must be nonzero".to_owned(),
        ));
    }
    Ok(xid)
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn set_external_parent_impl(
    window: &Window,
    parent: &str,
    modal: bool,
) -> Result<(), PlatformError> {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{
        AtomEnum, CLIENT_MESSAGE_EVENT, ClientMessageEvent, ConnectionExt as _, EventMask, PropMode,
    };
    use x11rb::wrapper::ConnectionExt as _;

    let parent_xid = parse_x11_parent(parent)?;
    let child_xid = match window.window_handle() {
        Ok(handle) => match handle.as_raw() {
            RawWindowHandle::Xlib(xlib) => u32::try_from(xlib.window)
                .map_err(|_| PlatformError::Backend("Xlib window ID overflow".to_owned()))?,
            RawWindowHandle::Xcb(xcb) => xcb.window.get(),
            _ => return Err(PlatformError::Unsupported(Unsupported)),
        },
        Err(_) => return Err(PlatformError::Unsupported(Unsupported)),
    };

    let (conn, screen_num) = x11rb::rust_connection::RustConnection::connect(None)
        .map_err(|err| PlatformError::Backend(format!("x11 connect failed: {err}")))?;
    conn.change_property32(
        PropMode::REPLACE,
        child_xid,
        AtomEnum::WM_TRANSIENT_FOR,
        AtomEnum::WINDOW,
        &[parent_xid],
    )
    .map_err(|err| PlatformError::Backend(err.to_string()))?;

    let wm_state = conn
        .intern_atom(false, b"_NET_WM_STATE")
        .map_err(|err| PlatformError::Backend(err.to_string()))?
        .reply()
        .map_err(|err| PlatformError::Backend(err.to_string()))?
        .atom;
    let wm_state_modal = conn
        .intern_atom(false, b"_NET_WM_STATE_MODAL")
        .map_err(|err| PlatformError::Backend(err.to_string()))?
        .reply()
        .map_err(|err| PlatformError::Backend(err.to_string()))?
        .atom;

    let mut states: Vec<u32> = conn
        .get_property(false, child_xid, wm_state, AtomEnum::ATOM, 0, 64)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|v| v.collect()))
        .unwrap_or_default();
    if modal {
        if !states.contains(&wm_state_modal) {
            states.push(wm_state_modal);
        }
    } else {
        states.retain(|&atom| atom != wm_state_modal);
    }
    conn.change_property32(
        PropMode::REPLACE,
        child_xid,
        wm_state,
        AtomEnum::ATOM,
        &states,
    )
    .map_err(|err| PlatformError::Backend(err.to_string()))?;

    if let Some(screen) = conn.setup().roots.get(screen_num) {
        let msg = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: child_xid,
            type_: wm_state,
            data: [u32::from(modal), wm_state_modal, 0, 1, 0].into(),
        };
        let _ = conn.send_event(
            false,
            screen.root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            msg,
        );
    }
    conn.flush()
        .map_err(|err| PlatformError::Backend(err.to_string()))?;
    Ok(())
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn set_external_parent_impl(
    _window: &Window,
    _parent: &str,
    _modal: bool,
) -> Result<(), PlatformError> {
    Err(PlatformError::Unsupported(Unsupported))
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn set_x11_startup_id(window: &Window, token: &str) {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};
    use x11rb::wrapper::ConnectionExt as _;

    let Ok(handle) = window.window_handle() else {
        return;
    };
    let child_xid = match handle.as_raw() {
        RawWindowHandle::Xlib(xlib) => match u32::try_from(xlib.window) {
            Ok(id) => id,
            Err(_) => return,
        },
        RawWindowHandle::Xcb(xcb) => xcb.window.get(),
        _ => return,
    };
    let Ok((conn, _)) = x11rb::rust_connection::RustConnection::connect(None) else {
        return;
    };
    if let Ok(cookie) = conn.intern_atom(false, b"_NET_STARTUP_ID")
        && let Ok(reply) = cookie.reply()
        && let Ok(utf8_cookie) = conn.intern_atom(false, b"UTF8_STRING")
        && let Ok(utf8_reply) = utf8_cookie.reply()
    {
        let _ = conn.change_property8(
            PropMode::REPLACE,
            child_xid,
            reply.atom,
            if utf8_reply.atom != 0 {
                utf8_reply.atom
            } else {
                AtomEnum::STRING.into()
            },
            token.as_bytes(),
        );
        let _ = conn.flush();
    }
}

#[cfg(all(
    test,
    any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )
))]
mod tests {
    use super::parse_x11_parent;
    use zgui_platform::PlatformError;

    #[test]
    fn parses_decimal_and_hex_x11_parent_handles() {
        assert_eq!(parse_x11_parent("x11:12345"), Ok(12345));
        assert_eq!(parse_x11_parent("x11:0x1a2b"), Ok(0x1a2b));
        assert!(matches!(
            parse_x11_parent("wayland:abc"),
            Err(PlatformError::Unsupported(_))
        ));
        assert!(matches!(
            parse_x11_parent("x11:0"),
            Err(PlatformError::Backend(_))
        ));
    }
}
