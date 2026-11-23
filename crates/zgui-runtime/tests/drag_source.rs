//! Outgoing native drag lifecycle events reach window callbacks inside a frame.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use zgui_platform::{
    DragAction, DragActions, DragMime, DragSessionId, DragSourceEvent, DragSourceRequest,
    PlatformError, SurfaceEvent, Unsupported,
};
use zgui_reactive::prelude::{GetUntracked, Set};
use zgui_reactive::signal;
use zgui_runtime::windows::use_window;
use zgui_view::{BuildCx, IntoView, View};

const CSS: &str = "root { display: block; width: 320px; height: 200px }";

#[test]
fn window_handle_delivers_drag_source_events_until_guard_drops() {
    let seen: Rc<RefCell<Vec<DragSourceEvent>>> = Rc::default();
    let seen_in_view = Rc::clone(&seen);
    let guard_slot: Rc<RefCell<Option<zgui_runtime::DragSourceGuard>>> = Rc::default();
    let guard_in_view = Rc::clone(&guard_slot);
    let handle_slot: Rc<RefCell<Option<zgui_runtime::WindowHandle>>> = Rc::default();
    let handle_in_view = Rc::clone(&handle_slot);
    let (finished_count, set_finished_count) = signal(0_u32);

    let mut app = support::app(CSS, move |cx: &mut BuildCx<'_>| {
        let window = use_window();
        *handle_in_view.borrow_mut() = Some(window.clone());
        let seen = Rc::clone(&seen_in_view);
        let guard = window.on_drag_source(move |event| {
            if matches!(event, DragSourceEvent::Finished { .. }) {
                set_finished_count.set(finished_count.get_untracked() + 1);
            }
            seen.borrow_mut().push(event);
        });
        *guard_in_view.borrow_mut() = Some(guard);
        Box::new(zgui_elements::column().into_view().build(cx))
    });
    app.settle(4);

    let window = handle_slot
        .borrow()
        .clone()
        .expect("window handle was captured");
    let unsupported = window.start_drag(DragSourceRequest::new(
        vec![DragMime::new("text/uri-list", b"file:///tmp/a.txt\r\n")],
        DragActions::COPY,
    ));
    assert_eq!(unsupported, Err(PlatformError::Unsupported(Unsupported)));

    let session = DragSessionId::new(1);
    let target = DragSourceEvent::TargetMime {
        session,
        mime_type: Some("text/uri-list".to_owned()),
    };
    let action = DragSourceEvent::ActionChanged {
        session,
        action: Some(DragAction::Copy),
    };
    let dropped = DragSourceEvent::Dropped {
        session,
        action: Some(DragAction::Copy),
    };
    let finished = DragSourceEvent::Finished {
        session,
        action: Some(DragAction::Copy),
    };

    for event in [&target, &action, &dropped, &finished] {
        app.deliver_to_first(SurfaceEvent::DragSource(event.clone()));
    }
    app.settle(4);

    assert_eq!(*seen.borrow(), [target, action, dropped, finished]);
    assert_eq!(finished_count.get_untracked(), 1);

    // Dropping the guard unregisters the listener.
    guard_slot.borrow_mut().take();
    app.deliver_to_first(SurfaceEvent::DragSource(DragSourceEvent::Cancelled {
        session: DragSessionId::new(2),
    }));
    app.settle(4);
    assert_eq!(seen.borrow().len(), 4);
}
