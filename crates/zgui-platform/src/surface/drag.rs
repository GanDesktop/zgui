//! Content being dragged between surfaces, in either direction.

use std::path::PathBuf;

use zgui_geom::{Css, CssPx, Point};
pub use zgui_vocab::{DragAction, DragActions, DragDecision, DragMime, DropHandle};

use crate::clipboard::{ClipboardMime, LazyMimeProvider};
use crate::surface::{InputSerial, WindowIcon};

impl From<ClipboardMime> for DragMime {
    fn from(entry: ClipboardMime) -> Self {
        Self {
            mime_type: entry.mime_type,
            bytes: entry.bytes,
        }
    }
}

impl From<DragMime> for ClipboardMime {
    fn from(entry: DragMime) -> Self {
        Self {
            mime_type: entry.mime_type,
            bytes: entry.bytes,
        }
    }
}

/// Identifier of an outgoing native drag session started from a surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DragSessionId(u64);

impl DragSessionId {
    /// Creates a session identifier from a raw backend number.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw backend number.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Identifier of an incoming external drag offer over a surface.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DragOfferId(u64);

impl DragOfferId {
    /// Creates an offer identifier from a raw backend number.
    pub const fn new(raw: u64) -> Self {
        Self(raw)
    }

    /// The raw backend number.
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Visual preview attached to the pointer during an outgoing native drag.
///
/// The image holds straight (non-premultiplied) RGBA pixels; backends convert and upload it into
/// their native drag-icon surface (such as a Wayland `wl_surface` backed by `wl_shm`).
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct DragIcon {
    /// Straight RGBA pixels of the drag preview.
    pub image: WindowIcon,
    /// Grab hotspot offset from the icon's top-left corner, in CSS pixels.
    ///
    /// The icon is placed so this point sits under the pointer hotspot (i.e. offset by
    /// `(-hotspot.x, -hotspot.y)`).
    pub hotspot: Point<CssPx, Css>,
    /// Buffer scale factor of [`Self::image`] (clamped to at least `1` on upload).
    ///
    /// For example, pass `2` when `image` was rasterised at 2× physical pixels for a HiDPI screen.
    pub scale: u32,
}

impl DragIcon {
    /// Creates a drag icon at 1× scale with its top-left corner at the pointer hotspot.
    pub fn new(image: WindowIcon) -> Self {
        Self {
            image,
            hotspot: Point::new(CssPx(0.0), CssPx(0.0)),
            scale: 1,
        }
    }

    /// Sets the grab hotspot offset from the icon's top-left corner, in CSS pixels.
    #[must_use]
    pub const fn with_hotspot(mut self, hotspot: Point<CssPx, Css>) -> Self {
        self.hotspot = hotspot;
        self
    }

    /// Sets the buffer scale factor of [`Self::image`] (values below `1` are clamped to `1`).
    #[must_use]
    pub const fn with_scale(mut self, scale: u32) -> Self {
        self.scale = if scale == 0 { 1 } else { scale };
        self
    }
}

impl From<WindowIcon> for DragIcon {
    fn from(image: WindowIcon) -> Self {
        Self::new(image)
    }
}

/// Everything a surface needs to begin an outgoing native drag session.
#[derive(Clone)]
#[non_exhaustive]
pub struct DragSourceRequest {
    /// Advertised MIME payloads in preference order.
    pub entries: Vec<DragMime>,
    /// Lazily produced MIME payloads advertised alongside [`Self::entries`] and evaluated on
    /// demand only when a drop target requests them.
    pub lazy_entries: Vec<(String, LazyMimeProvider)>,
    /// Actions the source permits for this drag.
    pub actions: DragActions,
    /// Optional explicit press token captured on pointer or touch down.
    ///
    /// When `None`, the backend quotes the surface's active pointer or touch press serial.
    pub serial: Option<InputSerial>,
    /// Optional visual icon surface to follow the pointer across windows, where supported.
    pub icon: Option<DragIcon>,
}

impl core::fmt::Debug for DragSourceRequest {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let lazy_mimes: Vec<&str> = self
            .lazy_entries
            .iter()
            .map(|(mime, _)| mime.as_str())
            .collect();
        formatter
            .debug_struct("DragSourceRequest")
            .field("entries", &self.entries)
            .field("lazy_mimes", &lazy_mimes)
            .field("actions", &self.actions)
            .field("serial", &self.serial)
            .field("icon", &self.icon)
            .finish()
    }
}

impl DragSourceRequest {
    /// Creates an outgoing drag request carrying `entries` with allowed `actions`.
    pub fn new(entries: Vec<DragMime>, actions: DragActions) -> Self {
        Self {
            entries,
            lazy_entries: Vec::new(),
            actions,
            serial: None,
            icon: None,
        }
    }

    /// Advertises an additional `mime_type` whose bytes are produced lazily on demand when a drop
    /// target requests it.
    #[must_use]
    pub fn with_lazy_mime(
        mut self,
        mime_type: impl Into<String>,
        provider: impl Fn(&str) -> Option<Vec<u8>> + Send + Sync + 'static,
    ) -> Self {
        self.lazy_entries
            .push((mime_type.into(), std::sync::Arc::new(provider)));
        self
    }

    /// Quotes an explicit press serial captured when the drag gesture began.
    #[must_use]
    pub const fn with_serial(mut self, serial: InputSerial) -> Self {
        self.serial = Some(serial);
        self
    }

    /// Attaches an icon to follow the pointer while the drag is active.
    #[must_use]
    pub fn with_icon(mut self, icon: impl Into<DragIcon>) -> Self {
        self.icon = Some(icon.into());
        self
    }
}

/// Lifecycle updates reported back to the surface that started a native drag.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum DragSourceEvent {
    /// The hovered destination accepted `mime_type`, or rejected the offer when `None`.
    TargetMime {
        /// Which outgoing drag session this update belongs to.
        session: DragSessionId,
        /// The accepted media type, or `None` when no target accepts the drag.
        mime_type: Option<String>,
    },
    /// The compositor settled on `action` from the source, destination, and held modifiers.
    ActionChanged {
        /// Which outgoing drag session this update belongs to.
        session: DragSessionId,
        /// The negotiated action, or `None` when no action is currently selected.
        action: Option<DragAction>,
    },
    /// The pointer was released over a destination that accepted the drag.
    ///
    /// The destination may still be reading data or confirming conflicts; the source must keep
    /// serving requests and must not treat a move as committed until [`Self::Finished`].
    Dropped {
        /// Which outgoing drag session was dropped.
        session: DragSessionId,
        /// The last action negotiated before release.
        action: Option<DragAction>,
    },
    /// The destination completed the drop and acknowledged success.
    Finished {
        /// Which outgoing drag session finished.
        session: DragSessionId,
        /// The final negotiated action.
        action: Option<DragAction>,
    },
    /// The drag ended without completion (for example via Escape, an invalid target, or rejection).
    Cancelled {
        /// Which outgoing drag session was cancelled.
        session: DragSessionId,
    },
}

impl DragSourceEvent {
    /// Which outgoing drag session produced this event.
    pub const fn session(&self) -> DragSessionId {
        match self {
            Self::TargetMime { session, .. }
            | Self::ActionChanged { session, .. }
            | Self::Dropped { session, .. }
            | Self::Finished { session, .. }
            | Self::Cancelled { session } => *session,
        }
    }

    /// Whether this event ends the outgoing drag session.
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Finished { .. } | Self::Cancelled { .. })
    }
}

/// A stage of a drag that started outside the surface.
///
/// On lazy-transfer platforms (such as Wayland), [`Self::Entered`] reports only advertised
/// metadata (`mime_types`, `source_actions`) without reading any bytes over a pipe. The hovered
/// control replies with [`Surface::respond_drag_offer`](crate::Surface::respond_drag_offer), and
/// the payload is transferred only once the user drops. [`Self::Dropped`] carries a [`DropHandle`]
/// so the protocol acknowledges completion only after the drop operation succeeds.
///
/// ```
/// use std::path::PathBuf;
/// use zgui_geom::{CssPx, Point};
/// use zgui_platform::{DragAction, DragActions, DragEvent, DragOfferId};
///
/// let entered = DragEvent::Entered {
///     offer: DragOfferId::new(1),
///     mime_types: vec!["text/uri-list".to_owned()],
///     source_actions: DragActions::COPY,
///     action: Some(DragAction::Copy),
///     paths: vec![PathBuf::from("/tmp/a.png")],
///     position: Point::new(CssPx(4.0), CssPx(4.0)),
/// };
/// assert_eq!(entered.paths().len(), 1);
/// assert!(!entered.is_drop());
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum DragEvent {
    /// A drag came over the surface.
    Entered {
        /// Identifier of this external drag offer.
        offer: DragOfferId,
        /// Media types advertised by the drag source.
        mime_types: Vec<String>,
        /// Actions permitted by the drag source.
        source_actions: DragActions,
        /// Action currently selected by the compositor, when known.
        action: Option<DragAction>,
        /// Local paths when supplied directly by the backend without a transfer.
        paths: Vec<PathBuf>,
        /// Where the pointer is, in CSS pixels from the surface's top-left corner.
        position: Point<CssPx, Css>,
    },
    /// The drag moved while over the surface.
    Moved {
        /// Identifier of this external drag offer.
        offer: DragOfferId,
        /// Where the pointer is now.
        position: Point<CssPx, Css>,
    },
    /// The compositor changed the negotiated drag action (for example when Ctrl/Shift changed).
    ActionChanged {
        /// Identifier of this external drag offer.
        offer: DragOfferId,
        /// The newly negotiated action, or `None` when none is selected.
        action: Option<DragAction>,
    },
    /// The drag was released over the surface.
    Dropped {
        /// Identifier of this external drag offer.
        offer: DragOfferId,
        /// Media types advertised by the drag source.
        mime_types: Vec<String>,
        /// Actions permitted by the drag source.
        source_actions: DragActions,
        /// Final negotiated action at release.
        action: Option<DragAction>,
        /// Local file paths decoded for `text/uri-list` or supplied directly by the backend.
        paths: Vec<PathBuf>,
        /// Raw bytes transferred for the accepted MIME type, when available.
        payload: Option<DragMime>,
        /// Where it was dropped.
        position: Point<CssPx, Css>,
        /// One-shot completion token; call [`DropHandle::complete`] only once the drop succeeds.
        handle: DropHandle,
    },
    /// The drag left the surface without being dropped.
    Left {
        /// Identifier of the drag offer that left.
        offer: DragOfferId,
    },
}

impl DragEvent {
    /// Which external drag offer this stage belongs to.
    pub const fn offer(&self) -> DragOfferId {
        match self {
            Self::Entered { offer, .. }
            | Self::Moved { offer, .. }
            | Self::ActionChanged { offer, .. }
            | Self::Dropped { offer, .. }
            | Self::Left { offer } => *offer,
        }
    }

    /// Everything being dragged as paths, which is nothing on stages that do not carry paths.
    pub fn paths(&self) -> &[PathBuf] {
        match self {
            Self::Entered { paths, .. } | Self::Dropped { paths, .. } => paths,
            Self::Moved { .. } | Self::ActionChanged { .. } | Self::Left { .. } => &[],
        }
    }

    /// Media types advertised on stages that carry them.
    pub fn mime_types(&self) -> &[String] {
        match self {
            Self::Entered { mime_types, .. } | Self::Dropped { mime_types, .. } => mime_types,
            Self::Moved { .. } | Self::ActionChanged { .. } | Self::Left { .. } => &[],
        }
    }

    /// Where the pointer is, when this stage says.
    pub fn position(&self) -> Option<Point<CssPx, Css>> {
        match self {
            Self::Entered { position, .. }
            | Self::Moved { position, .. }
            | Self::Dropped { position, .. } => Some(*position),
            Self::ActionChanged { .. } | Self::Left { .. } => None,
        }
    }

    /// Whether this stage is the release.
    pub const fn is_drop(&self) -> bool {
        matches!(self, Self::Dropped { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DragAction, DragActions, DragEvent, DragOfferId, DragSessionId, DragSourceEvent, DropHandle,
    };
    use std::path::PathBuf;
    use zgui_geom::{Css, CssPx, Point};

    fn at(x: f32) -> Point<CssPx, Css> {
        Point::new(CssPx(x), CssPx(0.0))
    }

    #[test]
    fn only_the_carrying_stages_report_paths() {
        let offer = DragOfferId::new(1);
        let entered = DragEvent::Entered {
            offer,
            mime_types: vec!["text/uri-list".to_owned()],
            source_actions: DragActions::COPY,
            action: Some(DragAction::Copy),
            paths: vec![PathBuf::from("/a")],
            position: at(1.0),
        };
        assert_eq!(entered.paths().len(), 1);
        assert!(
            DragEvent::Moved {
                offer,
                position: at(2.0),
            }
            .paths()
            .is_empty()
        );
        assert!(DragEvent::Left { offer }.paths().is_empty());
    }

    #[test]
    fn leaving_reports_no_position_and_motion_stages_do() {
        let offer = DragOfferId::new(2);
        assert_eq!(DragEvent::Left { offer }.position(), None);
        assert_eq!(
            DragEvent::ActionChanged {
                offer,
                action: Some(DragAction::Move),
            }
            .position(),
            None
        );
        assert_eq!(
            DragEvent::Moved {
                offer,
                position: at(3.0),
            }
            .position(),
            Some(at(3.0))
        );
    }

    #[test]
    fn exactly_one_stage_is_the_release() {
        let offer = DragOfferId::new(3);
        let dropped = DragEvent::Dropped {
            offer,
            mime_types: Vec::new(),
            source_actions: DragActions::NONE,
            action: None,
            paths: Vec::new(),
            payload: None,
            position: at(0.0),
            handle: DropHandle::noop(),
        };
        assert!(dropped.is_drop());
        assert!(!DragEvent::Left { offer }.is_drop());
    }

    #[test]
    fn drag_source_event_distinguishes_terminal_states() {
        let session = DragSessionId::new(7);
        let dropped = DragSourceEvent::Dropped {
            session,
            action: Some(DragAction::Copy),
        };
        let finished = DragSourceEvent::Finished {
            session,
            action: Some(DragAction::Copy),
        };
        let cancelled = DragSourceEvent::Cancelled { session };

        assert_eq!(dropped.session(), session);
        assert!(!dropped.is_terminal());
        assert!(finished.is_terminal());
        assert!(cancelled.is_terminal());
    }

    #[test]
    fn drag_icon_builds_from_window_icon_and_clamps_zero_scale() {
        use super::{DragIcon, DragMime, DragSourceRequest};
        use crate::surface::WindowIcon;

        let image = WindowIcon::from_rgba(vec![0xFF; 4 * 4 * 4], 4, 4).expect("valid icon");
        let icon = DragIcon::new(image.clone())
            .with_hotspot(Point::new(CssPx(2.0), CssPx(3.0)))
            .with_scale(0);
        assert_eq!(icon.scale, 1);
        assert_eq!(icon.hotspot, Point::new(CssPx(2.0), CssPx(3.0)));

        let request = DragSourceRequest::new(
            vec![DragMime::new("text/plain;charset=utf-8", b"hi")],
            DragActions::COPY,
        )
        .with_icon(image)
        .with_lazy_mime("application/vnd.portal.filetransfer", |_| {
            Some(b"transfer-key".to_vec())
        });
        assert_eq!(request.icon.as_ref().map(|i| i.scale), Some(1));
        assert_eq!(request.lazy_entries.len(), 1);
        assert_eq!(
            request.lazy_entries[0].0,
            "application/vnd.portal.filetransfer"
        );
        assert_eq!(
            (request.lazy_entries[0].1)("application/vnd.portal.filetransfer"),
            Some(b"transfer-key".to_vec())
        );
    }
}
