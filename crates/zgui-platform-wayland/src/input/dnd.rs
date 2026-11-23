//! Content dragged over a window from another application, negotiated dynamically and
//! acknowledged only once the drop operation succeeds.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use smithay_client_toolkit::data_device_manager::data_offer::DragOffer;
use smithay_client_toolkit::reexports::client::Connection;
use smithay_client_toolkit::reexports::client::protocol::wl_data_device_manager::DndAction;
use zgui_geom::{Css, CssPx, Point};
use zgui_platform::{
    DragAction, DragActions, DragDecision, DragEvent, DragMime, DragOfferId, DropHandle, SurfaceId,
    WakeReason, Waker,
};

/// Incoming Wayland drag state shared between the event loop, surfaces, and live [`DropHandle`]s.
///
/// # Lazy transfer and delayed acknowledgement
///
/// While the pointer hovers over a window, only advertised metadata (`mime_types`,
/// `source_actions`) is delivered — no pipe transfer starts on entry. Controls answer with
/// [`Surface::respond_drag_offer`](zgui_platform::Surface::respond_drag_offer), which immediately
/// forwards `wl_data_offer.accept` and `wl_data_offer.set_actions` to the compositor.
///
/// Only once the user releases over an accepting target does a worker thread read the chosen MIME
/// type from the pipe. Even after reading finishes, the `wl_data_offer` stays alive in
/// [`Inner::pending_finish`] until the handling operation invokes [`DropHandle::complete`] (which
/// sends `wl_data_offer.finish` and destroys the offer) or [`DropHandle::reject`] / drops the
/// handle (which destroys the offer without sending `finish`, preventing a move source from
/// deleting its original data).
#[derive(Debug, Default)]
pub struct Drag {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Connection used to flush immediate negotiation replies and completion acknowledgements.
    conn: Option<Connection>,
    /// Waker poked by drop-read threads and off-thread handle completions.
    waker: Option<Arc<dyn Waker>>,
    /// Monotonic identifier counter for incoming offers.
    next_offer: u64,
    /// The drag currently hovering over a window.
    active: Option<Session>,
    /// Dropped offers currently reading their negotiated initial MIME payload on a worker thread.
    reading: HashMap<DragOfferId, Session>,
    /// Completed drop pipe reads waiting to be emitted on the loop thread.
    completed_reads: Vec<CompletedRead>,
    /// Dropped offers awaiting explicit completion, additional MIME reads, or rejection via
    /// [`DropHandle`].
    pending_finish: HashMap<DragOfferId, PendingOffer>,
}

impl core::fmt::Debug for Inner {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Inner")
            .field("active", &self.active.as_ref().map(|s| s.id))
            .field("reading", &self.reading.len())
            .field("completed_reads", &self.completed_reads.len())
            .field("pending_finish", &self.pending_finish.len())
            .finish_non_exhaustive()
    }
}

/// A dropped offer held alive until its [`DropHandle`] completes or rejects.
struct PendingOffer {
    offer: DragOffer,
    mime_types: Vec<String>,
}

/// One incoming drag over one window.
#[derive(Debug)]
struct Session {
    id: DragOfferId,
    surface: SurfaceId,
    offer: DragOffer,
    mime_types: Vec<String>,
    source_actions: DragActions,
    selected_action: Option<DragAction>,
    accepted_mime: Option<String>,
    at: Point<CssPx, Css>,
}

struct CompletedRead {
    offer: DragOfferId,
    mime_type: String,
    bytes: Option<Vec<u8>>,
}

impl Drag {
    /// Wires the connection and loop waker once the event loop starts.
    pub(crate) fn attach(&self, conn: Connection, waker: Arc<dyn Waker>) {
        let mut inner = self.lock();
        inner.conn = Some(conn);
        inner.waker = Some(waker);
    }

    /// Records a drag entering `surface` and produces its [`DragEvent::Entered`] metadata event
    /// immediately without starting any pipe read.
    pub(crate) fn entered(
        &self,
        surface: SurfaceId,
        offer: DragOffer,
        at: Point<CssPx, Css>,
    ) -> (SurfaceId, DragEvent) {
        let mut inner = self.lock();
        if let Some(old) = inner.active.take() {
            old.offer.destroy();
        }
        inner.next_offer = inner.next_offer.saturating_add(1);
        let id = DragOfferId::new(inner.next_offer);
        let mime_types = offer.with_mime_types(<[String]>::to_vec);
        let source_actions = from_wayland_actions(offer.source_actions);
        let selected_action = from_wayland_action(offer.selected_action);

        inner.active = Some(Session {
            id,
            surface,
            offer,
            mime_types: mime_types.clone(),
            source_actions,
            selected_action,
            accepted_mime: None,
            at,
        });
        (
            surface,
            DragEvent::Entered {
                offer: id,
                mime_types,
                source_actions,
                action: selected_action,
                paths: Vec::new(),
                position: at,
            },
        )
    }

    /// Forwards a hovered control's negotiation decision (`accept` + `set_actions`) to Wayland.
    pub(crate) fn respond(
        &self,
        surface: SurfaceId,
        offer_id: DragOfferId,
        decision: DragDecision,
    ) {
        let mut inner = self.lock();
        let Some(session) = inner
            .active
            .as_mut()
            .filter(|s| s.id == offer_id && s.surface == surface)
        else {
            return;
        };

        let chosen_mime = decision.mime_type.as_deref().and_then(|wanted| {
            crate::clipboard::mime::match_offered(&session.mime_types, wanted).map(str::to_owned)
        });
        let (supported, preferred) =
            resolve_offer_actions(decision.supported_actions, decision.preferred_action);

        if let Some(mime) = chosen_mime
            && supported != DndAction::None
        {
            session
                .offer
                .accept_mime_type(session.offer.serial, Some(mime.clone()));
            session.offer.set_actions(supported, preferred);
            session.accepted_mime = Some(mime);
        } else {
            session.offer.accept_mime_type(session.offer.serial, None);
            session.offer.set_actions(DndAction::None, DndAction::None);
            session.accepted_mime = None;
        }
        if let Some(conn) = &inner.conn {
            let _ = conn.flush();
        }
    }

    /// Records the pointer moving over the active surface.
    pub(crate) fn moved(&self, at: Point<CssPx, Css>) -> Option<(SurfaceId, DragEvent)> {
        let mut inner = self.lock();
        let session = inner.active.as_mut()?;
        session.at = at;
        Some((
            session.surface,
            DragEvent::Moved {
                offer: session.id,
                position: at,
            },
        ))
    }

    /// Updates advertised source actions when the compositor delivers them on an active offer.
    pub(crate) fn source_actions_changed(&self, offer: &DragOffer, actions: DndAction) {
        let mut inner = self.lock();
        if let Some(session) = inner
            .active
            .as_mut()
            .filter(|s| s.offer.inner() == offer.inner())
        {
            session.source_actions = from_wayland_actions(actions);
        }
    }

    /// Records a compositor-selected action change (for example when Ctrl/Shift is toggled).
    pub(crate) fn selected_action_changed(
        &self,
        offer: &DragOffer,
        action: DndAction,
    ) -> Option<(SurfaceId, DragEvent)> {
        let mut inner = self.lock();
        let session = inner
            .active
            .as_mut()
            .filter(|s| s.offer.inner() == offer.inner())?;
        let mapped = from_wayland_action(action);
        if session.selected_action == mapped {
            return None;
        }
        session.selected_action = mapped;
        Some((
            session.surface,
            DragEvent::ActionChanged {
                offer: session.id,
                action: mapped,
            },
        ))
    }

    /// Records the drag leaving the surface.
    ///
    /// A drop is always followed by `wl_data_device.leave`; because [`Self::dropped`] moves a
    /// dropped session out of `active` into `reading`, a post-drop `leave` is naturally ignored.
    pub(crate) fn left(&self) -> Option<(SurfaceId, DragEvent)> {
        let mut inner = self.lock();
        let session = inner.active.take()?;
        session.offer.destroy();
        if let Some(conn) = &inner.conn {
            let _ = conn.flush();
        }
        Some((session.surface, DragEvent::Left { offer: session.id }))
    }

    /// Starts reading the negotiated MIME payload when the user lets go over an accepting target.
    pub(crate) fn dropped(self: &Arc<Self>) {
        let (offer_id, mime, reader, waker) = {
            let mut inner = self.lock();
            let Some(mut session) = inner.active.take() else {
                return;
            };
            session.offer.dropped = true;
            let offer_id = session.id;

            let Some(mime) = session.accepted_mime.clone() else {
                // No control accepted this offer; destroy without sending `finish`.
                session.offer.destroy();
                if let Some(conn) = &inner.conn {
                    let _ = conn.flush();
                }
                return;
            };

            let Ok(reader) = session.offer.receive(mime.clone()) else {
                session.offer.destroy();
                if let Some(conn) = &inner.conn {
                    let _ = conn.flush();
                }
                return;
            };
            let waker = inner.waker.clone();
            if let Some(conn) = &inner.conn {
                let _ = conn.flush();
            }
            inner.reading.insert(offer_id, session);
            (offer_id, mime, reader, waker)
        };

        let drag = Arc::clone(self);
        let _ = std::thread::Builder::new()
            .name("zgui-drop".to_owned())
            .spawn(move || {
                let bytes =
                    crate::clipboard::pipe::read(reader, crate::clipboard::pipe::PATIENCE).ok();
                {
                    let mut inner = drag.lock();
                    inner.completed_reads.push(CompletedRead {
                        offer: offer_id,
                        mime_type: mime,
                        bytes,
                    });
                }
                if let Some(waker) = waker {
                    waker.wake(WakeReason::AppWork);
                }
            });
    }

    /// Drains finished drop pipe reads and produces [`DragEvent::Dropped`] carrying a [`DropHandle`].
    pub(crate) fn take_completed_drops(self: &Arc<Self>) -> Vec<(SurfaceId, DragEvent)> {
        let mut inner = self.lock();
        let completed = std::mem::take(&mut inner.completed_reads);
        let mut events = Vec::new();

        for read in completed {
            let Some(session) = inner.reading.remove(&read.offer) else {
                continue;
            };
            let offer_id = session.id;
            inner.pending_finish.insert(
                offer_id,
                PendingOffer {
                    offer: session.offer,
                    mime_types: session.mime_types.clone(),
                },
            );

            let decoded_paths = if read
                .mime_type
                .eq_ignore_ascii_case(crate::clipboard::mime::URI_LIST)
            {
                read.bytes.as_deref().map(paths).unwrap_or_default()
            } else {
                Vec::new()
            };
            let payload = read.bytes.map(|bytes| DragMime::new(read.mime_type, bytes));

            let drag_finish = Arc::clone(self);
            let drag_read = Arc::clone(self);
            let handle = DropHandle::new(move |accepted| {
                drag_finish.finish_drop(offer_id, accepted);
            })
            .with_reader(move |mime, max_bytes, then| {
                drag_read.read_drop_mime(offer_id, mime, max_bytes, then);
            });

            events.push((
                session.surface,
                DragEvent::Dropped {
                    offer: offer_id,
                    mime_types: session.mime_types,
                    source_actions: session.source_actions,
                    action: session.selected_action,
                    paths: decoded_paths,
                    payload,
                    position: session.at,
                    handle,
                },
            ));
        }

        events
    }

    /// Reads an additional advertised MIME type from a still-live dropped offer on a worker thread.
    fn read_drop_mime(
        &self,
        offer_id: DragOfferId,
        mime_type: &str,
        max_bytes: usize,
        then: Box<dyn FnOnce(Option<DragMime>) + Send>,
    ) {
        let started = {
            let inner = self.lock();
            inner.pending_finish.get(&offer_id).and_then(|pending| {
                let matched =
                    crate::clipboard::mime::match_offered(&pending.mime_types, mime_type)?
                        .to_owned();
                let reader = pending.offer.receive(matched.clone()).ok()?;
                if let Some(conn) = &inner.conn {
                    let _ = conn.flush();
                }
                Some((matched, reader, inner.waker.clone()))
            })
        };
        let Some((matched_mime, reader, waker)) = started else {
            then(None);
            return;
        };
        let limit = max_bytes.min(crate::clipboard::pipe::LIMIT);
        let _ = std::thread::Builder::new()
            .name("zgui-drop-mime".to_owned())
            .spawn(move || {
                let deadline = std::time::Instant::now() + crate::clipboard::pipe::PATIENCE;
                let payload = crate::clipboard::pipe::read_until(reader, deadline, limit)
                    .ok()
                    .map(|bytes| DragMime::new(matched_mime, bytes));
                then(payload);
                if let Some(waker) = waker {
                    waker.wake(WakeReason::AppWork);
                }
            });
    }

    /// Completes or rejects a dropped offer once the application finishes processing it.
    fn finish_drop(&self, offer_id: DragOfferId, accepted: bool) {
        let waker = {
            let mut inner = self.lock();
            let Some(pending) = inner.pending_finish.remove(&offer_id) else {
                return;
            };
            if accepted {
                pending.offer.finish();
            }
            pending.offer.destroy();
            if let Some(conn) = &inner.conn {
                let _ = conn.flush();
            }
            inner.waker.clone()
        };
        if let Some(waker) = waker {
            waker.wake(WakeReason::AppWork);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Maps platform [`DragActions`] to core Wayland [`DndAction`] bits.
pub(crate) fn to_wayland_actions(actions: DragActions) -> DndAction {
    let mut out = DndAction::None;
    if actions.contains(DragActions::COPY) {
        out |= DndAction::Copy;
    }
    if actions.contains(DragActions::MOVE) {
        out |= DndAction::Move;
    }
    out
}

/// Maps core Wayland [`DndAction`] bits to platform [`DragActions`].
pub(crate) fn from_wayland_actions(actions: DndAction) -> DragActions {
    let mut out = DragActions::NONE;
    if actions.contains(DndAction::Copy) {
        out |= DragActions::COPY;
    }
    if actions.contains(DndAction::Move) {
        out |= DragActions::MOVE;
    }
    out
}

/// Maps a single selected Wayland [`DndAction`] to a platform [`DragAction`].
pub(crate) fn from_wayland_action(action: DndAction) -> Option<DragAction> {
    if action == DndAction::Copy {
        Some(DragAction::Copy)
    } else if action == DndAction::Move {
        Some(DragAction::Move)
    } else {
        None
    }
}

/// Resolves `(supported, preferred)` for `wl_data_offer.set_actions`, guaranteeing that
/// `preferred` is either `DndAction::None` or a single bit contained in `supported` as required
/// by the Wayland protocol.
pub(crate) fn resolve_offer_actions(
    supported: DragActions,
    preferred: Option<DragAction>,
) -> (DndAction, DndAction) {
    let mask = to_wayland_actions(supported);
    if mask == DndAction::None {
        return (DndAction::None, DndAction::None);
    }
    let single = match preferred {
        Some(DragAction::Copy) => DndAction::Copy,
        Some(DragAction::Move) => DndAction::Move,
        _ => DndAction::None,
    };
    let chosen_preferred = if single != DndAction::None && mask.contains(single) {
        single
    } else if mask.contains(DndAction::Copy) {
        DndAction::Copy
    } else if mask.contains(DndAction::Move) {
        DndAction::Move
    } else {
        DndAction::None
    };
    (mask, chosen_preferred)
}

/// The paths a list of file addresses names.
///
/// The format is one address per line with comment lines, and only `file` addresses are paths at
/// all — a drag of a web address carries something no file system can open, and turning it into a
/// path would produce a name nothing can read rather than nothing. An address with a host other
/// than `localhost` names a file on another machine, so it is dropped too.
///
/// Read as bytes, because a Unix file name is bytes and need not be UTF-8.
pub fn paths(bytes: &[u8]) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::trim_ascii)
        .filter(|line| !line.is_empty() && !line.starts_with(b"#"))
        .filter_map(|line| line.strip_prefix(b"file://"))
        .filter_map(|address| {
            let path = address.strip_prefix(b"localhost").unwrap_or(address);
            if !path.starts_with(b"/") {
                return None;
            }
            let bytes = unescape(path);
            (!bytes.contains(&0)).then(|| PathBuf::from(std::ffi::OsString::from_vec(bytes)))
        })
        .collect()
}

/// One address with its percent escapes resolved.
fn unescape(bytes: &[u8]) -> Vec<u8> {
    let digit = |byte: u8| (byte as char).to_digit(16).map(|value| value as u8);
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (digit(bytes[index + 1]), digit(bytes[index + 2]))
        {
            out.push(high * 16 + low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{from_wayland_actions, paths, resolve_offer_actions};
    use smithay_client_toolkit::reexports::client::protocol::wl_data_device_manager::DndAction;
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use zgui_platform::{DragAction, DragActions};

    #[test]
    fn resolve_offer_actions_always_produces_valid_wayland_preferred_action() {
        assert_eq!(
            resolve_offer_actions(DragActions::NONE, Some(DragAction::Copy)),
            (DndAction::None, DndAction::None)
        );
        assert_eq!(
            resolve_offer_actions(
                DragActions::COPY | DragActions::MOVE,
                Some(DragAction::Move)
            ),
            (DndAction::Copy | DndAction::Move, DndAction::Move)
        );
        // Preferred Move when only Copy is supported falls back to Copy rather than violating the
        // protocol mask invariant.
        assert_eq!(
            resolve_offer_actions(DragActions::COPY, Some(DragAction::Move)),
            (DndAction::Copy, DndAction::Copy)
        );
        assert_eq!(
            from_wayland_actions(DndAction::Copy | DndAction::Move),
            DragActions::COPY | DragActions::MOVE
        );
    }

    #[test]
    fn a_utf8_name_survives_its_escapes() {
        assert_eq!(
            paths("file:///tmp/flight%20%C3%BC.ulg\nfile://localhost/tmp/日本.ulg\n".as_bytes()),
            vec![
                PathBuf::from("/tmp/flight ü.ulg"),
                PathBuf::from("/tmp/日本.ulg")
            ]
        );
    }

    #[test]
    fn a_name_that_is_not_utf8_keeps_its_bytes() {
        let name = |bytes: &[u8]| PathBuf::from(OsStr::from_bytes(bytes));
        assert_eq!(paths(b"file:///tmp/%FF.ulg"), vec![name(b"/tmp/\xFF.ulg")]);
        assert_eq!(paths(b"file:///tmp/\xFE.ulg"), vec![name(b"/tmp/\xFE.ulg")]);
    }

    #[test]
    fn a_split_character_after_a_percent_is_kept() {
        assert_eq!(
            paths("file:///tmp/%aé.ulg".as_bytes()),
            vec![PathBuf::from("/tmp/%aé.ulg")]
        );
        assert_eq!(
            paths("file:///tmp/%é.ulg".as_bytes()),
            vec![PathBuf::from("/tmp/%é.ulg")]
        );
    }

    #[test]
    fn a_file_on_another_host_is_dropped() {
        assert!(paths(b"file://remote/tmp/log.ulg\nfile://localhostfoo/tmp/log.ulg").is_empty());
    }

    #[test]
    fn a_name_with_an_escaped_nul_is_dropped() {
        assert!(paths(b"file:///tmp/bad%00.ulg").is_empty());
    }

    #[test]
    fn a_list_with_nothing_on_it_names_no_files() {
        assert!(paths(b"").is_empty());
        assert!(paths(b"# a comment and nothing else\r\n").is_empty());
    }

    #[test]
    fn every_address_on_the_list_becomes_a_path() {
        let list = b"file:///home/a/one.png\r\nfile:///home/a/two.png\r\n";
        assert_eq!(
            paths(list),
            vec![
                PathBuf::from("/home/a/one.png"),
                PathBuf::from("/home/a/two.png")
            ]
        );
    }

    #[test]
    fn something_that_is_not_a_file_is_dropped_rather_than_turned_into_a_name() {
        let mixed = b"https://example.invalid/a\r\nfile:///home/a/one.png\r\n";
        assert_eq!(paths(mixed), vec![PathBuf::from("/home/a/one.png")]);
    }

    #[test]
    fn an_escaped_name_comes_back_as_the_name_it_stands_for() {
        let escaped = b"file:///home/a/two%20words%21.txt\r\n";
        assert_eq!(
            paths(escaped),
            vec![PathBuf::from("/home/a/two words!.txt")]
        );
    }

    #[test]
    fn an_escape_that_is_not_one_is_left_alone_rather_than_dropping_the_character() {
        let odd = b"file:///home/a/100%.txt\r\n";
        assert_eq!(paths(odd), vec![PathBuf::from("/home/a/100%.txt")]);
    }
}
