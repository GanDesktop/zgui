//! The desktop's clipboards, and the two protocols they are reached over.
//!
//! There are two implementations behind one type and the choice is made once, at start-up, from
//! the connection the window system opened. The reason is written down here so that nobody
//! re-enables the configuration that breaks it:
//!
//! * **Under a Wayland compositor** the clipboard is reached over the application's own connection
//!   with the ordinary data-device protocol — the protocol a window is supposed to use. The
//!   desktop-wide library's Wayland support speaks the *data-control* protocol instead, which is
//!   what clipboard managers and screen recorders use: it needs a privileged interface, several
//!   compositors do not offer it at all, and where it is missing every copy silently fails. So it
//!   is compiled out and this arm is taken instead.
//! * **Everywhere else**, including X11, the desktop-wide library is exactly right and is used as
//!   it comes.
//!
//! # Why a read is a request and not a return value
//!
//! The content belongs to another process. That process has to be asked for it, may take as long
//! as it likes to answer, and may never answer at all. [`Clipboard::read`] therefore starts a read
//! and hands back an identifier, and the answer arrives later as a wake carrying that identifier —
//! a shape every desktop can satisfy. [`Clipboard::read_blocking`] is there because on a desktop
//! the answer usually *is* available at once, and threading it through the loop for that case is
//! ceremony.

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
mod desktop;
mod mime;
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
mod wayland;
#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
mod x11;

use std::cell::RefCell;
use std::sync::Arc;

use raw_window_handle::HasDisplayHandle;
use zgui_platform::{
    Clipboard, ClipboardData, ClipboardError, ClipboardFormat, ClipboardKind, ClipboardMime,
    ClipboardMimeOffer, ClipboardSerial, ClipboardSerials, ClipboardWriteOptions, WakeReason,
    Waker,
};

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
use crate::clipboard::desktop::Desktop;

/// Which implementation a running program ended up with.
enum Backend {
    /// The desktop-wide library on macOS and Windows.
    #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    Desktop(Desktop),
    /// Native X11 selection protocol over `x11rb`.
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    X11(Arc<x11::X11>),
    /// The compositor's own data device, over the connection the window system already owns.
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    Wayland(Arc<wayland::Wayland>),
}

/// The desktop's clipboards.
pub struct DesktopClipboard {
    /// Identifiers for reads answered through the loop.
    serials: ClipboardSerials,
    /// The implementation, once the connection has been seen.
    backend: RefCell<Backend>,
    /// Where the answer to a read is delivered.
    waker: RefCell<Option<Arc<dyn Waker>>>,
}

impl Default for DesktopClipboard {
    fn default() -> Self {
        Self::new()
    }
}

impl DesktopClipboard {
    /// A clipboard that has not yet seen which desktop it is on.
    pub fn new() -> Self {
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        let backend = Backend::X11(Arc::new(x11::X11::default()));
        #[cfg(not(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        let backend = Backend::Desktop(Desktop::default());
        Self {
            serials: ClipboardSerials::new(),
            backend: RefCell::new(backend),
            waker: RefCell::new(None),
        }
    }

    /// Chooses the implementation from the connection the window system opened, once.
    ///
    /// The waker is taken at the same time because a read started through the loop is answered
    /// through the loop, and the answer has to reach the same loop the request came from.
    pub(crate) fn attach(&self, display: &dyn HasDisplayHandle, waker: Arc<dyn Waker>) {
        *self.waker.borrow_mut() = Some(waker);
        self.choose(display);
    }

    /// Picks the Wayland implementation when the connection is a Wayland one.
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    fn choose(&self, display: &dyn HasDisplayHandle) {
        use raw_window_handle::RawDisplayHandle;

        let Ok(handle) = display.display_handle() else {
            return;
        };
        if let RawDisplayHandle::Wayland(wayland) = handle.as_raw() {
            // SAFETY: the pointer comes from the running event loop's own display handle, and the
            // event loop owns that connection for as long as the program runs. This clipboard is
            // stored beside the loop and dropped with it, so the connection outlives it.
            let clipboard = unsafe { wayland::Wayland::new(wayland.display.as_ptr()) };
            *self.backend.borrow_mut() = Backend::Wayland(Arc::new(clipboard));
            tracing::debug!(target: "zgui::platform", "the compositor's own data device is in use");
        }
    }

    /// Keeps the desktop-wide implementation, which is the only one on this platform.
    #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    fn choose(&self, display: &dyn HasDisplayHandle) {
        let _ = display;
    }
}

const fn is_format_supported(format: ClipboardFormat) -> bool {
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    {
        matches!(
            format,
            ClipboardFormat::Text | ClipboardFormat::FileList | ClipboardFormat::Image
        )
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    {
        matches!(format, ClipboardFormat::Text)
    }
}

impl core::fmt::Debug for DesktopClipboard {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("DesktopClipboard")
    }
}

impl Clipboard for DesktopClipboard {
    fn read(&self, kind: ClipboardKind, format: ClipboardFormat) -> ClipboardSerial {
        let serial = self.serials.take();
        let Some(waker) = self.waker.borrow().clone() else {
            tracing::warn!(
                target: "zgui::platform",
                "a clipboard read was started before the loop could answer it"
            );
            return serial;
        };
        if !is_format_supported(format) {
            waker.wake(WakeReason::ClipboardRead {
                serial,
                result: Err(ClipboardError::UnsupportedFormat(format)),
            });
            return serial;
        }
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        {
            enum Target {
                X11(Arc<x11::X11>),
                Wayland(Arc<wayland::Wayland>),
            }
            let target = match &*self.backend.borrow() {
                Backend::X11(x11) => Target::X11(Arc::clone(x11)),
                Backend::Wayland(wayland) => Target::Wayland(Arc::clone(wayland)),
            };
            let _ = std::thread::Builder::new()
                .name("zgui-clipboard".to_owned())
                .spawn(move || {
                    let result = match target {
                        Target::X11(x11) => x11.read(kind, format, x11::PATIENCE),
                        Target::Wayland(wayland) => wayland.read(kind, format, wayland::PATIENCE),
                    };
                    waker.wake(WakeReason::ClipboardRead { serial, result });
                });
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        {
            let result = match &mut *self.backend.borrow_mut() {
                Backend::Desktop(desktop) => desktop.read(kind, format),
            };
            waker.wake(WakeReason::ClipboardRead { serial, result });
        }
        serial
    }

    fn read_blocking(
        &self,
        kind: ClipboardKind,
        format: ClipboardFormat,
    ) -> Result<ClipboardData, ClipboardError> {
        if !is_format_supported(format) {
            return Err(ClipboardError::UnsupportedFormat(format));
        }
        match &mut *self.backend.borrow_mut() {
            #[cfg(not(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            )))]
            Backend::Desktop(desktop) => desktop.read(kind, format),
            #[cfg(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            ))]
            Backend::X11(x11) => x11.read(kind, format, x11::IMPATIENCE),
            #[cfg(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            ))]
            Backend::Wayland(wayland) => wayland.read(kind, format, wayland::IMPATIENCE),
        }
    }

    fn write(
        &self,
        kind: ClipboardKind,
        data: ClipboardData,
        options: ClipboardWriteOptions,
    ) -> Result<(), ClipboardError> {
        if !is_format_supported(data.format()) {
            return Err(ClipboardError::UnsupportedFormat(data.format()));
        }
        let _ = options.exclude_from_history;
        #[cfg(not(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        if let Backend::Desktop(desktop) = &mut *self.backend.borrow_mut() {
            let ClipboardData::Text(text) = data else {
                return Err(ClipboardError::UnsupportedFormat(data.format()));
            };
            return desktop.set_text(kind, text.as_str().to_owned(), options.exclude_from_history);
        }
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        {
            let entries = mime::data_to_mimes(data)?;
            self.write_mime(kind, &entries, None)
        }
    }

    fn read_mime(
        &self,
        kind: ClipboardKind,
        mime_types: &[String],
        max_bytes: usize,
    ) -> ClipboardSerial {
        let serial = self.serials.take();
        let Some(waker) = self.waker.borrow().clone() else {
            tracing::warn!(
                target: "zgui::platform",
                "a clipboard read was started before the loop could answer it"
            );
            return serial;
        };
        #[cfg(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        ))]
        {
            enum Target {
                X11(Arc<x11::X11>),
                Wayland(Arc<wayland::Wayland>),
            }
            let target = match &*self.backend.borrow() {
                Backend::X11(x11) => Target::X11(Arc::clone(x11)),
                Backend::Wayland(wayland) => Target::Wayland(Arc::clone(wayland)),
            };
            let mime_types = mime_types.to_vec();
            let _ = std::thread::Builder::new()
                .name("zgui-clipboard".to_owned())
                .spawn(move || {
                    let result = match target {
                        Target::X11(x11) => {
                            x11.read_mime(kind, &mime_types, max_bytes, x11::PATIENCE)
                        }
                        Target::Wayland(wayland) => {
                            wayland.read_mime(kind, &mime_types, max_bytes, wayland::PATIENCE)
                        }
                    };
                    waker.wake(WakeReason::ClipboardMimeRead { serial, result });
                });
        }
        #[cfg(not(any(
            target_os = "linux",
            target_os = "dragonfly",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd"
        )))]
        {
            let result = match &mut *self.backend.borrow_mut() {
                Backend::Desktop(desktop) => desktop.read_mime(kind, mime_types, max_bytes),
            };
            waker.wake(WakeReason::ClipboardMimeRead { serial, result });
        }
        serial
    }

    fn read_mime_blocking(
        &self,
        kind: ClipboardKind,
        mime_types: &[String],
        max_bytes: usize,
    ) -> Result<ClipboardMimeOffer, ClipboardError> {
        match &mut *self.backend.borrow_mut() {
            #[cfg(not(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            )))]
            Backend::Desktop(desktop) => desktop.read_mime(kind, mime_types, max_bytes),
            #[cfg(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            ))]
            Backend::X11(x11) => x11.read_mime(kind, mime_types, max_bytes, x11::IMPATIENCE),
            #[cfg(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            ))]
            Backend::Wayland(wayland) => {
                wayland.read_mime(kind, mime_types, max_bytes, wayland::IMPATIENCE)
            }
        }
    }

    fn write_mime(
        &self,
        kind: ClipboardKind,
        entries: &[ClipboardMime],
        expected_revision: Option<u64>,
    ) -> Result<(), ClipboardError> {
        match &mut *self.backend.borrow_mut() {
            #[cfg(not(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            )))]
            Backend::Desktop(desktop) => desktop.write_mime(kind, entries, expected_revision),
            #[cfg(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            ))]
            Backend::X11(x11) => x11.write_mime(kind, entries, expected_revision),
            #[cfg(any(
                target_os = "linux",
                target_os = "dragonfly",
                target_os = "freebsd",
                target_os = "netbsd",
                target_os = "openbsd"
            ))]
            Backend::Wayland(wayland) => wayland.write_mime(kind, entries, expected_revision),
        }
    }

    fn clear(&self, kind: ClipboardKind) -> Result<(), ClipboardError> {
        self.write_mime(kind, &[], None)
    }
}

#[cfg(test)]
mod tests {
    use super::DesktopClipboard;
    use zgui_platform::{Clipboard, ClipboardData, ClipboardFormat, ClipboardKind};

    #[test]
    fn html_representation_is_refused_permanently() {
        let clipboard = DesktopClipboard::new();
        let refusal = clipboard
            .read_blocking(ClipboardKind::Standard, ClipboardFormat::Html)
            .expect_err("html is not supported on this backend");
        assert!(refusal.is_permanent());
    }

    #[test]
    fn writing_html_is_refused_without_touching_the_desktop() {
        let clipboard = DesktopClipboard::new();
        let refusal = clipboard
            .write(
                ClipboardKind::Standard,
                ClipboardData::Html {
                    markup: "<b>hi</b>".into(),
                    text: None,
                },
                zgui_platform::ClipboardWriteOptions::default(),
            )
            .expect_err("html write is not supported");
        assert!(refusal.is_permanent());
    }

    #[test]
    fn every_read_started_through_the_loop_gets_its_own_identity() {
        let clipboard = DesktopClipboard::new();
        let first = clipboard.read(ClipboardKind::Standard, ClipboardFormat::Html);
        let second = clipboard.read_mime(ClipboardKind::Standard, &[], 0);
        assert_ne!(first, second);
    }
}
