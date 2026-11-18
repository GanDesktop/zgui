//! Safe multi-MIME X11 clipboard backend built on `x11rb` and `XFixes`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::Event;
use x11rb::protocol::xfixes::{ConnectionExt as _, SelectionEventMask};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux, EventMask,
    PropMode, Property, SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent,
    Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use zgui_platform::{
    ClipboardData, ClipboardError, ClipboardFormat, ClipboardKind, ClipboardMime,
    ClipboardMimeOffer,
};

use crate::clipboard::mime;

/// Patience for asynchronous background clipboard transfers.
pub(crate) const PATIENCE: Duration = Duration::from_secs(4);
/// Patience for synchronous foreground clipboard reads.
pub(crate) const IMPATIENCE: Duration = Duration::from_millis(250);
/// Default size ceiling for single-format reads.
const LEGACY_LIMIT: usize = 32 * 1024 * 1024;
/// Fallback X11 chunk ceiling when transferring properties via INCR.
const INCR_CHUNK_FALLBACK: usize = 256 * 1024;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        CLIPBOARD,
        PRIMARY,
        TARGETS,
        MULTIPLE,
        TIMESTAMP,
        ATOM,
        ATOM_PAIR,
        INCR,
        INTEGER,
        UTF8_STRING,
        STRING,
        TEXT,
        _ZGUI_CLIPBOARD_PROP,
    }
}

struct Held {
    /// Ordered `(atom, canonical_mime_name, bytes)` entries served by this owner.
    entries: Vec<(Atom, String, Vec<u8>)>,
    timestamp: u32,
}

struct IncrWrite {
    target: Atom,
    bytes: Vec<u8>,
    offset: usize,
    chunk_size: usize,
}

struct State {
    revisions: [u64; 2],
    held: [Option<Held>; 2],
    awaiting_own_owner: [bool; 2],
    last_owner: [Window; 2],
    has_xfixes: bool,
    atom_to_name: HashMap<Atom, String>,
    name_to_atom: HashMap<String, Atom>,
    incr_writes: HashMap<(Window, Atom), IncrWrite>,
}

struct Shared {
    conn: RustConnection,
    win_id: Window,
    atoms: Atoms,
    state: Mutex<State>,
    reader: Mutex<Option<ReaderConn>>,
}

struct ReaderConn {
    conn: RustConnection,
    win_id: Window,
    atoms: Atoms,
}

/// X11 multi-MIME clipboard backend, opened lazily on first clipboard access.
#[derive(Default)]
pub(crate) struct X11 {
    slot: Mutex<Option<LiveX11>>,
}

struct LiveX11 {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<()>>,
}

impl Drop for LiveX11 {
    fn drop(&mut self) {
        let _ = self.shared.conn.destroy_window(self.shared.win_id);
        let _ = self.shared.conn.flush();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl X11 {
    /// Reads `format` from X11 `kind` selection.
    pub(crate) fn read(
        &self,
        kind: ClipboardKind,
        format: ClipboardFormat,
        patience: Duration,
    ) -> Result<ClipboardData, ClipboardError> {
        let shared = self.ensure_connected()?;
        shared.read_single(kind, format, patience)
    }

    /// Reads matching `mime_types` atomically from X11 `kind` selection.
    pub(crate) fn read_mime(
        &self,
        kind: ClipboardKind,
        mime_types: &[String],
        max_bytes: usize,
        patience: Duration,
    ) -> Result<ClipboardMimeOffer, ClipboardError> {
        let shared = self.ensure_connected()?;
        shared.read_mime(kind, mime_types, max_bytes, patience)
    }

    /// Replaces X11 `kind` selection with `entries`, validating `expected_revision` when provided.
    pub(crate) fn write_mime(
        &self,
        kind: ClipboardKind,
        entries: &[ClipboardMime],
        expected_revision: Option<u64>,
    ) -> Result<(), ClipboardError> {
        let shared = self.ensure_connected()?;
        shared.write_mime(kind, entries, expected_revision)
    }

    fn ensure_connected(&self) -> Result<Arc<Shared>, ClipboardError> {
        let mut guard = self
            .slot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(live) = guard.as_ref() {
            return Ok(Arc::clone(&live.shared));
        }

        let (conn, screen_num) = RustConnection::connect(None)
            .map_err(|err| ClipboardError::Backend(format!("x11 connect failed: {err}")))?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| ClipboardError::Backend("x11 screen missing".to_owned()))?;
        let win_id = conn
            .generate_id()
            .map_err(|err| ClipboardError::Backend(err.to_string()))?;
        let mask = EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win_id,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::COPY_FROM_PARENT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(mask),
        )
        .map_err(|err| ClipboardError::Backend(err.to_string()))?;

        let atoms = Atoms::new(&conn)
            .map_err(|err| ClipboardError::Backend(err.to_string()))?
            .reply()
            .map_err(|err| ClipboardError::Backend(err.to_string()))?;

        let has_xfixes = conn
            .xfixes_query_version(5, 0)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some();
        if has_xfixes {
            let mask = SelectionEventMask::SET_SELECTION_OWNER
                | SelectionEventMask::SELECTION_WINDOW_DESTROY
                | SelectionEventMask::SELECTION_CLIENT_CLOSE;
            let _ = conn.xfixes_select_selection_input(win_id, atoms.CLIPBOARD, mask);
            let _ = conn.xfixes_select_selection_input(win_id, atoms.PRIMARY, mask);
        }
        let _ = conn.flush();

        let shared = Arc::new(Shared {
            conn,
            win_id,
            atoms,
            state: Mutex::new(State {
                revisions: [0, 0],
                held: [None, None],
                awaiting_own_owner: [false, false],
                last_owner: [x11rb::NONE, x11rb::NONE],
                has_xfixes,
                atom_to_name: HashMap::new(),
                name_to_atom: HashMap::new(),
                incr_writes: HashMap::new(),
            }),
            reader: Mutex::new(None),
        });
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("zgui-x11-clipboard".to_owned())
            .spawn(move || serve_loop(worker_shared))
            .ok();
        let ret = Arc::clone(&shared);
        *guard = Some(LiveX11 { shared, worker });
        Ok(ret)
    }
}

impl ReaderConn {
    fn open() -> Result<Self, ClipboardError> {
        let (conn, screen_num) = RustConnection::connect(None)
            .map_err(|err| ClipboardError::Backend(format!("x11 connect failed: {err}")))?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| ClipboardError::Backend("x11 screen missing".to_owned()))?;
        let win_id = conn
            .generate_id()
            .map_err(|err| ClipboardError::Backend(err.to_string()))?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            win_id,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::COPY_FROM_PARENT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new()
                .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY),
        )
        .map_err(|err| ClipboardError::Backend(err.to_string()))?;
        let atoms = Atoms::new(&conn)
            .map_err(|err| ClipboardError::Backend(err.to_string()))?
            .reply()
            .map_err(|err| ClipboardError::Backend(err.to_string()))?;
        let _ = conn.flush();
        Ok(Self {
            conn,
            win_id,
            atoms,
        })
    }
}

impl Shared {
    fn selection_atom(&self, kind: ClipboardKind) -> Atom {
        match kind {
            ClipboardKind::Primary => self.atoms.PRIMARY,
            _ => self.atoms.CLIPBOARD,
        }
    }

    fn selection_index(&self, selection: Atom) -> Option<usize> {
        if selection == self.atoms.CLIPBOARD {
            Some(0)
        } else if selection == self.atoms.PRIMARY {
            Some(1)
        } else {
            None
        }
    }

    fn intern_atom(&self, name: &str) -> Result<Atom, ClipboardError> {
        {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(&atom) = state.name_to_atom.get(name) {
                return Ok(atom);
            }
        }
        let reply = self
            .conn
            .intern_atom(false, name.as_bytes())
            .map_err(|err| ClipboardError::Backend(err.to_string()))?
            .reply()
            .map_err(|err| ClipboardError::Backend(err.to_string()))?;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.name_to_atom.insert(name.to_owned(), reply.atom);
        state.atom_to_name.insert(reply.atom, name.to_owned());
        Ok(reply.atom)
    }

    fn atom_name(&self, atom: Atom) -> Option<String> {
        {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(name) = state.atom_to_name.get(&atom) {
                return Some(name.clone());
            }
        }
        let reply = self.conn.get_atom_name(atom).ok()?.reply().ok()?;
        let name = String::from_utf8_lossy(&reply.name).into_owned();
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.atom_to_name.insert(atom, name.clone());
        state.name_to_atom.insert(name.clone(), atom);
        Some(name)
    }

    fn sync_owner(&self, kind: ClipboardKind) -> Result<(Window, u64), ClipboardError> {
        let idx = kind_index(kind);
        let owner = self
            .conn
            .get_selection_owner(self.selection_atom(kind))
            .map_err(|err| ClipboardError::Backend(err.to_string()))?
            .reply()
            .map_err(|err| ClipboardError::Backend(err.to_string()))?
            .owner;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.has_xfixes && owner != state.last_owner[idx] {
            state.revisions[idx] = state.revisions[idx].saturating_add(1);
        }
        state.last_owner[idx] = owner;
        if owner != self.win_id && state.held[idx].is_some() {
            state.held[idx] = None;
        }
        Ok((owner, state.revisions[idx]))
    }

    fn write_mime(
        &self,
        kind: ClipboardKind,
        entries: &[ClipboardMime],
        expected_revision: Option<u64>,
    ) -> Result<(), ClipboardError> {
        let idx = kind_index(kind);
        let selection_atom = self.selection_atom(kind);
        let current_owner = self
            .conn
            .get_selection_owner(selection_atom)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|r| r.owner)
            .unwrap_or(x11rb::NONE);

        let mut prepared = Vec::with_capacity(entries.len());
        if !entries.is_empty() {
            for entry in entries {
                let atom = self.intern_atom(&entry.mime_type)?;
                prepared.push((atom, entry.mime_type.clone(), entry.bytes.clone()));
            }
        }

        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if !state.has_xfixes && current_owner != state.last_owner[idx] {
            state.revisions[idx] = state.revisions[idx].saturating_add(1);
            state.last_owner[idx] = current_owner;
        }
        if expected_revision.is_some_and(|expected| expected != state.revisions[idx]) {
            return Err(ClipboardError::Changed(kind));
        }

        if entries.is_empty() {
            state.held[idx] = None;
            state.awaiting_own_owner[idx] = true;
            state.revisions[idx] = state.revisions[idx].saturating_add(1);
            state.last_owner[idx] = x11rb::NONE;
            self.conn
                .set_selection_owner(x11rb::NONE, selection_atom, x11rb::CURRENT_TIME)
                .map_err(|err| ClipboardError::Backend(err.to_string()))?;
        } else {
            state.held[idx] = Some(Held {
                entries: prepared,
                timestamp: 0,
            });
            state.awaiting_own_owner[idx] = true;
            state.revisions[idx] = state.revisions[idx].saturating_add(1);
            state.last_owner[idx] = self.win_id;
            self.conn
                .set_selection_owner(self.win_id, selection_atom, x11rb::CURRENT_TIME)
                .map_err(|err| ClipboardError::Backend(err.to_string()))?;
        }
        self.conn
            .flush()
            .map_err(|err| ClipboardError::Backend(err.to_string()))?;
        Ok(())
    }

    fn read_single(
        &self,
        kind: ClipboardKind,
        format: ClipboardFormat,
        patience: Duration,
    ) -> Result<ClipboardData, ClipboardError> {
        let (owner, _) = self.sync_owner(kind)?;
        if owner == x11rb::NONE {
            return Err(ClipboardError::Empty(kind));
        }
        if owner == self.win_id {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(held) = &state.held[kind_index(kind)] {
                let names: Vec<String> = held
                    .entries
                    .iter()
                    .map(|(_, name, _)| name.clone())
                    .collect();
                let wanted = mime::pick_for_format(&names, format)
                    .ok_or(ClipboardError::UnavailableFormat(format))?;
                let bytes = held
                    .entries
                    .iter()
                    .find(|(_, name, _)| name.eq_ignore_ascii_case(&wanted))
                    .map(|(_, _, bytes)| bytes.clone())
                    .unwrap_or_default();
                return Ok(mime::decode_for_format(format, &wanted, bytes));
            }
        }

        let deadline = Instant::now() + patience;
        let offered = self.query_targets(kind, deadline)?;
        let wanted = mime::pick_for_format(
            &offered.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(),
            format,
        )
        .ok_or(ClipboardError::UnavailableFormat(format))?;
        let atom = offered
            .into_iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&wanted))
            .map(|(_, atom)| atom)
            .ok_or(ClipboardError::UnavailableFormat(format))?;
        let bytes = self.read_target(kind, atom, LEGACY_LIMIT, deadline)?;
        if bytes.is_empty() {
            return Err(ClipboardError::Empty(kind));
        }
        Ok(mime::decode_for_format(format, &wanted, bytes))
    }

    fn read_mime(
        &self,
        kind: ClipboardKind,
        mime_types: &[String],
        max_bytes: usize,
        patience: Duration,
    ) -> Result<ClipboardMimeOffer, ClipboardError> {
        let (owner, revision) = self.sync_owner(kind)?;
        if owner == x11rb::NONE {
            return Err(ClipboardError::Empty(kind));
        }
        if owner == self.win_id {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(held) = &state.held[kind_index(kind)] {
                let mut total_bytes = 0_usize;
                let mut entries = Vec::new();
                for wanted in mime_types {
                    if let Some((_, _, bytes)) = held
                        .entries
                        .iter()
                        .find(|(_, name, _)| name == wanted)
                        .or_else(|| {
                            held.entries
                                .iter()
                                .find(|(_, name, _)| name.eq_ignore_ascii_case(wanted))
                        })
                    {
                        total_bytes = total_bytes.saturating_add(bytes.len());
                        if total_bytes > max_bytes {
                            return Err(ClipboardError::Backend(format!(
                                "clipboard payload exceeds {max_bytes} bytes"
                            )));
                        }
                        entries.push(ClipboardMime::new(wanted.clone(), bytes.clone()));
                    }
                }
                return Ok(ClipboardMimeOffer { revision, entries });
            }
        }

        let deadline = Instant::now() + patience;
        let offered = self.query_targets(kind, deadline)?;
        let offered_names: Vec<String> = offered.iter().map(|(name, _)| name.clone()).collect();
        let mut total_bytes = 0_usize;
        let mut entries = Vec::new();
        for wanted in mime_types {
            let Some(matched_name) = mime::match_offered(&offered_names, wanted) else {
                continue;
            };
            let Some((_, atom)) = offered.iter().find(|(name, _)| name == matched_name) else {
                continue;
            };
            let remaining = max_bytes.saturating_sub(total_bytes);
            let bytes = self.read_target(kind, *atom, remaining, deadline)?;
            total_bytes = total_bytes.saturating_add(bytes.len());
            entries.push(ClipboardMime::new(wanted.clone(), bytes));
        }
        Ok(ClipboardMimeOffer { revision, entries })
    }

    fn query_targets(
        &self,
        kind: ClipboardKind,
        deadline: Instant,
    ) -> Result<Vec<(String, Atom)>, ClipboardError> {
        let mut reader_guard = self.reader.lock().unwrap_or_else(|p| p.into_inner());
        if reader_guard.is_none() {
            *reader_guard = Some(ReaderConn::open()?);
        }
        let reader = reader_guard.as_ref().expect("reader initialized");
        let raw = match read_selection_property(
            reader,
            self.selection_atom(kind),
            self.atoms.TARGETS,
            LEGACY_LIMIT,
            deadline,
        ) {
            Ok(bytes) => bytes,
            Err(ClipboardError::Empty(_)) | Err(ClipboardError::TimedOut) => return Ok(Vec::new()),
            Err(err) => {
                *reader_guard = None;
                return Err(err);
            }
        };
        drop(reader_guard);

        let mut targets = Vec::new();
        for chunk in raw.as_chunks::<4>().0 {
            let atom = u32::from_ne_bytes(*chunk);
            if atom != x11rb::NONE
                && let Some(name) = self.atom_name(atom)
            {
                targets.push((name, atom));
            }
        }
        Ok(targets)
    }

    fn read_target(
        &self,
        kind: ClipboardKind,
        target: Atom,
        max_bytes: usize,
        deadline: Instant,
    ) -> Result<Vec<u8>, ClipboardError> {
        let mut reader_guard = self.reader.lock().unwrap_or_else(|p| p.into_inner());
        if reader_guard.is_none() {
            *reader_guard = Some(ReaderConn::open()?);
        }
        let reader = reader_guard.as_ref().expect("reader initialized");
        match read_selection_property(
            reader,
            self.selection_atom(kind),
            target,
            max_bytes,
            deadline,
        ) {
            Ok(bytes) => Ok(bytes),
            Err(err @ ClipboardError::Backend(_)) => {
                *reader_guard = None;
                Err(err)
            }
            Err(err) => Err(err),
        }
    }

    fn handle_selection_request(&self, ev: &SelectionRequestEvent) {
        let Some(idx) = self.selection_index(ev.selection) else {
            self.send_selection_notify(ev, x11rb::NONE);
            return;
        };
        let prop = if ev.property == x11rb::NONE {
            ev.target
        } else {
            ev.property
        };

        if ev.target == self.atoms.MULTIPLE && ev.property != x11rb::NONE {
            self.handle_multiple_request(idx, ev);
            return;
        }

        let succeeded = self.fulfill_target(idx, ev.requestor, prop, ev.target);
        self.send_selection_notify(ev, if succeeded { prop } else { x11rb::NONE });
    }

    fn handle_multiple_request(&self, idx: usize, ev: &SelectionRequestEvent) {
        let Ok(cookie) = self.conn.get_property(
            false,
            ev.requestor,
            ev.property,
            self.atoms.ATOM_PAIR,
            0,
            1024,
        ) else {
            self.send_selection_notify(ev, x11rb::NONE);
            return;
        };
        let Ok(reply) = cookie.reply() else {
            self.send_selection_notify(ev, x11rb::NONE);
            return;
        };
        let mut pairs: Vec<u32> = reply
            .value32()
            .map(|iter| iter.collect())
            .unwrap_or_default();
        if !pairs.len().is_multiple_of(2) {
            self.send_selection_notify(ev, x11rb::NONE);
            return;
        }
        for chunk in pairs.as_chunks_mut::<2>().0 {
            let target = chunk[0];
            let prop = chunk[1];
            if !self.fulfill_target(idx, ev.requestor, prop, target) {
                chunk[1] = x11rb::NONE;
            }
        }
        let _ = self.conn.change_property32(
            PropMode::REPLACE,
            ev.requestor,
            ev.property,
            self.atoms.ATOM_PAIR,
            &pairs,
        );
        let _ = self.conn.flush();
        self.send_selection_notify(ev, ev.property);
    }

    fn fulfill_target(&self, idx: usize, requestor: Window, prop: Atom, target: Atom) -> bool {
        let chunk_limit = self
            .conn
            .maximum_request_bytes()
            .saturating_sub(64)
            .clamp(1024, INCR_CHUNK_FALLBACK);

        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(held) = &state.held[idx] else {
            return false;
        };
        if target == self.atoms.TARGETS {
            let mut targets = vec![
                self.atoms.TARGETS,
                self.atoms.TIMESTAMP,
                self.atoms.MULTIPLE,
            ];
            for (atom, _, _) in &held.entries {
                if !targets.contains(atom) {
                    targets.push(*atom);
                }
            }
            if self
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    requestor,
                    prop,
                    self.atoms.ATOM,
                    &targets,
                )
                .is_err()
            {
                return false;
            }
            let _ = self.conn.flush();
            return true;
        }
        if target == self.atoms.TIMESTAMP {
            if self
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    requestor,
                    prop,
                    self.atoms.INTEGER,
                    &[held.timestamp],
                )
                .is_err()
            {
                return false;
            }
            let _ = self.conn.flush();
            return true;
        }
        let Some((_, _, bytes)) = held.entries.iter().find(|(atom, _, _)| *atom == target) else {
            return false;
        };
        let bytes = bytes.clone();
        if bytes.len() <= chunk_limit {
            if self
                .conn
                .change_property8(PropMode::REPLACE, requestor, prop, target, &bytes)
                .is_err()
            {
                return false;
            }
            let _ = self.conn.flush();
            true
        } else {
            let _ = self.conn.change_window_attributes(
                requestor,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            );
            let total = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
            if self
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    requestor,
                    prop,
                    self.atoms.INCR,
                    &[total],
                )
                .is_err()
            {
                return false;
            }
            let _ = self.conn.flush();
            state.incr_writes.insert(
                (requestor, prop),
                IncrWrite {
                    target,
                    bytes,
                    offset: 0,
                    chunk_size: chunk_limit,
                },
            );
            true
        }
    }

    fn send_selection_notify(&self, ev: &SelectionRequestEvent, property: Atom) {
        let notify = SelectionNotifyEvent {
            response_type: SELECTION_NOTIFY_EVENT,
            sequence: ev.sequence,
            time: ev.time,
            requestor: ev.requestor,
            selection: ev.selection,
            target: ev.target,
            property,
        };
        let _ = self
            .conn
            .send_event(false, ev.requestor, EventMask::NO_EVENT, notify);
        let _ = self.conn.flush();
    }

    fn handle_incr_delete(&self, window: Window, atom: Atom) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let Some(entry) = state.incr_writes.get_mut(&(window, atom)) else {
            return;
        };
        if entry.offset >= entry.bytes.len() {
            let target = entry.target;
            state.incr_writes.remove(&(window, atom));
            let _ = self
                .conn
                .change_property8(PropMode::REPLACE, window, atom, target, &[]);
            let _ = self.conn.flush();
            return;
        }
        let end = (entry.offset + entry.chunk_size).min(entry.bytes.len());
        let chunk = entry.bytes[entry.offset..end].to_vec();
        entry.offset = end;
        let target = entry.target;
        let _ = self
            .conn
            .change_property8(PropMode::REPLACE, window, atom, target, &chunk);
        let _ = self.conn.flush();
    }
}

fn serve_loop(shared: Arc<Shared>) {
    while let Ok(event) = shared.conn.wait_for_event() {
        match event {
            Event::DestroyNotify(ev) if ev.window == shared.win_id => break,
            Event::SelectionRequest(ev) => shared.handle_selection_request(&ev),
            Event::SelectionClear(ev) => {
                if let Some(idx) = shared.selection_index(ev.selection) {
                    let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
                    state.held[idx] = None;
                    if !state.has_xfixes {
                        state.revisions[idx] = state.revisions[idx].saturating_add(1);
                    }
                }
            }
            Event::XfixesSelectionNotify(ev) => {
                if let Some(idx) = shared.selection_index(ev.selection) {
                    let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
                    if ev.owner == shared.win_id && state.awaiting_own_owner[idx] {
                        state.awaiting_own_owner[idx] = false;
                    } else {
                        state.awaiting_own_owner[idx] = false;
                        state.revisions[idx] = state.revisions[idx].saturating_add(1);
                    }
                    state.last_owner[idx] = ev.owner;
                }
            }
            Event::PropertyNotify(ev) if ev.state == Property::DELETE => {
                shared.handle_incr_delete(ev.window, ev.atom);
            }
            _ => {}
        }
    }
}

fn read_selection_property(
    reader: &ReaderConn,
    selection: Atom,
    target: Atom,
    max_bytes: usize,
    deadline: Instant,
) -> Result<Vec<u8>, ClipboardError> {
    while reader
        .conn
        .poll_for_event()
        .map_err(|err| ClipboardError::Backend(err.to_string()))?
        .is_some()
    {}

    let prop = reader.atoms._ZGUI_CLIPBOARD_PROP;
    reader
        .conn
        .delete_property(reader.win_id, prop)
        .map_err(|err| ClipboardError::Backend(err.to_string()))?;
    reader
        .conn
        .convert_selection(reader.win_id, selection, target, prop, x11rb::CURRENT_TIME)
        .map_err(|err| ClipboardError::Backend(err.to_string()))?;
    reader
        .conn
        .flush()
        .map_err(|err| ClipboardError::Backend(err.to_string()))?;

    let max_words = u32::try_from((max_bytes / 4).saturating_add(4)).unwrap_or(u32::MAX);
    let mut incr_mode = false;
    let mut incr_bytes = Vec::new();

    loop {
        if Instant::now() >= deadline {
            return Err(ClipboardError::TimedOut);
        }
        match reader
            .conn
            .poll_for_event()
            .map_err(|err| ClipboardError::Backend(err.to_string()))?
        {
            None => std::thread::sleep(Duration::from_millis(1)),
            Some(Event::SelectionNotify(ev))
                if ev.selection == selection && ev.target == target =>
            {
                if ev.property == x11rb::NONE {
                    return Err(ClipboardError::Empty(
                        if selection == reader.atoms.PRIMARY {
                            ClipboardKind::Primary
                        } else {
                            ClipboardKind::Standard
                        },
                    ));
                }
                let reply = reader
                    .conn
                    .get_property(true, reader.win_id, prop, AtomEnum::ANY, 0, max_words)
                    .map_err(|err| ClipboardError::Backend(err.to_string()))?
                    .reply()
                    .map_err(|err| ClipboardError::Backend(err.to_string()))?;
                if reply.type_ == reader.atoms.INCR {
                    incr_mode = true;
                    let _ = reader.conn.flush();
                } else {
                    if reply.value.len() > max_bytes || reply.bytes_after > 0 {
                        return Err(ClipboardError::Backend(format!(
                            "clipboard payload exceeds {max_bytes} bytes"
                        )));
                    }
                    return Ok(reply.value);
                }
            }
            Some(Event::PropertyNotify(ev))
                if incr_mode
                    && ev.window == reader.win_id
                    && ev.atom == prop
                    && ev.state == Property::NEW_VALUE =>
            {
                let reply = reader
                    .conn
                    .get_property(true, reader.win_id, prop, AtomEnum::ANY, 0, max_words)
                    .map_err(|err| ClipboardError::Backend(err.to_string()))?
                    .reply()
                    .map_err(|err| ClipboardError::Backend(err.to_string()))?;
                if reply.value.is_empty() {
                    return Ok(incr_bytes);
                }
                if incr_bytes.len().saturating_add(reply.value.len()) > max_bytes
                    || reply.bytes_after > 0
                {
                    return Err(ClipboardError::Backend(format!(
                        "clipboard payload exceeds {max_bytes} bytes"
                    )));
                }
                incr_bytes.extend_from_slice(&reply.value);
                let _ = reader.conn.flush();
            }
            Some(_) => {}
        }
    }
}

const fn kind_index(kind: ClipboardKind) -> usize {
    match kind {
        ClipboardKind::Primary => 1,
        _ => 0,
    }
}
