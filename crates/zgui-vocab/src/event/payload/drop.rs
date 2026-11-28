//! Content being dragged over or dropped onto a window, and how a target answers it.

use core::fmt;
use core::ops::{BitAnd, BitOr, BitOrAssign};
use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use zgui_geom::{Css, CssPx, Point};

/// A single drag-and-drop action chosen by the compositor or preferred by a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DragAction {
    /// Duplicate the dragged content at the destination.
    Copy,
    /// Transfer the dragged content and let the source release it once finished.
    Move,
    /// Link to the dragged content rather than duplicating it.
    Link,
}

impl DragAction {
    /// This action as a single-element [`DragActions`] mask.
    pub const fn as_mask(self) -> DragActions {
        match self {
            Self::Copy => DragActions::COPY,
            Self::Move => DragActions::MOVE,
            Self::Link => DragActions::LINK,
        }
    }
}

/// A set of drag-and-drop actions offered by a source or accepted by a target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct DragActions(u8);

impl DragActions {
    /// No action.
    pub const NONE: Self = Self(0);
    /// Copying the dragged content is allowed.
    pub const COPY: Self = Self(1 << 0);
    /// Moving the dragged content is allowed.
    pub const MOVE: Self = Self(1 << 1);
    /// Linking to the dragged content is allowed.
    pub const LINK: Self = Self(1 << 2);

    /// Whether no actions are set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every bit in `other` is set in `self`.
    pub const fn contains(self, other: Self) -> bool {
        (self.0 & other.0) == other.0
    }

    /// The union of two action sets.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The intersection of two action sets.
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
}

impl BitOr for DragActions {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.union(rhs)
    }
}

impl BitOrAssign for DragActions {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = self.union(rhs);
    }
}

impl BitAnd for DragActions {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        self.intersection(rhs)
    }
}

/// A single media-type payload offered by a drag source or received on drop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragMime {
    /// The media type, such as `"text/uri-list"`, `"text/plain;charset=utf-8"`, or `"image/png"`.
    pub mime_type: String,
    /// The raw bytes carried for [`Self::mime_type`].
    pub bytes: Vec<u8>,
}

impl DragMime {
    /// Creates a drag payload entry for `mime_type`.
    pub fn new(mime_type: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            mime_type: mime_type.into(),
            bytes: bytes.into(),
        }
    }

    /// Interprets the payload bytes as strict UTF-8 text.
    pub fn as_utf8(&self) -> Result<&str, core::str::Utf8Error> {
        core::str::from_utf8(&self.bytes)
    }

    /// Interprets the payload bytes as UTF-8 text, replacing invalid sequences.
    pub fn to_utf8_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }
}

/// A hovered control's negotiation decision for an external drag offer.
///
/// Returned back to the platform after `drag_enter` / `drag_over` dispatch so the compositor can
/// update the pointer feedback and settle the action before the user lets go.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DragDecision {
    /// The advertised MIME type this target accepts, or `None` to reject dropping here.
    pub mime_type: Option<String>,
    /// Actions this target supports if the user drops here.
    pub supported_actions: DragActions,
    /// Action this target prefers by default when modifiers do not override it.
    pub preferred_action: Option<DragAction>,
}

impl DragDecision {
    /// Rejects dropping at the hovered location.
    pub fn reject() -> Self {
        Self::default()
    }

    /// Accepts `mime_type` with the given supported and preferred actions.
    pub fn accept(
        mime_type: impl Into<String>,
        supported_actions: DragActions,
        preferred_action: Option<DragAction>,
    ) -> Self {
        Self {
            mime_type: Some(mime_type.into()),
            supported_actions,
            preferred_action,
        }
    }

    /// Whether this decision accepts a drop.
    pub const fn is_accepted(&self) -> bool {
        self.mime_type.is_some() && !self.supported_actions.is_empty()
    }
}

/// Callback invoked by [`DropHandle::read_mime`] to fetch an additional MIME type from a live
/// dropped offer without blocking the caller's thread.
type ReadMimeFn = dyn Fn(&str, usize, Box<dyn FnOnce(Option<DragMime>) + Send>) + Send + Sync;

/// One-shot completion token tying protocol drop acknowledgement to the actual operation outcome.
///
/// While alive (before [`Self::complete`] or [`Self::reject`] consumes it), the handle can also
/// read additional offered MIME types from the dropped offer via [`Self::read_mime`].
///
/// Dropping an uncompleted handle safely rejects the drop rather than reporting success, so an
/// abandoned conflict prompt, failed transfer, or early return can never cause a move source to
/// delete its original data.
pub struct DropHandle {
    finish: Option<Box<dyn FnOnce(bool) + Send>>,
    read_mime: Option<Arc<ReadMimeFn>>,
}

impl DropHandle {
    /// Creates a handle backed by `finish(true)` on completion and `finish(false)` on rejection.
    pub fn new(finish: impl FnOnce(bool) + Send + 'static) -> Self {
        Self {
            finish: Some(Box::new(finish)),
            read_mime: None,
        }
    }

    /// Attaches an asynchronous reader for fetching additional MIME types from the live dropped
    /// offer before completion or rejection.
    #[must_use]
    pub fn with_reader(
        mut self,
        read_mime: impl Fn(&str, usize, Box<dyn FnOnce(Option<DragMime>) + Send>)
        + Send
        + Sync
        + 'static,
    ) -> Self {
        self.read_mime = Some(Arc::new(read_mime));
        self
    }

    /// A handle for backends whose drop protocol has no post-drop acknowledgement.
    pub fn noop() -> Self {
        Self {
            finish: None,
            read_mime: None,
        }
    }

    /// Reads an additional advertised `mime_type` (capped at `max_bytes`) from the still-live
    /// dropped offer and passes the result to `then` without blocking the UI thread.
    ///
    /// Calls `then(None)` immediately when this handle cannot read additional MIME types, has been
    /// moved out via [`Self::take`], or the peer does not offer `mime_type`.
    pub fn read_mime(
        &self,
        mime_type: &str,
        max_bytes: usize,
        then: impl FnOnce(Option<DragMime>) + Send + 'static,
    ) {
        if let Some(reader) = &self.read_mime {
            reader(mime_type, max_bytes, Box::new(then));
        } else {
            then(None);
        }
    }

    /// Whether this handle can fetch additional MIME types from the dropped offer.
    pub fn can_read_mime(&self) -> bool {
        self.read_mime.is_some()
    }

    /// Reports that the drop operation succeeded completely (for example `wl_data_offer.finish`).
    pub fn complete(mut self) {
        self.read_mime = None;
        if let Some(finish) = self.finish.take() {
            finish(true);
        }
    }

    /// Reports that the drop was cancelled, refused, or failed; does not send `finish`.
    pub fn reject(mut self) {
        self.read_mime = None;
        if let Some(finish) = self.finish.take() {
            finish(false);
        }
    }

    /// Moves the completion callback and MIME reader out into a fresh handle, leaving `self` inert.
    pub fn take(&mut self) -> Option<Self> {
        let finish = self.finish.take();
        let read_mime = self.read_mime.take();
        if finish.is_none() && read_mime.is_none() {
            return None;
        }
        Some(Self { finish, read_mime })
    }

    /// Whether this handle still owns a pending completion callback.
    pub const fn is_pending(&self) -> bool {
        self.finish.is_some()
    }
}

impl Drop for DropHandle {
    fn drop(&mut self) {
        self.read_mime = None;
        if let Some(finish) = self.finish.take() {
            finish(false);
        }
    }
}

impl fmt::Debug for DropHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DropHandle")
            .field("pending", &self.finish.is_some())
            .field("readable", &self.read_mime.is_some())
            .finish()
    }
}

/// What a drag-hover or drop event carries: metadata, payload when dropped, and position.
///
/// During hover (`drag_enter` / `drag_over` / `drag_leave`), platforms that support lazy transfer
/// (such as Wayland) advertise [`Self::mime_types`] and [`Self::source_actions`] without reading
/// any data over a pipe. On `drop`, [`Self::paths`] and [`Self::payload`] carry the transferred
/// data for the negotiated MIME type, and the handling control claims completion ownership via
/// `EventCx::take_drop_handle`.
///
/// ```
/// use std::path::PathBuf;
/// use zgui_geom::{CssPx, Point};
/// use zgui_vocab::DropEvent;
///
/// let event = DropEvent::from_paths(
///     vec![PathBuf::from("/tmp/a.png"), PathBuf::from("/tmp/b.png")],
///     Point::new(CssPx(10.0), CssPx(20.0)),
/// );
/// assert_eq!(event.paths.len(), 2);
/// assert!(event.offers_files());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct DropEvent {
    /// Local file paths carried by the drop, in the order the platform listed them.
    ///
    /// Populated on drop when the negotiated MIME is `text/uri-list`, or up front by backends
    /// whose native drag API reports file paths directly during hover.
    pub paths: Vec<PathBuf>,
    /// Media types advertised by the drag source (e.g. `"text/uri-list"`, `"text/plain;charset=utf-8"`).
    pub mime_types: Vec<String>,
    /// Raw bytes transferred for the negotiated MIME type on drop, when available.
    pub payload: Option<DragMime>,
    /// Actions allowed by the drag source.
    pub source_actions: DragActions,
    /// Action currently selected by the compositor, when known.
    pub action: Option<DragAction>,
    /// Where the drag or drop happened, in CSS pixels from the window's top-left corner.
    pub position: Point<CssPx, Css>,
}

impl DropEvent {
    /// Creates a path-only drop event, populating `"text/uri-list"` in [`Self::mime_types`] when
    /// `paths` is non-empty.
    pub fn from_paths(paths: Vec<PathBuf>, position: Point<CssPx, Css>) -> Self {
        let mime_types = if paths.is_empty() {
            Vec::new()
        } else {
            vec!["text/uri-list".to_owned()]
        };
        Self {
            paths,
            mime_types,
            payload: None,
            source_actions: DragActions::COPY,
            action: Some(DragAction::Copy),
            position,
        }
    }

    /// Whether the event carries neither paths, advertised MIME types, nor a raw payload.
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty() && self.mime_types.is_empty() && self.payload.is_none()
    }

    /// Whether the drag source advertised `mime_type` (ASCII case-insensitive).
    pub fn offers_mime(&self, mime_type: &str) -> bool {
        self.mime_types
            .iter()
            .any(|offered| offered.eq_ignore_ascii_case(mime_type))
    }

    /// Whether the drag source advertised an XDG Document Portal file transfer
    /// (`application/vnd.portal.filetransfer`).
    pub fn offers_portal_file_transfer(&self) -> bool {
        self.offers_mime("application/vnd.portal.filetransfer")
    }

    /// Whether the drag carries or advertises files (`text/uri-list` or
    /// `application/vnd.portal.filetransfer`).
    pub fn offers_files(&self) -> bool {
        !self.paths.is_empty()
            || self.offers_mime("text/uri-list")
            || self.offers_portal_file_transfer()
    }

    /// Preferred plain-text MIME type advertised by the drag source, preserving the peer's exact
    /// string for [`DragDecision::accept`].
    pub fn best_text_mime(&self) -> Option<&str> {
        const PREFERRED: [&str; 5] = [
            "text/plain;charset=utf-8",
            "UTF8_STRING",
            "text/plain",
            "STRING",
            "TEXT",
        ];
        PREFERRED.iter().find_map(|wanted| {
            self.mime_types
                .iter()
                .find(|offered| offered.eq_ignore_ascii_case(wanted))
                .map(String::as_str)
        })
    }

    /// Whether the drag advertises plain text.
    pub fn offers_text(&self) -> bool {
        self.best_text_mime().is_some()
    }

    /// Preferred image MIME type advertised by the drag source, preserving the peer's exact string
    /// for [`DragDecision::accept`].
    pub fn best_image_mime(&self) -> Option<&str> {
        const PREFERRED: [&str; 5] = [
            "image/png",
            "image/webp",
            "image/jpeg",
            "image/bmp",
            "image/gif",
        ];
        PREFERRED
            .iter()
            .find_map(|wanted| {
                self.mime_types
                    .iter()
                    .find(|offered| offered.eq_ignore_ascii_case(wanted))
                    .map(String::as_str)
            })
            .or_else(|| {
                self.mime_types
                    .iter()
                    .find(|offered| {
                        offered
                            .get(..6)
                            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
                    })
                    .map(String::as_str)
            })
    }

    /// Whether the drag advertises an image payload.
    pub fn offers_image(&self) -> bool {
        self.best_image_mime().is_some()
    }

    /// The transferred bytes when [`Self::payload`] matches `mime_type` (ASCII case-insensitive).
    pub fn payload_for(&self, mime_type: &str) -> Option<&[u8]> {
        self.payload
            .as_ref()
            .filter(|entry| entry.mime_type.eq_ignore_ascii_case(mime_type))
            .map(|entry| entry.bytes.as_slice())
    }

    /// The transferred payload decoded as lossy UTF-8 when it carries a text media type.
    pub fn text(&self) -> Option<Cow<'_, str>> {
        let entry = self.payload.as_ref()?;
        let is_text = entry
            .mime_type
            .get(..5)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("text/"))
            || entry.mime_type.eq_ignore_ascii_case("UTF8_STRING")
            || entry.mime_type.eq_ignore_ascii_case("STRING")
            || entry.mime_type.eq_ignore_ascii_case("TEXT");
        is_text.then(|| entry.to_utf8_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::{DragAction, DragActions, DragDecision, DropEvent, DropHandle};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU8, Ordering};
    use zgui_geom::{CssPx, Point};

    #[test]
    fn a_drop_carries_every_path_at_once() {
        let event = DropEvent::from_paths(
            vec![PathBuf::from("/a"), PathBuf::from("/b")],
            Point::new(CssPx(0.0), CssPx(0.0)),
        );
        assert!(!event.is_empty());
        assert!(event.offers_files());
        assert_eq!(event.paths.len(), 2);
    }

    #[test]
    fn a_drop_of_nothing_is_expressible() {
        let event = DropEvent::from_paths(Vec::new(), Point::new(CssPx(0.0), CssPx(0.0)));
        assert!(event.is_empty());
        assert!(!event.offers_files());
    }

    #[test]
    fn drag_actions_combine_and_intersect() {
        let offered = DragActions::COPY | DragActions::MOVE;
        assert!(offered.contains(DragActions::COPY));
        assert!(offered.contains(DragActions::MOVE));
        assert!(!offered.contains(DragActions::LINK));
        assert_eq!(offered & DragAction::Copy.as_mask(), DragActions::COPY);
        assert!(DragActions::NONE.is_empty());
    }

    #[test]
    fn drop_handle_rejects_on_drop_and_completes_once() {
        let state = Arc::new(AtomicU8::new(0));
        {
            let state = Arc::clone(&state);
            let _unclaimed = DropHandle::new(move |ok| {
                state.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
            });
        }
        assert_eq!(state.load(Ordering::Relaxed), 2);

        let completed = Arc::new(AtomicU8::new(0));
        let mut handle = {
            let completed = Arc::clone(&completed);
            DropHandle::new(move |ok| {
                completed.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
            })
        };
        let taken = handle.take().expect("first take succeeds");
        assert!(handle.take().is_none());
        drop(handle);
        assert_eq!(completed.load(Ordering::Relaxed), 0);
        taken.complete();
        assert_eq!(completed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn drag_decision_distinguishes_accept_and_reject() {
        assert!(!DragDecision::reject().is_accepted());
        assert!(
            DragDecision::accept("text/uri-list", DragActions::COPY, Some(DragAction::Copy))
                .is_accepted()
        );
        assert!(!DragDecision::accept("text/uri-list", DragActions::NONE, None).is_accepted());
    }

    #[test]
    fn drop_event_helpers_select_preferred_text_and_image_mimes() {
        use super::DragMime;

        let mut event = DropEvent::from_paths(Vec::new(), Point::new(CssPx(1.0), CssPx(2.0)));
        event.mime_types = vec![
            "STRING".to_owned(),
            "text/plain".to_owned(),
            "TEXT/PLAIN;CHARSET=UTF-8".to_owned(),
            "image/jpeg".to_owned(),
            "image/png".to_owned(),
        ];
        assert!(event.offers_text());
        assert_eq!(event.best_text_mime(), Some("TEXT/PLAIN;CHARSET=UTF-8"));
        assert!(event.offers_image());
        assert_eq!(event.best_image_mime(), Some("image/png"));
        assert!(!event.offers_files());
        assert!(!event.offers_portal_file_transfer());

        event.mime_types = vec!["application/vnd.portal.filetransfer".to_owned()];
        assert!(event.offers_portal_file_transfer());
        assert!(event.offers_files());

        event.payload = Some(DragMime::new("image/png", vec![0x89, b'P', b'N', b'G']));
        assert_eq!(
            event.payload_for("IMAGE/PNG"),
            Some([0x89, b'P', b'N', b'G'].as_slice())
        );
        assert_eq!(event.text(), None);

        event.payload = Some(DragMime::new("text/plain;charset=utf-8", b"hello world"));
        assert_eq!(event.text().as_deref(), Some("hello world"));
        assert_eq!(
            event.payload.as_ref().and_then(|p| p.as_utf8().ok()),
            Some("hello world")
        );
    }

    #[test]
    fn drop_handle_reader_moves_with_take_and_reads_additional_mimes() {
        use super::DragMime;
        use std::sync::Mutex;

        let mut handle = DropHandle::new(|_| {}).with_reader(|mime, limit, then| {
            let mut bytes = format!("extra:{mime}").into_bytes();
            bytes.truncate(limit);
            then(Some(DragMime::new(mime, bytes)));
        });
        assert!(handle.can_read_mime());

        let taken = handle.take().expect("take succeeds");
        assert!(!handle.can_read_mime());
        assert!(taken.can_read_mime());

        let result = Arc::new(Mutex::new(None));
        let out = Arc::clone(&result);
        taken.read_mime("text/html", 64, move |payload| {
            *out.lock().expect("lock") = payload;
        });
        assert_eq!(
            *result.lock().expect("lock"),
            Some(DragMime::new("text/html", b"extra:text/html"))
        );

        let after_take = Arc::new(Mutex::new(Some(DragMime::new("sentinel", b"x"))));
        let out_after = Arc::clone(&after_take);
        handle.read_mime("text/html", 64, move |payload| {
            *out_after.lock().expect("lock") = payload;
        });
        assert_eq!(*after_take.lock().expect("lock"), None);
        taken.complete();
    }
}
