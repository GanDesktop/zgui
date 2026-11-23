//! A second Wayland client acting as a drop target for drags started by `zgui`.
//!
//! Maps a window beside the application under test, drives a virtual pointer to click once (for
//! clipboard setup), then press and drag from the application window onto this target window,
//! accepts `text/uri-list` as a copy, reads the payload through a pipe, and acknowledges with
//! `wl_data_offer.finish()`.

#![allow(dead_code)]

use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_data_device::{self, WlDataDevice};
use wayland_client::protocol::wl_data_device_manager::{DndAction, WlDataDeviceManager};
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_pointer::{self, ButtonState, WlPointer};
use wayland_client::protocol::wl_registry::WlRegistry;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, event_created_child,
};
use wayland_protocols::xdg::shell::client::xdg_surface::{self, XdgSurface};
use wayland_protocols::xdg::shell::client::xdg_toplevel::{self, XdgToplevel};
use wayland_protocols::xdg::shell::client::xdg_wm_base::{self, XdgWmBase};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

/// The kernel's code for the primary mouse button.
const BTN_LEFT: u32 = 0x110;

/// The scale pointer positions are given in: thousandths of the output on each axis.
const EXTENT: u32 = 1000;

/// What the destination client observed from the drag started by `zgui`.
#[derive(Clone, Debug, Default)]
pub(crate) struct Received {
    /// MIME types advertised by the drag source.
    pub(crate) offered_mimes: Vec<String>,
    /// Actions advertised by the drag source.
    pub(crate) source_actions: Option<DndAction>,
    /// Final action selected by the compositor on the offer.
    pub(crate) selected_action: Option<DndAction>,
    /// Raw bytes read from the `text/uri-list` pipe on drop.
    pub(crate) payload: Option<Vec<u8>>,
    /// Clipboard text read back from a fresh connection after the drag completed.
    pub(crate) clipboard_after: Option<String>,
}

/// The destination client's protocol state.
struct Target {
    conn: Connection,
    wanted_mime: String,
    configure: Option<(u32, (i32, i32))>,
    size: (i32, i32),
    pointer: bool,
    over: bool,
    pending_mimes: Vec<String>,
    pending_actions: Option<DndAction>,
    active_offer: Option<WlDataOffer>,
    dropped: bool,
    received: Received,
}

/// Opens a tiled target window, triggers a drag from `zgui`, accepts and finishes it, and reads
/// back the clipboard afterwards to verify the drag left it untouched.
pub(crate) fn receive_drag(wanted_mime: &str) -> Result<Received, String> {
    let conn = Connection::connect_to_env().map_err(|error| error.to_string())?;
    let (globals, mut queue) =
        registry_queue_init::<Target>(&conn).map_err(|error| error.to_string())?;
    let qh = queue.handle();
    let compositor: WlCompositor = globals
        .bind(&qh, 1..=6, ())
        .map_err(bind_error("compositor"))?;
    let shm: WlShm = globals.bind(&qh, 1..=1, ()).map_err(bind_error("shm"))?;
    let shell: XdgWmBase = globals
        .bind(&qh, 1..=6, ())
        .map_err(bind_error("xdg shell"))?;
    let seat: WlSeat = globals.bind(&qh, 1..=9, ()).map_err(bind_error("seat"))?;
    let devices: WlDataDeviceManager = globals
        .bind(&qh, 3..=3, ())
        .map_err(bind_error("data device manager"))?;
    let hands: ZwlrVirtualPointerManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .map_err(bind_error("virtual pointer manager"))?;

    let mut state = Target {
        conn: conn.clone(),
        wanted_mime: wanted_mime.to_owned(),
        configure: None,
        size: (0, 0),
        pointer: false,
        over: false,
        pending_mimes: Vec::new(),
        pending_actions: None,
        active_offer: None,
        dropped: false,
        received: Received::default(),
    };
    let hand = hands.create_virtual_pointer(Some(&seat), &qh, ());
    let mut clock = 0;
    let mut step = |queue: &mut EventQueue<Target>,
                    state: &mut Target,
                    run: &dyn Fn(&ZwlrVirtualPointerV1, u32)| {
        clock += 16;
        run(&hand, clock);
        hand.frame();
        let _ = queue.roundtrip(state);
        std::thread::sleep(Duration::from_millis(60));
        let _ = queue.roundtrip(state);
    };
    let at = |point: (u32, u32)| {
        move |hand: &ZwlrVirtualPointerV1, time: u32| {
            hand.motion_absolute(time, point.0, point.1, EXTENT, EXTENT);
        }
    };

    step(&mut queue, &mut state, &at((500, 500)));
    if !state.pointer {
        return Err("the seat has no pointer, even with a virtual one attached".to_owned());
    }
    let _pointer = seat.get_pointer(&qh, ());
    let device = devices.get_data_device(&seat, &qh, ());

    let surface = compositor.create_surface(&qh, ());
    let shell_surface = shell.get_xdg_surface(&surface, &qh, ());
    let toplevel = shell_surface.get_toplevel(&qh, ());
    toplevel.set_title("drop target".to_owned());
    toplevel.set_app_id("dev.zgui.platform-wayland-test.drop-target".to_owned());
    surface.commit();

    let mut buffer = None;
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        let _ = queue.roundtrip(&mut state);
        if let Some((serial, size)) = state.configure.take() {
            shell_surface.ack_configure(serial);
            let size = (
                if size.0 > 0 { size.0 } else { 200 },
                if size.1 > 0 { size.1 } else { 200 },
            );
            if let Some(old) = buffer.replace(paint(&shm, &qh, &surface, size)?) {
                WlBuffer::destroy(&old);
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if buffer.is_none() {
        return Err("the drop target window was never configured".to_owned());
    }

    // Discover which half of the tiled output belongs to this target window vs zgui's window.
    let mut target_pos = None;
    for candidate in [(750, 500), (250, 500)] {
        step(&mut queue, &mut state, &at(candidate));
        if state.over {
            target_pos = Some(candidate);
            break;
        }
    }
    let Some(target_pos) = target_pos else {
        return Err("the pointer never reached the drop target window".to_owned());
    };
    let source_pos = if target_pos.0 > 500 {
        (250, 500)
    } else {
        (750, 500)
    };

    // First click on zgui's window so it can claim the clipboard selection before dragging.
    step(&mut queue, &mut state, &at(source_pos));
    step(&mut queue, &mut state, &|hand, time| {
        hand.button(time, BTN_LEFT, ButtonState::Pressed);
    });
    step(&mut queue, &mut state, &|hand, time| {
        hand.button(time, BTN_LEFT, ButtonState::Released);
    });
    std::thread::sleep(Duration::from_millis(150));

    // Press on zgui's window and nudge past the drag threshold so zgui calls `start_drag`.
    step(&mut queue, &mut state, &|hand, time| {
        hand.button(time, BTN_LEFT, ButtonState::Pressed);
    });
    let nudge = (
        if target_pos.0 > source_pos.0 {
            source_pos.0 + 30
        } else {
            source_pos.0 - 30
        },
        source_pos.1,
    );
    step(&mut queue, &mut state, &at(nudge));
    std::thread::sleep(Duration::from_millis(150));

    // Carry the pointer across into this target window.
    for index in 1..=8_i64 {
        let along = |from: u32, to: u32| {
            u32::try_from(i64::from(from) + (i64::from(to) - i64::from(from)) * index / 8)
                .unwrap_or(0)
        };
        step(
            &mut queue,
            &mut state,
            &at((along(nudge.0, target_pos.0), along(nudge.1, target_pos.1))),
        );
    }
    for _ in 0..4 {
        step(&mut queue, &mut state, &at(target_pos));
    }
    step(&mut queue, &mut state, &|hand, time| {
        hand.button(time, BTN_LEFT, ButtonState::Released);
    });

    let deadline = Instant::now() + Duration::from_secs(3);
    while !state.dropped && Instant::now() < deadline {
        let _ = queue.roundtrip(&mut state);
        std::thread::sleep(Duration::from_millis(30));
    }
    // Give zgui a moment to receive `dnd_finished` before reading back the clipboard.
    let _ = queue.roundtrip(&mut state);
    std::thread::sleep(Duration::from_millis(150));

    state.received.clipboard_after = super::paste::from_a_second_connection();

    if let Some(offer) = state.active_offer.take() {
        offer.destroy();
    }
    device.release();
    toplevel.destroy();
    shell_surface.destroy();
    surface.destroy();
    let _ = queue.roundtrip(&mut state);
    Ok(state.received)
}

fn bind_error(name: &'static str) -> impl Fn(wayland_client::globals::BindError) -> String {
    move |error| format!("no {name}: {error}")
}

fn paint(
    shm: &WlShm,
    qh: &QueueHandle<Target>,
    surface: &WlSurface,
    size: (i32, i32),
) -> Result<WlBuffer, String> {
    let stride = size.0 * 4;
    let bytes = stride * size.1;
    let directory = std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(std::env::temp_dir, std::path::PathBuf::from);
    let name = directory.join(format!("zgui-drop-target-{}", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&name)
        .map_err(|error| format!("no buffer file: {error}"))?;
    let _ = std::fs::remove_file(&name);
    let pixels = vec![0x60_u8; usize::try_from(bytes).unwrap_or(0)];
    file.write_all(&pixels)
        .map_err(|error| format!("the buffer file would not fill: {error}"))?;
    let pool = shm.create_pool(file.as_fd(), bytes, qh, ());
    let buffer = pool.create_buffer(0, size.0, size.1, stride, wl_shm::Format::Xrgb8888, qh, ());
    pool.destroy();
    surface.attach(Some(&buffer), 0, 0);
    surface.damage_buffer(0, 0, size.0, size.1);
    surface.commit();
    Ok(buffer)
}

impl Dispatch<WlRegistry, GlobalListContents> for Target {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for Target {
    fn event(
        state: &mut Self,
        _: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(capabilities),
        } = event
        {
            state.pointer = capabilities.contains(wl_seat::Capability::Pointer);
        }
    }
}

impl Dispatch<WlPointer, ()> for Target {
    fn event(
        state: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { .. } => state.over = true,
            wl_pointer::Event::Leave { .. } => state.over = false,
            _ => {}
        }
    }
}

impl Dispatch<XdgWmBase, ()> for Target {
    fn event(
        _: &mut Self,
        shell: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            shell.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for Target {
    fn event(
        state: &mut Self,
        _: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            let size = state.size;
            state.configure = Some((serial, size));
        }
    }
}

impl Dispatch<XdgToplevel, ()> for Target {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel::Event::Configure { width, height, .. } = event {
            state.size = (width, height);
        }
    }
}

impl Dispatch<WlDataDevice, ()> for Target {
    event_created_child!(Target, WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (WlDataOffer, ()),
    ]);

    fn event(
        state: &mut Self,
        _: &WlDataDevice,
        event: wl_data_device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_device::Event::DataOffer { .. } => {
                state.pending_mimes.clear();
                state.pending_actions = None;
            }
            wl_data_device::Event::Enter {
                serial,
                id: Some(offer),
                ..
            } => {
                state.received.offered_mimes = state.pending_mimes.clone();
                state.received.source_actions = state.pending_actions;
                if state
                    .pending_mimes
                    .iter()
                    .any(|mime| mime == &state.wanted_mime)
                {
                    offer.accept(serial, Some(state.wanted_mime.clone()));
                    offer.set_actions(DndAction::Copy, DndAction::Copy);
                } else {
                    offer.accept(serial, None);
                    offer.set_actions(DndAction::None, DndAction::None);
                }
                let _ = state.conn.flush();
                if let Some(old) = state.active_offer.replace(offer) {
                    old.destroy();
                }
            }
            wl_data_device::Event::Drop => {
                state.dropped = true;
                if let Some(offer) = state.active_offer.take() {
                    if let Ok((reader, writer)) = rustix::pipe::pipe() {
                        offer.receive(state.wanted_mime.clone(), writer.as_fd());
                        let _ = state.conn.flush();
                        drop(writer);
                        let mut file = std::fs::File::from(reader);
                        let mut bytes = Vec::new();
                        let _ = file.read_to_end(&mut bytes);
                        state.received.payload = Some(bytes);
                    }
                    offer.finish();
                    offer.destroy();
                    let _ = state.conn.flush();
                }
            }
            wl_data_device::Event::Selection { id: Some(offer) } => {
                // Ignore selection offers on this connection; clipboard is verified separately.
                offer.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<WlDataOffer, ()> for Target {
    fn event(
        state: &mut Self,
        _: &WlDataOffer,
        event: wl_data_offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_data_offer::Event::Offer { mime_type } => {
                state.pending_mimes.push(mime_type);
            }
            wl_data_offer::Event::SourceActions {
                source_actions: WEnum::Value(actions),
            } => {
                state.pending_actions = Some(actions);
            }
            wl_data_offer::Event::Action {
                dnd_action: WEnum::Value(action),
            } => {
                state.received.selected_action = Some(action);
            }
            _ => {}
        }
    }
}

macro_rules! ignored {
    ($($object:ty),* $(,)?) => {$(
        impl Dispatch<$object, ()> for Target {
            fn event(
                _: &mut Self,
                _: &$object,
                _: <$object as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

ignored!(
    WlCompositor,
    WlShm,
    WlShmPool,
    WlBuffer,
    WlSurface,
    WlDataDeviceManager,
    ZwlrVirtualPointerManagerV1,
    ZwlrVirtualPointerV1,
);
