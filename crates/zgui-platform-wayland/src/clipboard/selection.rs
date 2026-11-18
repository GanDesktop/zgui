//! What is on each clipboard, and what this application has put there.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use smithay_client_toolkit::data_device_manager::DataDeviceManagerState;
use smithay_client_toolkit::data_device_manager::data_device::DataDevice;
use smithay_client_toolkit::data_device_manager::data_source::CopyPasteSource;
use smithay_client_toolkit::data_device_manager::{ReadPipe, WritePipe};
use smithay_client_toolkit::primary_selection::PrimarySelectionManagerState;
use smithay_client_toolkit::primary_selection::device::PrimarySelectionDevice;
use smithay_client_toolkit::primary_selection::selection::PrimarySelectionSource;
use smithay_client_toolkit::reexports::client::Connection;
use smithay_client_toolkit::reexports::client::QueueHandle;
use smithay_client_toolkit::reexports::client::protocol::wl_data_source::WlDataSource;
use smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1;
use zgui_platform::{
    ClipboardData, ClipboardError, ClipboardFormat, ClipboardKind, ClipboardMime,
    ClipboardMimeOffer, ClipboardSerial, Waker,
};

use crate::clipboard::{deliver, deliver_mime, mime, pipe};
use crate::driver::WaylandState;

/// The devices, the offers on them, and what this application owns.
///
/// Shared and thread-safe because a read is answered on a thread of its own and a write can be
/// asked for from anywhere. The loop writes the device half; every request reads it.
#[derive(Debug, Default)]
pub struct Selections {
    /// Everything under one lock, because a read consults all of it at once.
    inner: Mutex<Inner>,
}

/// The changing half.
#[derive(Default)]
struct Inner {
    /// The connection every request is flushed on.
    conn: Option<Connection>,
    /// The queue the sources this application makes are dispatched on.
    qh: Option<QueueHandle<WaylandState>>,
    /// Where an answer to a read is delivered.
    waker: Option<Arc<dyn Waker>>,
    /// The standard clipboard's device and manager.
    standard: Option<Standard>,
    /// The selection clipboard's, where the compositor offers one.
    primary: Option<Primary>,
    /// The serial a claim on a selection may quote.
    serial: Option<u32>,
    /// Whether this client currently holds pointer, keyboard, or touch focus.
    focused: bool,
    /// Monotonic revision per clipboard kind (`Standard`, `Primary`).
    revisions: [u64; 2],
    /// Expects the compositor's own selection echo for the indexed clipboard.
    awaiting_own_selection: [bool; 2],
    /// What this application has put on each clipboard.
    held: [Option<Held>; 2],
}

/// The ordinary clipboard's objects.
#[derive(Debug)]
struct Standard {
    /// The factory for sources.
    manager: DataDeviceManagerState,
    /// This seat's device.
    device: DataDevice,
}

/// The selection clipboard's objects.
#[derive(Debug)]
struct Primary {
    /// The factory for sources.
    manager: PrimarySelectionManagerState,
    /// This seat's device.
    device: PrimarySelectionDevice,
}

/// A selection this application owns, and the source that serves it.
#[derive(Debug)]
struct Held {
    /// Offered payloads keyed by advertised media type.
    entries: BTreeMap<String, Vec<u8>>,
    /// The source serving these payloads, kept alive for as long as the selection is owned.
    source: Source,
}

/// The source object, whichever clipboard it belongs to.
#[derive(Debug)]
enum Source {
    /// A source on the ordinary clipboard.
    Standard(CopyPasteSource),
    /// A source on the selection clipboard.
    Primary(PrimarySelectionSource),
}

/// A read that has been asked for and not yet answered.
#[derive(Debug)]
pub struct Pending {
    /// The pipe the other process writes into.
    reader: ReadPipe,
    /// Which clipboard it came from, for the error that says it was empty.
    kind: ClipboardKind,
    /// The format expected from the bytes.
    format: ClipboardFormat,
    /// The MIME type that was requested from the offer.
    mime_type: String,
}

impl Pending {
    /// Waits for the answer here, up to `patience`.
    pub fn answer_now(
        self,
        patience: std::time::Duration,
    ) -> Result<ClipboardData, ClipboardError> {
        finish(
            self.kind,
            self.format,
            &self.mime_type,
            pipe::read(self.reader, patience),
        )
    }

    /// Waits for the answer on a thread of its own, and delivers it to the loop.
    ///
    /// A thread rather than a source on the loop, because the wait is on another process and the
    /// point of this backend is that the loop's own thread never waits on one.
    pub fn answer_on_a_thread(self, selections: Arc<Selections>, serial: ClipboardSerial) {
        let kind = self.kind;
        let format = self.format;
        let mime_type = self.mime_type;
        let answering = Arc::clone(&selections);
        let started = std::thread::Builder::new()
            .name("zgui-clipboard".to_owned())
            .spawn(move || {
                let answer = finish(
                    kind,
                    format,
                    &mime_type,
                    pipe::read(self.reader, pipe::PATIENCE),
                );
                answering.answer(serial, answer);
            });
        if let Err(error) = started {
            // Nobody is going to answer, so the request is answered here instead: a read that is
            // never answered leaves whatever asked for it waiting for ever.
            selections.answer(serial, Err(ClipboardError::Backend(error.to_string())));
        }
    }
}

/// A multi-MIME read from a single selection offer.
#[derive(Debug)]
pub struct PendingMime {
    /// Observed offer revision when the pipes were opened.
    revision: u64,
    /// Maximum total payload bytes permitted across matched entries.
    max_bytes: usize,
    /// Requested MIME names paired with the pipes serving them.
    readers: Vec<(String, ReadPipe)>,
}

impl PendingMime {
    /// Waits for all matched MIME pipes, up to `patience`.
    pub fn answer_now(
        self,
        patience: std::time::Duration,
    ) -> Result<ClipboardMimeOffer, ClipboardError> {
        finish_mime(self.revision, self.max_bytes, self.readers, patience)
    }

    /// Waits for all matched MIME pipes on a worker thread, then wakes the loop.
    pub fn answer_on_a_thread(self, selections: Arc<Selections>, serial: ClipboardSerial) {
        if self.readers.is_empty() {
            selections.answer_mime(
                serial,
                Ok(ClipboardMimeOffer {
                    revision: self.revision,
                    entries: Vec::new(),
                }),
            );
            return;
        }
        let answering = Arc::clone(&selections);
        let started = std::thread::Builder::new()
            .name("zgui-clipboard".to_owned())
            .spawn(move || {
                let answer =
                    finish_mime(self.revision, self.max_bytes, self.readers, pipe::PATIENCE);
                answering.answer_mime(serial, answer);
            });
        if let Err(error) = started {
            selections.answer_mime(serial, Err(ClipboardError::Backend(error.to_string())));
        }
    }
}

/// What a completed single-format transfer means.
fn finish(
    kind: ClipboardKind,
    format: ClipboardFormat,
    mime_type: &str,
    taken: Result<Vec<u8>, pipe::Failed>,
) -> Result<ClipboardData, ClipboardError> {
    let bytes = taken?;
    if bytes.is_empty() {
        return Err(ClipboardError::Empty(kind));
    }
    Ok(match format {
        ClipboardFormat::FileList => ClipboardData::FileList(mime::decode_uri_list(&bytes)),
        ClipboardFormat::Image => ClipboardData::Image {
            media_type: mime_type.to_owned().into(),
            bytes,
        },
        _ => {
            // Lossy rather than refused: a selection whose owner declared UTF-8 and wrote
            // something else is still mostly readable.
            let text = String::from_utf8_lossy(&bytes).into_owned();
            ClipboardData::Text(text.into())
        }
    })
}

/// Completes a multi-MIME transfer within one shared deadline and byte budget.
fn finish_mime(
    revision: u64,
    max_bytes: usize,
    readers: Vec<(String, ReadPipe)>,
    patience: std::time::Duration,
) -> Result<ClipboardMimeOffer, ClipboardError> {
    let deadline = Instant::now() + patience;
    let mut total_bytes = 0_usize;
    let mut entries = Vec::with_capacity(readers.len());
    for (mime_type, reader) in readers {
        let remaining = max_bytes.saturating_sub(total_bytes);
        let bytes =
            pipe::read_until(reader, deadline, remaining).map_err(|failed| match failed {
                pipe::Failed::TooLarge => {
                    ClipboardError::Backend(format!("clipboard payload exceeds {max_bytes} bytes"))
                }
                other => ClipboardError::from(other),
            })?;
        total_bytes = total_bytes.saturating_add(bytes.len());
        entries.push(ClipboardMime { mime_type, bytes });
    }
    Ok(ClipboardMimeOffer { revision, entries })
}

impl core::fmt::Debug for Inner {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("Selections")
            .field("standard", &self.standard.is_some())
            .field("primary", &self.primary.is_some())
            .field(
                "owns",
                &self.held.iter().filter(|held| held.is_some()).count(),
            )
            .finish_non_exhaustive()
    }
}

impl Selections {
    /// Records the connection and the queue everything is made on.
    pub(crate) fn attach(
        &self,
        conn: Connection,
        qh: QueueHandle<WaylandState>,
        waker: Arc<dyn Waker>,
    ) {
        let mut inner = self.lock();
        inner.conn = Some(conn);
        inner.qh = Some(qh);
        inner.waker = Some(waker);
    }

    /// Records this seat's devices.
    pub(crate) fn devices(
        &self,
        standard: Option<(DataDeviceManagerState, DataDevice)>,
        primary: Option<(PrimarySelectionManagerState, PrimarySelectionDevice)>,
    ) {
        let mut inner = self.lock();
        inner.standard = standard.map(|(manager, device)| Standard { manager, device });
        inner.primary = primary.map(|(manager, device)| Primary { manager, device });
    }

    /// Records the serial a claim on a selection may quote.
    ///
    /// Any input serial, not only a press. A pop-up grab and an interactive drag are declined
    /// against anything but a press, but a selection is not: the protocol asks for the serial of
    /// the event that caused the request, and a copy caused by the window merely being focused —
    /// a menu command, a shortcut on a freshly focused window — has only that one to give. Being
    /// stricter here means a copy that silently does nothing until the user has clicked.
    pub(crate) fn observed(&self, serial: u32) {
        self.lock().serial = Some(serial);
    }

    /// Records whether any surface in this client holds seat focus.
    pub(crate) fn set_focused(&self, focused: bool) {
        self.lock().focused = focused;
    }

    /// Notes that the compositor announced a new selection on `kind`.
    pub(crate) fn selection_changed(&self, kind: ClipboardKind) {
        let mut inner = self.lock();
        let idx = index(kind);
        let has_offer = match kind {
            ClipboardKind::Primary => inner
                .primary
                .as_ref()
                .and_then(|primary| primary.device.data().selection_offer())
                .is_some(),
            _ => inner
                .standard
                .as_ref()
                .and_then(|standard| standard.device.data().selection_offer())
                .is_some(),
        };
        if inner.awaiting_own_selection[idx] && (inner.held[idx].is_some() || !has_offer) {
            inner.awaiting_own_selection[idx] = false;
            return;
        }
        inner.awaiting_own_selection[idx] = false;
        inner.revisions[idx] = inner.revisions[idx].saturating_add(1);
    }

    /// Reads `format` directly from the selection owned by this application, if any.
    pub fn owned_data(
        &self,
        kind: ClipboardKind,
        format: ClipboardFormat,
    ) -> Option<Result<ClipboardData, ClipboardError>> {
        let inner = self.lock();
        let held = inner.held[index(kind)].as_ref()?;
        let keys: Vec<String> = held.entries.keys().cloned().collect();
        let result = match format {
            ClipboardFormat::Text => match mime::best_text(&keys) {
                Some(key) => {
                    let bytes = lookup_entry(&held.entries, &key).unwrap_or(&[]);
                    Ok(ClipboardData::Text(
                        String::from_utf8_lossy(bytes).into_owned().into(),
                    ))
                }
                None => Err(ClipboardError::UnavailableFormat(format)),
            },
            ClipboardFormat::FileList => {
                match keys
                    .iter()
                    .find(|key| key.eq_ignore_ascii_case(mime::URI_LIST))
                {
                    Some(key) => {
                        let bytes = lookup_entry(&held.entries, key).unwrap_or(&[]);
                        Ok(ClipboardData::FileList(mime::decode_uri_list(bytes)))
                    }
                    None => Err(ClipboardError::UnavailableFormat(format)),
                }
            }
            ClipboardFormat::Image => match mime::best_image(&keys) {
                Some(key) => {
                    let bytes = lookup_entry(&held.entries, &key).unwrap_or(&[]).to_vec();
                    Ok(ClipboardData::Image {
                        media_type: key.into(),
                        bytes,
                    })
                }
                None => Err(ClipboardError::UnavailableFormat(format)),
            },
            _ => Err(ClipboardError::UnsupportedFormat(format)),
        };
        Some(result)
    }

    /// Reads requested `mime_types` directly from the selection owned by this application, if any.
    pub(crate) fn owned_mime(
        &self,
        kind: ClipboardKind,
        mime_types: &[String],
        max_bytes: usize,
    ) -> Option<Result<ClipboardMimeOffer, ClipboardError>> {
        let inner = self.lock();
        let idx = index(kind);
        let held = inner.held[idx].as_ref()?;
        let revision = inner.revisions[idx];
        let mut total_bytes = 0_usize;
        let mut entries = Vec::new();
        for wanted in mime_types {
            if let Some(bytes) = lookup_entry(&held.entries, wanted) {
                total_bytes = total_bytes.saturating_add(bytes.len());
                if total_bytes > max_bytes {
                    return Some(Err(ClipboardError::Backend(format!(
                        "clipboard payload exceeds {max_bytes} bytes"
                    ))));
                }
                entries.push(ClipboardMime {
                    mime_type: wanted.clone(),
                    bytes: bytes.to_vec(),
                });
            }
        }
        Some(Ok(ClipboardMimeOffer { revision, entries }))
    }

    /// Gives up ownership of `kind`, because the compositor gave it to somebody else.
    pub(crate) fn lost(&self, source: &WlDataSource) {
        let mut inner = self.lock();
        for idx in 0..inner.held.len() {
            let matches_source = matches!(
                inner.held[idx].as_ref().map(|held| &held.source),
                Some(Source::Standard(owned)) if owned.inner() == source
            );
            if matches_source {
                inner.held[idx] = None;
                inner.awaiting_own_selection[idx] = false;
                inner.revisions[idx] = inner.revisions[idx].saturating_add(1);
            }
        }
    }

    /// The same, for the selection clipboard.
    pub(crate) fn lost_primary(&self, source: &ZwpPrimarySelectionSourceV1) {
        let mut inner = self.lock();
        for idx in 0..inner.held.len() {
            let matches_source = matches!(
                inner.held[idx].as_ref().map(|held| &held.source),
                Some(Source::Primary(owned)) if owned.inner() == source
            );
            if matches_source {
                inner.held[idx] = None;
                inner.awaiting_own_selection[idx] = false;
                inner.revisions[idx] = inner.revisions[idx].saturating_add(1);
            }
        }
    }

    /// Serves a request for what this application put on a clipboard.
    ///
    /// On a thread, because the destination is another process and a destination that stops
    /// reading would otherwise hold the loop for as long as it liked.
    pub(crate) fn serve(&self, source: &WlDataSource, mime: &str, destination: WritePipe) {
        let bytes = self.lock().held.iter().flatten().find_map(|held| {
            matches!(&held.source, Source::Standard(owned) if owned.inner() == source)
                .then(|| lookup_entry(&held.entries, mime).map(<[u8]>::to_vec))
                .flatten()
        });
        serve_bytes(bytes, destination);
    }

    /// The same, for the selection clipboard.
    pub(crate) fn serve_primary(
        &self,
        source: &ZwpPrimarySelectionSourceV1,
        mime: &str,
        destination: WritePipe,
    ) {
        let bytes = self.lock().held.iter().flatten().find_map(|held| {
            matches!(&held.source, Source::Primary(owned) if owned.inner() == source)
                .then(|| lookup_entry(&held.entries, mime).map(<[u8]>::to_vec))
                .flatten()
        });
        serve_bytes(bytes, destination);
    }

    /// Asks the owner of `kind` for `format`, answering with the pipe it will write into.
    pub(crate) fn start_read(
        &self,
        kind: ClipboardKind,
        format: ClipboardFormat,
    ) -> Result<Pending, ClipboardError> {
        let pick_mime = |offered: &[String]| -> Option<String> {
            match format {
                ClipboardFormat::Text => mime::best_text(offered),
                ClipboardFormat::FileList => mime::has_files(offered).then(|| {
                    mime::match_offered(offered, mime::URI_LIST)
                        .unwrap_or(mime::URI_LIST)
                        .to_owned()
                }),
                ClipboardFormat::Image => mime::best_image(offered),
                _ => None,
            }
        };
        let inner = self.lock();
        let (wanted, reader) = match kind {
            ClipboardKind::Primary => {
                let primary = inner
                    .primary
                    .as_ref()
                    .ok_or(ClipboardError::UnsupportedKind(kind))?;
                let offer = primary
                    .device
                    .data()
                    .selection_offer()
                    .ok_or(ClipboardError::Empty(kind))?;
                let wanted = offer
                    .with_mime_types(pick_mime)
                    .ok_or(ClipboardError::UnavailableFormat(format))?;
                let reader = offer
                    .receive(wanted.clone())
                    .map_err(|error| ClipboardError::Backend(error.to_string()))?;
                (wanted, reader)
            }
            _ => {
                let standard = inner.standard.as_ref().ok_or(ClipboardError::Empty(kind))?;
                let offer = standard
                    .device
                    .data()
                    .selection_offer()
                    .ok_or(ClipboardError::Empty(kind))?;
                let wanted = offer
                    .with_mime_types(pick_mime)
                    .ok_or(ClipboardError::UnavailableFormat(format))?;
                let reader = offer
                    .receive(wanted.clone())
                    .map_err(|error| ClipboardError::Backend(error.to_string()))?;
                (wanted, reader)
            }
        };
        // The request has to reach the compositor before anything waits on the pipe, and the loop
        // is not going to flush until it next parks — which is after this returns.
        if let Some(conn) = &inner.conn {
            let _ = conn.flush();
        }
        Ok(Pending {
            reader,
            kind,
            format,
            mime_type: wanted,
        })
    }

    /// Asks the owner of `kind` for any matching `mime_types` from the current offer.
    pub(crate) fn start_read_mime(
        &self,
        kind: ClipboardKind,
        mime_types: &[String],
        max_bytes: usize,
    ) -> Result<PendingMime, ClipboardError> {
        let inner = self.lock();
        let revision = inner.revisions[index(kind)];
        let collect_matches = |offered: &[String]| -> Vec<(String, String)> {
            mime_types
                .iter()
                .filter_map(|wanted| {
                    mime::match_offered(offered, wanted)
                        .map(|matched| (wanted.clone(), matched.to_owned()))
                })
                .collect()
        };
        let readers = match kind {
            ClipboardKind::Primary => {
                let primary = inner
                    .primary
                    .as_ref()
                    .ok_or(ClipboardError::UnsupportedKind(kind))?;
                let offer = primary
                    .device
                    .data()
                    .selection_offer()
                    .ok_or(ClipboardError::Empty(kind))?;
                let matched = offer.with_mime_types(collect_matches);
                let mut readers = Vec::with_capacity(matched.len());
                for (wanted, offered_name) in matched {
                    let reader = offer
                        .receive(offered_name)
                        .map_err(|error| ClipboardError::Backend(error.to_string()))?;
                    readers.push((wanted, reader));
                }
                readers
            }
            _ => {
                let standard = inner.standard.as_ref().ok_or(ClipboardError::Empty(kind))?;
                let offer = standard
                    .device
                    .data()
                    .selection_offer()
                    .ok_or(ClipboardError::Empty(kind))?;
                let matched = offer.with_mime_types(collect_matches);
                let mut readers = Vec::with_capacity(matched.len());
                for (wanted, offered_name) in matched {
                    let reader = offer
                        .receive(offered_name)
                        .map_err(|error| ClipboardError::Backend(error.to_string()))?;
                    readers.push((wanted, reader));
                }
                readers
            }
        };
        if let Some(conn) = &inner.conn {
            let _ = conn.flush();
        }
        Ok(PendingMime {
            revision,
            max_bytes,
            readers,
        })
    }

    /// Claims `kind` for this application with `entries`, or gives it up when empty.
    pub(crate) fn take_selection_mime(
        &self,
        kind: ClipboardKind,
        entries: &[ClipboardMime],
        expected_revision: Option<u64>,
    ) -> Result<(), ClipboardError> {
        let mut inner = self.lock();
        let idx = index(kind);
        if !inner.focused || inner.serial.is_none() {
            return Err(ClipboardError::NotFocused(kind));
        }
        let serial = inner.serial.ok_or(ClipboardError::NotFocused(kind))?;
        if expected_revision.is_some_and(|expected| expected != inner.revisions[idx]) {
            return Err(ClipboardError::Changed(kind));
        }
        let qh = inner
            .qh
            .clone()
            .ok_or_else(|| ClipboardError::Backend("the loop has not started".to_owned()))?;

        let held = if entries.is_empty() {
            match kind {
                ClipboardKind::Primary => {
                    if let Some(primary) = &inner.primary {
                        primary.device.unset_selection(serial);
                    }
                }
                _ => {
                    if let Some(standard) = &inner.standard {
                        standard.device.unset_selection(serial);
                    }
                }
            }
            None
        } else {
            let mut map = BTreeMap::new();
            let mut offered_mimes = Vec::with_capacity(entries.len());
            for entry in entries {
                if !map.contains_key(&entry.mime_type) {
                    offered_mimes.push(entry.mime_type.as_str());
                }
                map.insert(entry.mime_type.clone(), entry.bytes.clone());
            }
            match kind {
                ClipboardKind::Primary => {
                    let primary = inner
                        .primary
                        .as_ref()
                        .ok_or(ClipboardError::UnsupportedKind(kind))?;
                    let source = primary.manager.create_selection_source(&qh, offered_mimes);
                    source.set_selection(&primary.device, serial);
                    Some(Held {
                        entries: map,
                        source: Source::Primary(source),
                    })
                }
                _ => {
                    let standard = inner.standard.as_ref().ok_or_else(|| {
                        ClipboardError::Backend("this compositor offers no clipboard".to_owned())
                    })?;
                    let source = standard
                        .manager
                        .create_copy_paste_source(&qh, offered_mimes);
                    source.set_selection(&standard.device, serial);
                    Some(Held {
                        entries: map,
                        source: Source::Standard(source),
                    })
                }
            }
        };
        // Replaced rather than dropped first: dropping the old source destroys it, and destroying
        // the source that currently owns the selection before the new one has claimed it leaves a
        // gap in which the clipboard is empty.
        inner.revisions[idx] = inner.revisions[idx].saturating_add(1);
        inner.awaiting_own_selection[idx] = true;
        inner.held[idx] = held;
        if let Some(conn) = &inner.conn {
            let _ = conn.flush();
        }
        Ok(())
    }

    /// Delivers a single-format answer to whoever asked for it.
    pub(crate) fn answer(
        &self,
        serial: ClipboardSerial,
        result: Result<ClipboardData, ClipboardError>,
    ) {
        let waker = self.lock().waker.clone();
        deliver(waker.as_ref(), serial, result);
    }

    /// Delivers a multi-MIME answer to whoever asked for it.
    pub(crate) fn answer_mime(
        &self,
        serial: ClipboardSerial,
        result: Result<ClipboardMimeOffer, ClipboardError>,
    ) {
        let waker = self.lock().waker.clone();
        deliver_mime(waker.as_ref(), serial, result);
    }

    /// The state, recovering from a panic on another thread.
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn lookup_entry<'a>(entries: &'a BTreeMap<String, Vec<u8>>, wanted: &str) -> Option<&'a [u8]> {
    entries
        .get(wanted)
        .or_else(|| {
            entries
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(wanted))
                .map(|(_, value)| value)
        })
        .map(Vec::as_slice)
}

/// Writes a held selection into `destination`, on a thread.
fn serve_bytes(bytes: Option<Vec<u8>>, destination: WritePipe) {
    let _ = std::thread::Builder::new()
        .name("zgui-clipboard-write".to_owned())
        .spawn(move || {
            // A destination asking for a selection this application no longer holds gets an empty
            // one: closing the pipe with nothing in it is how the protocol says "nothing here".
            pipe::write(
                destination,
                bytes.as_deref().unwrap_or_default(),
                pipe::PATIENCE,
            );
        });
}

/// Which slot a clipboard's state is kept in.
const fn index(kind: ClipboardKind) -> usize {
    match kind {
        ClipboardKind::Primary => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{finish, index};
    use crate::clipboard::pipe::Failed;
    use zgui_platform::{ClipboardData, ClipboardError, ClipboardFormat, ClipboardKind};

    #[test]
    fn the_two_clipboards_are_kept_apart() {
        // Writing the selection clipboard by mistake destroys whatever the user had selected
        // elsewhere, so the two must never share a slot.
        assert_ne!(
            index(ClipboardKind::Standard),
            index(ClipboardKind::Primary)
        );
    }

    #[test]
    fn an_empty_transfer_is_an_empty_clipboard_rather_than_a_failure() {
        assert_eq!(
            finish(
                ClipboardKind::Standard,
                ClipboardFormat::Text,
                "text/plain",
                Ok(Vec::new())
            ),
            Err(ClipboardError::Empty(ClipboardKind::Standard))
        );
    }

    #[test]
    fn what_arrived_is_what_is_answered_with() {
        assert_eq!(
            finish(
                ClipboardKind::Standard,
                ClipboardFormat::Text,
                "text/plain",
                Ok(b"copied".to_vec())
            ),
            Ok(ClipboardData::Text("copied".into()))
        );
    }

    #[test]
    fn text_that_is_not_valid_is_read_as_far_as_it_goes_rather_than_refused() {
        // A selection whose owner declared UTF-8 and wrote something else is still mostly
        // readable, and refusing the paste entirely helps nobody.
        let answered = finish(
            ClipboardKind::Standard,
            ClipboardFormat::Text,
            "text/plain",
            Ok(vec![b'o', b'k', 0xff]),
        );
        assert!(matches!(answered, Ok(ClipboardData::Text(_))));
    }

    #[test]
    fn an_owner_that_never_answered_is_reported_as_a_timeout() {
        assert_eq!(
            finish(
                ClipboardKind::Primary,
                ClipboardFormat::Text,
                "text/plain",
                Err(Failed::TimedOut)
            ),
            Err(ClipboardError::TimedOut)
        );
    }

    #[test]
    fn write_mime_requires_focus_before_taking_selection() {
        let selections = super::Selections::default();
        assert_eq!(
            selections.take_selection_mime(
                ClipboardKind::Standard,
                &[zgui_platform::ClipboardMime::new("text/plain", b"hi")],
                None,
            ),
            Err(ClipboardError::NotFocused(ClipboardKind::Standard))
        );
    }

    #[test]
    fn selection_changed_bumps_revision_except_on_own_echo() {
        let selections = super::Selections::default();
        assert_eq!(selections.lock().revisions, [0, 0]);
        selections.selection_changed(ClipboardKind::Standard);
        assert_eq!(selections.lock().revisions, [1, 0]);
        selections.selection_changed(ClipboardKind::Primary);
        assert_eq!(selections.lock().revisions, [1, 1]);
    }
}
