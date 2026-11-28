//! Multi-MIME Wayland clipboard worker over a foreign `wl_display`.

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use calloop::channel::{self, Sender};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use rustix::event::{PollFd, PollFlags};
use smithay_client_toolkit::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler};
use smithay_client_toolkit::data_device_manager::{DataDeviceManagerState, ReadPipe, WritePipe};
use smithay_client_toolkit::primary_selection::PrimarySelectionManagerState;
use smithay_client_toolkit::primary_selection::device::{
    PrimarySelectionDevice, PrimarySelectionDeviceHandler,
};
use smithay_client_toolkit::primary_selection::selection::{
    PrimarySelectionSource, PrimarySelectionSourceHandler,
};
use smithay_client_toolkit::reexports::client::backend::ObjectId;
use smithay_client_toolkit::reexports::client::globals::registry_queue_init;
use smithay_client_toolkit::reexports::client::protocol::wl_data_device::WlDataDevice;
use smithay_client_toolkit::reexports::client::protocol::wl_data_device_manager::DndAction;
use smithay_client_toolkit::reexports::client::protocol::wl_data_source::WlDataSource;
use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::WlKeyboard;
use smithay_client_toolkit::reexports::client::protocol::wl_pointer::WlPointer;
use smithay_client_toolkit::reexports::client::protocol::wl_seat::WlSeat;
use smithay_client_toolkit::reexports::client::protocol::wl_surface::WlSurface;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1;
use smithay_client_toolkit::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1;
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::pointer::{PointerData, PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::{
    delegate_data_device, delegate_pointer, delegate_primary_selection, delegate_registry,
    delegate_seat, registry_handlers,
};
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

enum Command {
    Read {
        kind: ClipboardKind,
        format: ClipboardFormat,
        patience: Duration,
        reply: mpsc::SyncSender<Result<ClipboardData, ClipboardError>>,
    },
    ReadMime {
        kind: ClipboardKind,
        mime_types: Vec<String>,
        max_bytes: usize,
        patience: Duration,
        reply: mpsc::SyncSender<Result<ClipboardMimeOffer, ClipboardError>>,
    },
    WriteMime {
        kind: ClipboardKind,
        entries: Vec<ClipboardMime>,
        expected_revision: Option<u64>,
        reply: mpsc::SyncSender<Result<(), ClipboardError>>,
    },
}

/// Multi-MIME Wayland clipboard worker sharing the event loop's `wl_display`.
pub(crate) struct Wayland {
    sender: Sender<Command>,
    worker: Option<JoinHandle<()>>,
}

impl Wayland {
    /// Opens a Wayland clipboard worker over a foreign `wl_display` pointer.
    ///
    /// # Safety
    ///
    /// `display` must be a live `wl_display` pointer that outlives the returned value.
    pub(crate) unsafe fn new(display: *mut c_void) -> Self {
        let addr = display as usize;
        let (sender, receiver) = channel::channel::<Command>();
        let worker = std::thread::Builder::new()
            .name("zgui-winit-clipboard".to_owned())
            .spawn(move || {
                // SAFETY: `addr` came from `display`, which the caller guarantees outlives this
                // struct; `Drop` closes `sender` and joins this thread before `display` dies.
                let backend = unsafe {
                    wayland_backend::sys::client::Backend::from_foreign_display(addr as *mut _)
                };
                let conn = Connection::from_backend(backend);
                run_worker(conn, receiver);
            })
            .ok();
        Self { sender, worker }
    }

    pub(crate) fn read(
        &self,
        kind: ClipboardKind,
        format: ClipboardFormat,
        patience: Duration,
    ) -> Result<ClipboardData, ClipboardError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.sender
            .send(Command::Read {
                kind,
                format,
                patience,
                reply: tx,
            })
            .map_err(|_| ClipboardError::Backend("wayland clipboard worker stopped".to_owned()))?;
        rx.recv().map_err(|_| {
            ClipboardError::Backend("wayland clipboard worker dropped reply".to_owned())
        })?
    }

    pub(crate) fn read_mime(
        &self,
        kind: ClipboardKind,
        mime_types: &[String],
        max_bytes: usize,
        patience: Duration,
    ) -> Result<ClipboardMimeOffer, ClipboardError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.sender
            .send(Command::ReadMime {
                kind,
                mime_types: mime_types.to_vec(),
                max_bytes,
                patience,
                reply: tx,
            })
            .map_err(|_| ClipboardError::Backend("wayland clipboard worker stopped".to_owned()))?;
        rx.recv().map_err(|_| {
            ClipboardError::Backend("wayland clipboard worker dropped reply".to_owned())
        })?
    }

    pub(crate) fn write_mime(
        &self,
        kind: ClipboardKind,
        entries: &[ClipboardMime],
        expected_revision: Option<u64>,
    ) -> Result<(), ClipboardError> {
        let (tx, rx) = mpsc::sync_channel(1);
        self.sender
            .send(Command::WriteMime {
                kind,
                entries: entries.to_vec(),
                expected_revision,
                reply: tx,
            })
            .map_err(|_| ClipboardError::Backend("wayland clipboard worker stopped".to_owned()))?;
        rx.recv().map_err(|_| {
            ClipboardError::Backend("wayland clipboard worker dropped reply".to_owned())
        })?
    }
}

impl Drop for Wayland {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            // Replace sender with a dummy closed channel so the worker's `Event::Closed` fires.
            let (tx, _rx) = channel::channel::<Command>();
            self.sender = tx;
            let _ = worker.join();
        }
    }
}

fn run_worker(conn: Connection, receiver: channel::Channel<Command>) {
    let Ok((globals, mut event_queue)) = registry_queue_init::<State>(&conn) else {
        tracing::warn!("wayland clipboard worker failed registry init");
        return;
    };
    let qh = event_queue.handle();
    let mut state = State {
        conn: conn.clone(),
        qh: qh.clone(),
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        data_device_manager_state: DataDeviceManagerState::bind(&globals, &qh).ok(),
        primary_selection_manager_state: PrimarySelectionManagerState::bind(&globals, &qh).ok(),
        seats: HashMap::new(),
        latest_seat: None,
        revisions: [0, 0],
        awaiting_own_selection: [false, false],
        held: [None, None],
    };

    if event_queue.roundtrip(&mut state).is_err() {
        return;
    }

    let Ok(mut event_loop) = EventLoop::<State>::try_new() else {
        return;
    };
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    if WaylandSource::new(conn, event_queue)
        .insert(loop_handle.clone())
        .is_err()
    {
        return;
    }

    if loop_handle
        .insert_source(receiver, move |event, _, state| match event {
            channel::Event::Msg(command) => state.handle_command(command),
            channel::Event::Closed => loop_signal.stop(),
        })
        .is_err()
    {
        return;
    }

    let _ = event_loop.run(None, &mut state, |_| {});
}

#[derive(Default)]
struct ClipboardSeatState {
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    data_device: Option<DataDevice>,
    primary_device: Option<PrimarySelectionDevice>,
    keyboard_focus: bool,
    pointer_focus: bool,
    latest_serial: u32,
}

impl Drop for ClipboardSeatState {
    fn drop(&mut self) {
        if let Some(keyboard) = self.keyboard.take()
            && keyboard.version() >= 3
        {
            keyboard.release();
        }
        if let Some(pointer) = self.pointer.take()
            && pointer.version() >= 3
        {
            pointer.release();
        }
    }
}

enum Source {
    Standard(CopyPasteSource),
    Primary(PrimarySelectionSource),
}

struct Held {
    entries: BTreeMap<String, Vec<u8>>,
    source: Source,
}

struct State {
    conn: Connection,
    qh: QueueHandle<Self>,
    registry_state: RegistryState,
    seat_state: SeatState,
    data_device_manager_state: Option<DataDeviceManagerState>,
    primary_selection_manager_state: Option<PrimarySelectionManagerState>,
    seats: HashMap<ObjectId, ClipboardSeatState>,
    latest_seat: Option<ObjectId>,
    revisions: [u64; 2],
    awaiting_own_selection: [bool; 2],
    held: [Option<Held>; 2],
}

impl State {
    fn active_seat_id(&self) -> Option<ObjectId> {
        if let Some(id) = &self.latest_seat
            && let Some(seat) = self.seats.get(id)
            && (seat.keyboard_focus || seat.pointer_focus)
            && seat.latest_serial != 0
        {
            return Some(id.clone());
        }
        self.seats
            .iter()
            .find(|(_, seat)| {
                (seat.keyboard_focus || seat.pointer_focus) && seat.latest_serial != 0
            })
            .map(|(id, _)| id.clone())
    }

    fn any_seat(&self) -> Option<&ClipboardSeatState> {
        if let Some(id) = &self.latest_seat
            && let Some(seat) = self.seats.get(id)
        {
            return Some(seat);
        }
        self.seats.values().next()
    }

    fn handle_command(&mut self, command: Command) {
        match command {
            Command::Read {
                kind,
                format,
                patience,
                reply,
            } => {
                self.read_single(kind, format, patience, reply);
            }
            Command::ReadMime {
                kind,
                mime_types,
                max_bytes,
                patience,
                reply,
            } => {
                self.read_mime(kind, mime_types, max_bytes, patience, reply);
            }
            Command::WriteMime {
                kind,
                entries,
                expected_revision,
                reply,
            } => {
                let _ = reply.send(self.write_mime(kind, entries, expected_revision));
            }
        }
    }

    fn read_single(
        &self,
        kind: ClipboardKind,
        format: ClipboardFormat,
        patience: Duration,
        reply: mpsc::SyncSender<Result<ClipboardData, ClipboardError>>,
    ) {
        if let Some(held) = &self.held[index(kind)] {
            let keys: Vec<String> = held.entries.keys().cloned().collect();
            let result = match mime::pick_for_format(&keys, format) {
                Some(key) => {
                    let bytes = lookup_entry(&held.entries, &key)
                        .unwrap_or_default()
                        .to_vec();
                    Ok(mime::decode_for_format(format, &key, bytes))
                }
                None => Err(ClipboardError::UnavailableFormat(format)),
            };
            let _ = reply.send(result);
            return;
        }
        let Some(seat) = self.any_seat() else {
            let _ = reply.send(Err(ClipboardError::Empty(kind)));
            return;
        };
        let offer_result = match kind {
            ClipboardKind::Primary => {
                let Some(primary) = &seat.primary_device else {
                    let _ = reply.send(Err(ClipboardError::UnsupportedKind(kind)));
                    return;
                };
                match primary.data().selection_offer() {
                    Some(offer) => {
                        let Some(wanted) =
                            offer.with_mime_types(|offered| mime::pick_for_format(offered, format))
                        else {
                            let _ = reply.send(Err(ClipboardError::UnavailableFormat(format)));
                            return;
                        };
                        offer
                            .receive(wanted.clone())
                            .map(|reader| (wanted, reader))
                            .map_err(|err| ClipboardError::Backend(err.to_string()))
                    }
                    None => Err(ClipboardError::Empty(kind)),
                }
            }
            _ => {
                let Some(standard) = &seat.data_device else {
                    let _ = reply.send(Err(ClipboardError::Empty(kind)));
                    return;
                };
                match standard.data().selection_offer() {
                    Some(offer) => {
                        let Some(wanted) =
                            offer.with_mime_types(|offered| mime::pick_for_format(offered, format))
                        else {
                            let _ = reply.send(Err(ClipboardError::UnavailableFormat(format)));
                            return;
                        };
                        offer
                            .receive(wanted.clone())
                            .map(|reader| (wanted, reader))
                            .map_err(|err| ClipboardError::Backend(err.to_string()))
                    }
                    None => Err(ClipboardError::Empty(kind)),
                }
            }
        };
        let (wanted, reader) = match offer_result {
            Ok(pair) => pair,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        let _ = self.conn.flush();
        let _ = std::thread::Builder::new()
            .name("zgui-winit-clip-read".to_owned())
            .spawn(move || {
                let deadline = Instant::now() + patience;
                let result = match read_pipe_until(reader, deadline, LEGACY_LIMIT) {
                    Ok(bytes) if bytes.is_empty() => Err(ClipboardError::Empty(kind)),
                    Ok(bytes) => Ok(mime::decode_for_format(format, &wanted, bytes)),
                    Err(error) => Err(error),
                };
                let _ = reply.send(result);
            });
    }

    fn read_mime(
        &self,
        kind: ClipboardKind,
        mime_types: Vec<String>,
        max_bytes: usize,
        patience: Duration,
        reply: mpsc::SyncSender<Result<ClipboardMimeOffer, ClipboardError>>,
    ) {
        let idx = index(kind);
        let revision = self.revisions[idx];
        if let Some(held) = &self.held[idx] {
            let offered_mime_types: Vec<String> = held.entries.keys().cloned().collect();
            let mut total_bytes = 0_usize;
            let mut entries = Vec::new();
            for wanted in &mime_types {
                if let Some(bytes) = lookup_entry(&held.entries, wanted) {
                    total_bytes = total_bytes.saturating_add(bytes.len());
                    if total_bytes > max_bytes {
                        let _ = reply.send(Err(ClipboardError::Backend(format!(
                            "clipboard payload exceeds {max_bytes} bytes"
                        ))));
                        return;
                    }
                    entries.push(ClipboardMime {
                        mime_type: wanted.clone(),
                        bytes: bytes.to_vec(),
                    });
                }
            }
            let _ = reply.send(Ok(ClipboardMimeOffer {
                revision,
                offered_mime_types,
                entries,
            }));
            return;
        }
        let Some(seat) = self.any_seat() else {
            let _ = reply.send(Err(ClipboardError::Empty(kind)));
            return;
        };
        let collect_matches = |offered: &[String]| -> (Vec<String>, Vec<(String, String)>) {
            let matched = mime_types
                .iter()
                .filter_map(|wanted| {
                    mime::match_offered(offered, wanted)
                        .map(|matched| (wanted.clone(), matched.to_owned()))
                })
                .collect();
            (offered.to_vec(), matched)
        };
        let readers_result = match kind {
            ClipboardKind::Primary => {
                let Some(primary) = &seat.primary_device else {
                    let _ = reply.send(Err(ClipboardError::UnsupportedKind(kind)));
                    return;
                };
                match primary.data().selection_offer() {
                    Some(offer) => {
                        let (offered_mime_types, matched) = offer.with_mime_types(collect_matches);
                        matched
                            .into_iter()
                            .map(|(wanted, offered_name)| {
                                offer
                                    .receive(offered_name)
                                    .map(|pipe| (wanted, pipe))
                                    .map_err(|err| ClipboardError::Backend(err.to_string()))
                            })
                            .collect::<Result<Vec<_>, _>>()
                            .map(|readers| (offered_mime_types, readers))
                    }
                    None => Err(ClipboardError::Empty(kind)),
                }
            }
            _ => {
                let Some(standard) = &seat.data_device else {
                    let _ = reply.send(Err(ClipboardError::Empty(kind)));
                    return;
                };
                match standard.data().selection_offer() {
                    Some(offer) => {
                        let (offered_mime_types, matched) = offer.with_mime_types(collect_matches);
                        matched
                            .into_iter()
                            .map(|(wanted, offered_name)| {
                                offer
                                    .receive(offered_name)
                                    .map(|pipe| (wanted, pipe))
                                    .map_err(|err| ClipboardError::Backend(err.to_string()))
                            })
                            .collect::<Result<Vec<_>, _>>()
                            .map(|readers| (offered_mime_types, readers))
                    }
                    None => Err(ClipboardError::Empty(kind)),
                }
            }
        };
        let (offered_mime_types, readers) = match readers_result {
            Ok(pair) => pair,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        if readers.is_empty() {
            let _ = reply.send(Ok(ClipboardMimeOffer {
                revision,
                offered_mime_types,
                entries: Vec::new(),
            }));
            return;
        }
        let _ = self.conn.flush();
        let _ = std::thread::Builder::new()
            .name("zgui-winit-clip-read".to_owned())
            .spawn(move || {
                let deadline = Instant::now() + patience;
                let mut total_bytes = 0_usize;
                let mut entries = Vec::with_capacity(readers.len());
                for (wanted, reader) in readers {
                    let remaining = max_bytes.saturating_sub(total_bytes);
                    match read_pipe_until(reader, deadline, remaining) {
                        Ok(bytes) => {
                            total_bytes = total_bytes.saturating_add(bytes.len());
                            entries.push(ClipboardMime {
                                mime_type: wanted,
                                bytes,
                            });
                        }
                        Err(error) => {
                            let _ = reply.send(Err(error));
                            return;
                        }
                    }
                }
                let _ = reply.send(Ok(ClipboardMimeOffer {
                    revision,
                    offered_mime_types,
                    entries,
                }));
            });
    }

    fn write_mime(
        &mut self,
        kind: ClipboardKind,
        entries: Vec<ClipboardMime>,
        expected_revision: Option<u64>,
    ) -> Result<(), ClipboardError> {
        let idx = index(kind);
        if expected_revision.is_some_and(|expected| expected != self.revisions[idx]) {
            return Err(ClipboardError::Changed(kind));
        }
        let Some(seat_id) = self.active_seat_id() else {
            return Err(ClipboardError::NotFocused(kind));
        };
        let Some(seat) = self.seats.get(&seat_id) else {
            return Err(ClipboardError::NotFocused(kind));
        };
        let serial = seat.latest_serial;
        let qh = &self.qh;

        let held = if entries.is_empty() {
            match kind {
                ClipboardKind::Primary => {
                    if let Some(primary) = &seat.primary_device {
                        primary.unset_selection(serial);
                    }
                }
                _ => {
                    if let Some(standard) = &seat.data_device {
                        standard.unset_selection(serial);
                    }
                }
            }
            None
        } else {
            let mut map = BTreeMap::new();
            let mut offered_mimes = Vec::with_capacity(entries.len());
            for entry in &entries {
                if !map.contains_key(&entry.mime_type) {
                    offered_mimes.push(entry.mime_type.as_str());
                }
                map.insert(entry.mime_type.clone(), entry.bytes.clone());
            }
            match kind {
                ClipboardKind::Primary => {
                    let manager = self
                        .primary_selection_manager_state
                        .as_ref()
                        .ok_or(ClipboardError::UnsupportedKind(kind))?;
                    let device = seat
                        .primary_device
                        .as_ref()
                        .ok_or(ClipboardError::UnsupportedKind(kind))?;
                    let source = manager.create_selection_source(qh, offered_mimes);
                    source.set_selection(device, serial);
                    Some(Held {
                        entries: map,
                        source: Source::Primary(source),
                    })
                }
                _ => {
                    let manager = self.data_device_manager_state.as_ref().ok_or_else(|| {
                        ClipboardError::Backend("compositor offers no clipboard".to_owned())
                    })?;
                    let device = seat.data_device.as_ref().ok_or_else(|| {
                        ClipboardError::Backend("compositor offers no clipboard".to_owned())
                    })?;
                    let source = manager.create_copy_paste_source(qh, offered_mimes);
                    source.set_selection(device, serial);
                    Some(Held {
                        entries: map,
                        source: Source::Standard(source),
                    })
                }
            }
        };

        self.revisions[idx] = self.revisions[idx].saturating_add(1);
        self.awaiting_own_selection[idx] = true;
        self.held[idx] = held;
        let _ = self.conn.flush();
        Ok(())
    }

    fn note_selection_changed(&mut self, kind: ClipboardKind, has_offer: bool) {
        let idx = index(kind);
        if self.awaiting_own_selection[idx] && (self.held[idx].is_some() || !has_offer) {
            self.awaiting_own_selection[idx] = false;
            return;
        }
        self.awaiting_own_selection[idx] = false;
        self.revisions[idx] = self.revisions[idx].saturating_add(1);
    }

    fn serve(&self, kind: ClipboardKind, mime_type: &str, destination: WritePipe) {
        let bytes = self.held[index(kind)]
            .as_ref()
            .and_then(|held| lookup_entry(&held.entries, mime_type))
            .map(<[u8]>::to_vec);
        let _ = std::thread::Builder::new()
            .name("zgui-winit-clip-write".to_owned())
            .spawn(move || {
                write_pipe_until(
                    destination,
                    bytes.as_deref().unwrap_or_default(),
                    Instant::now() + PATIENCE,
                );
            });
    }
}

impl ProvidesRegistryState for State {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![SeatState];
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.seats.insert(seat.id(), ClipboardSeatState::default());
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        let seat_state = self.seats.entry(seat.id()).or_default();
        match capability {
            Capability::Keyboard => {
                seat_state.keyboard = Some(seat.get_keyboard(qh, seat.id()));
                if seat_state.data_device.is_none()
                    && let Some(mgr) = &self.data_device_manager_state
                {
                    seat_state.data_device = Some(mgr.get_data_device(qh, &seat));
                }
                if seat_state.primary_device.is_none()
                    && let Some(mgr) = &self.primary_selection_manager_state
                {
                    seat_state.primary_device = Some(mgr.get_selection_device(qh, &seat));
                }
            }
            Capability::Pointer => {
                seat_state.pointer = self.seat_state.get_pointer(qh, &seat).ok();
                if seat_state.data_device.is_none()
                    && let Some(mgr) = &self.data_device_manager_state
                {
                    seat_state.data_device = Some(mgr.get_data_device(qh, &seat));
                }
                if seat_state.primary_device.is_none()
                    && let Some(mgr) = &self.primary_selection_manager_state
                {
                    seat_state.primary_device = Some(mgr.get_selection_device(qh, &seat));
                }
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        let Some(seat_state) = self.seats.get_mut(&seat.id()) else {
            return;
        };
        match capability {
            Capability::Keyboard => {
                seat_state.keyboard_focus = false;
                if let Some(keyboard) = seat_state.keyboard.take()
                    && keyboard.version() >= 3
                {
                    keyboard.release();
                }
            }
            Capability::Pointer => {
                seat_state.pointer_focus = false;
                if let Some(pointer) = seat_state.pointer.take()
                    && pointer.version() >= 3
                {
                    pointer.release();
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.seats.remove(&seat.id());
    }
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        let seat_id = pointer
            .data::<PointerData>()
            .expect("pointer seat")
            .seat()
            .id();
        let Some(seat_state) = self.seats.get_mut(&seat_id) else {
            return;
        };
        let mut updated = false;
        for event in events {
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    seat_state.pointer_focus = true;
                    seat_state.latest_serial = serial;
                    updated = true;
                }
                PointerEventKind::Leave { serial } => {
                    seat_state.pointer_focus = false;
                    seat_state.latest_serial = serial;
                    updated = true;
                }
                PointerEventKind::Press { serial, .. }
                | PointerEventKind::Release { serial, .. } => {
                    seat_state.latest_serial = serial;
                    updated = true;
                }
                _ => {}
            }
        }
        if updated {
            self.latest_seat = Some(seat_id);
        }
    }
}

impl Dispatch<WlKeyboard, ObjectId, State> for State {
    fn event(
        state: &mut State,
        _: &WlKeyboard,
        event: <WlKeyboard as Proxy>::Event,
        seat_id: &ObjectId,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use smithay_client_toolkit::reexports::client::protocol::wl_keyboard::Event as WlKeyboardEvent;

        let Some(seat_state) = state.seats.get_mut(seat_id) else {
            return;
        };
        match event {
            WlKeyboardEvent::Enter { serial, .. } => {
                seat_state.keyboard_focus = true;
                seat_state.latest_serial = serial;
                state.latest_seat = Some(seat_id.clone());
            }
            WlKeyboardEvent::Leave { .. } => {
                seat_state.keyboard_focus = false;
                if !seat_state.pointer_focus {
                    seat_state.latest_serial = 0;
                }
            }
            WlKeyboardEvent::Key { serial, .. } | WlKeyboardEvent::Modifiers { serial, .. } => {
                seat_state.latest_serial = serial;
                state.latest_seat = Some(seat_id.clone());
            }
            _ => {}
        }
    }
}

impl DataDeviceHandler for State {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataDevice,
        _: f64,
        _: f64,
        _: &WlSurface,
    ) {
    }
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _device: &WlDataDevice) {
        let has_offer = self
            .any_seat()
            .and_then(|seat| seat.data_device.as_ref())
            .and_then(|dev| dev.data().selection_offer())
            .is_some();
        self.note_selection_changed(ClipboardKind::Standard, has_offer);
    }
}

impl DataOfferHandler for State {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl DataSourceHandler for State {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime_type: String,
        destination: WritePipe,
    ) {
        let is_ours = matches!(
            self.held[index(ClipboardKind::Standard)].as_ref().map(|h| &h.source),
            Some(Source::Standard(owned)) if owned.inner() == source
        );
        if is_ours {
            self.serve(ClipboardKind::Standard, &mime_type, destination);
        }
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, deleted: &WlDataSource) {
        let idx = index(ClipboardKind::Standard);
        if matches!(
            self.held[idx].as_ref().map(|h| &h.source),
            Some(Source::Standard(owned)) if owned.inner() == deleted
        ) {
            self.held[idx] = None;
            self.awaiting_own_selection[idx] = false;
            self.revisions[idx] = self.revisions[idx].saturating_add(1);
        }
    }

    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }
    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
}

impl PrimarySelectionDeviceHandler for State {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _device: &ZwpPrimarySelectionDeviceV1,
    ) {
        let has_offer = self
            .any_seat()
            .and_then(|seat| seat.primary_device.as_ref())
            .and_then(|dev| dev.data().selection_offer())
            .is_some();
        self.note_selection_changed(ClipboardKind::Primary, has_offer);
    }
}

impl PrimarySelectionSourceHandler for State {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        mime_type: String,
        destination: WritePipe,
    ) {
        let is_ours = matches!(
            self.held[index(ClipboardKind::Primary)].as_ref().map(|h| &h.source),
            Some(Source::Primary(owned)) if owned.inner() == source
        );
        if is_ours {
            self.serve(ClipboardKind::Primary, &mime_type, destination);
        }
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        deleted: &ZwpPrimarySelectionSourceV1,
    ) {
        let idx = index(ClipboardKind::Primary);
        if matches!(
            self.held[idx].as_ref().map(|h| &h.source),
            Some(Source::Primary(owned)) if owned.inner() == deleted
        ) {
            self.held[idx] = None;
            self.awaiting_own_selection[idx] = false;
            self.revisions[idx] = self.revisions[idx].saturating_add(1);
        }
    }
}

delegate_seat!(State);
delegate_pointer!(State);
delegate_data_device!(State);
delegate_primary_selection!(State);
delegate_registry!(State);

const fn index(kind: ClipboardKind) -> usize {
    match kind {
        ClipboardKind::Primary => 1,
        _ => 0,
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

fn wait_fd(fd: BorrowedFd<'_>, flags: PollFlags, deadline: Instant) -> Result<(), ClipboardError> {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(ClipboardError::TimedOut);
        }
        let mut watched = [PollFd::new(&fd, flags)];
        match rustix::event::poll(&mut watched, Some(&left.try_into().unwrap_or_default())) {
            Ok(0) => return Err(ClipboardError::TimedOut),
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(err) => return Err(ClipboardError::Backend(err.to_string())),
        }
    }
}

fn read_pipe_until(
    mut source: ReadPipe,
    deadline: Instant,
    limit: usize,
) -> Result<Vec<u8>, ClipboardError> {
    let mut taken = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        wait_fd(source.as_fd(), PollFlags::IN, deadline)?;
        match source.read(&mut chunk) {
            Ok(0) => return Ok(taken),
            Ok(read) => {
                if taken.len().saturating_add(read) > limit {
                    return Err(ClipboardError::Backend(format!(
                        "clipboard payload exceeds {limit} bytes"
                    )));
                }
                taken.extend_from_slice(&chunk[..read]);
            }
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(err) => return Err(ClipboardError::Backend(err.to_string())),
        }
    }
}

fn write_pipe_until(mut destination: WritePipe, bytes: &[u8], deadline: Instant) {
    let mut left = bytes;
    while !left.is_empty() {
        if wait_fd(destination.as_fd(), PollFlags::OUT, deadline).is_err() {
            return;
        }
        match destination.write(left) {
            Ok(0) => return,
            Ok(written) => left = &left[written..],
            Err(err)
                if matches!(
                    err.kind(),
                    std::io::ErrorKind::Interrupted | std::io::ErrorKind::WouldBlock
                ) => {}
            Err(_) => return,
        }
    }
}
