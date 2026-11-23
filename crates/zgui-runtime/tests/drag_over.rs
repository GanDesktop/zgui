//! Content dragged from outside the window tells the elements it is over, negotiates accepted
//! MIME types and actions per control, and delays protocol completion via `DropHandle`.

mod support;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use zgui_geom::{Css, CssPx, Point};
use zgui_platform::{
    DragAction, DragActions, DragDecision, DragEvent, DragMime, DragOfferId, DropHandle,
    SurfaceEvent,
};
use zgui_reactive::prelude::GetUntracked;
use zgui_view::event::events;
use zgui_view::{BuildCx, IntoView, View, ViewHost};
use zgui_vocab::{KeyCode, KeyEvent, KeyState, Modifiers, NamedKey, PhysicalKey, Timestamp};

const CSS: &str = "root { display: block; width: 400px; height: 300px }
                   .zone { display: block; height: 40px }
                   .zone:state(drag-over) { height: 80px }
                   .inner { display: block; height: 20px }
                   .target { display: block; height: 40px }";

const NEGOTIATION_CSS: &str = "root { display: block; width: 400px; height: 300px }
                               .toolbar { display: block; height: 40px }
                               .folder { display: block; height: 40px }
                               .terminal { display: block; height: 40px }";

fn at(x: f32, y: f32) -> Point<CssPx, Css> {
    Point::new(CssPx(x), CssPx(y))
}

#[test]
fn a_drag_is_announced_and_styled_until_it_leaves_or_drops() {
    let log: Rc<RefCell<Vec<String>>> = Rc::default();
    let seen = Rc::clone(&log);
    let mut app = support::app(CSS, move |cx: &mut BuildCx<'_>| {
        let (enter, over, leave, dropped) = (
            Rc::clone(&seen),
            Rc::clone(&seen),
            Rc::clone(&seen),
            Rc::clone(&seen),
        );
        Box::new(
            zgui_elements::column()
                .child(
                    zgui_elements::r#box()
                        .class("zone")
                        .on(events::DRAG_ENTER, move |cx| {
                            enter.borrow_mut().push(format!("enter {}", cx.paths.len()));
                        })
                        .on(events::DRAG_OVER, move |_| {
                            over.borrow_mut().push("over".into())
                        })
                        .on(events::DRAG_LEAVE, move |cx| {
                            let at = cx.position;
                            leave
                                .borrow_mut()
                                .push(format!("leave {} {}", at.x.0, at.y.0));
                        })
                        .on(events::DROP, move |cx| {
                            dropped
                                .borrow_mut()
                                .push(format!("drop {}", cx.paths.len()));
                        })
                        .child(zgui_elements::r#box().class("inner")),
                )
                .into_view()
                .build(cx),
        )
    });
    app.settle(8);
    let height = |app: &zgui_platform_headless::Harness<zgui_runtime::Runtime>| {
        app.app().windows()[0]
            .box_with_class("zone")
            .expect("the zone is laid out")
            .size
            .height
            .0
    };
    assert_eq!(height(&app), 40.0);

    let offer = DragOfferId::new(1);
    let paths = vec![PathBuf::from("/tmp/a.wav"), PathBuf::from("/tmp/b.wav")];
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Entered {
        offer,
        mime_types: vec!["text/uri-list".to_owned()],
        source_actions: DragActions::COPY,
        action: Some(DragAction::Copy),
        paths: paths.clone(),
        position: at(10.0, 10.0),
    }));
    app.settle(8);
    assert_eq!(
        *log.borrow(),
        ["enter 2", "over"],
        "the inner box's drag_over bubbles"
    );
    assert_eq!(height(&app), 80.0, "the zone matches :state(drag-over)");

    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Moved {
        offer,
        position: at(10.0, 200.0),
    }));
    app.settle(8);
    assert_eq!(
        log.borrow().last().map(String::as_str),
        Some("leave 10 200")
    );
    assert_eq!(height(&app), 40.0, "off the zone, the state clears");

    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Moved {
        offer,
        position: at(10.0, 10.0),
    }));
    app.settle(8);
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Left { offer }));
    app.settle(8);
    assert_eq!(
        log.borrow().last().map(String::as_str),
        Some("leave 10 10"),
        "a drag that left the window is announced at its last position"
    );
    assert_eq!(
        height(&app),
        40.0,
        "a drag that left the window clears the state"
    );

    let offer2 = DragOfferId::new(2);
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Entered {
        offer: offer2,
        mime_types: vec!["text/uri-list".to_owned()],
        source_actions: DragActions::COPY,
        action: Some(DragAction::Copy),
        paths: paths.clone(),
        position: at(10.0, 10.0),
    }));
    app.settle(8);
    log.borrow_mut().clear();
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Dropped {
        offer: offer2,
        mime_types: vec!["text/uri-list".to_owned()],
        source_actions: DragActions::COPY,
        action: Some(DragAction::Copy),
        paths,
        payload: None,
        position: at(10.0, 10.0),
        handle: DropHandle::noop(),
    }));
    app.settle(8);
    assert_eq!(*log.borrow(), ["drop 2"], "a drop is not also a leave");
    assert_eq!(height(&app), 40.0, "a drop ends the drag");
}

#[test]
fn a_drop_goes_to_the_element_under_it_and_not_to_the_focused_one() {
    let log: Rc<RefCell<Vec<&'static str>>> = Rc::default();
    let seen = Rc::clone(&log);
    let mut app = support::app(CSS, move |cx: &mut BuildCx<'_>| {
        let (zone, target) = (Rc::clone(&seen), Rc::clone(&seen));
        Box::new(
            zgui_elements::column()
                .child(
                    zgui_elements::r#box()
                        .class("zone")
                        .on(events::DROP, move |_| zone.borrow_mut().push("zone")),
                )
                .child(
                    zgui_elements::control()
                        .class("target")
                        .on(events::DROP, move |_| target.borrow_mut().push("control")),
                )
                .into_view()
                .build(cx),
        )
    });
    app.settle(8);
    // Tab into the window: the control is the first focusable element in it.
    app.deliver_to_first(SurfaceEvent::Key {
        state: KeyState::Pressed,
        event: KeyEvent::named(NamedKey::Tab, PhysicalKey::Code(KeyCode::Tab)),
        modifiers: Modifiers::NONE,
        timestamp: Timestamp::ORIGIN,
    });
    app.settle(8);
    assert!(
        app.app().windows()[0]
            .host()
            .focused()
            .get_untracked()
            .is_some(),
        "the fixture never focused the control"
    );

    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Dropped {
        offer: DragOfferId::new(1),
        mime_types: vec!["text/uri-list".to_owned()],
        source_actions: DragActions::COPY,
        action: Some(DragAction::Copy),
        paths: vec![PathBuf::from("/tmp/a.wav")],
        payload: None,
        position: at(10.0, 10.0),
        handle: DropHandle::noop(),
    }));
    app.settle(8);
    assert_eq!(*log.borrow(), ["zone"]);
}

#[test]
fn controls_negotiate_mime_and_actions_and_claim_delayed_drop_handle() {
    let seen_actions: Rc<RefCell<Vec<Option<DragAction>>>> = Rc::default();
    let actions_in_folder = Rc::clone(&seen_actions);
    let claimed_handle: Rc<RefCell<Option<DropHandle>>> = Rc::default();
    let handle_in_folder = Rc::clone(&claimed_handle);
    let received_payload: Rc<RefCell<Option<DragMime>>> = Rc::default();
    let payload_in_folder = Rc::clone(&received_payload);

    let mut app = support::app(NEGOTIATION_CSS, move |cx: &mut BuildCx<'_>| {
        let actions = Rc::clone(&actions_in_folder);
        let handle_slot = Rc::clone(&handle_in_folder);
        let payload_slot = Rc::clone(&payload_in_folder);
        Box::new(
            zgui_elements::column()
                .child(zgui_elements::r#box().class("toolbar"))
                .child(
                    zgui_elements::r#box()
                        .class("folder")
                        .on(events::DRAG_OVER, move |ev| {
                            actions.borrow_mut().push(ev.action);
                            if ev.offers_files() {
                                ev.accept_drag(DragDecision::accept(
                                    "text/uri-list",
                                    DragActions::COPY | DragActions::MOVE,
                                    Some(DragAction::Move),
                                ));
                            }
                        })
                        .on(events::DROP, move |ev| {
                            *payload_slot.borrow_mut() = ev.payload.clone();
                            *handle_slot.borrow_mut() = ev.take_drop_handle();
                        }),
                )
                .child(
                    zgui_elements::r#box()
                        .class("terminal")
                        .on(events::DRAG_OVER, move |ev| {
                            if ev.offers_files() {
                                ev.accept_drag(DragDecision::accept(
                                    "text/uri-list",
                                    DragActions::COPY,
                                    Some(DragAction::Copy),
                                ));
                            }
                        }),
                )
                .into_view()
                .build(cx),
        )
    });
    app.settle(8);

    let surface = app.platform().offscreens()[0].clone();
    let offer = DragOfferId::new(10);

    // 1. Hover toolbar -> rejected.
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Entered {
        offer,
        mime_types: vec!["text/uri-list".to_owned()],
        source_actions: DragActions::COPY | DragActions::MOVE,
        action: Some(DragAction::Move),
        paths: Vec::new(),
        position: at(10.0, 15.0),
    }));
    app.settle(8);
    assert_eq!(
        surface.take_drag_responses(),
        vec![(offer, DragDecision::reject())]
    );

    // 2. Move onto folder -> accepts Copy | Move, prefers Move.
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Moved {
        offer,
        position: at(10.0, 55.0),
    }));
    app.settle(8);
    assert_eq!(
        surface.take_drag_responses(),
        vec![(
            offer,
            DragDecision::accept(
                "text/uri-list",
                DragActions::COPY | DragActions::MOVE,
                Some(DragAction::Move),
            ),
        )]
    );

    // 3. Compositor switches negotiated action (e.g. user holds Ctrl) -> folder observes it.
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::ActionChanged {
        offer,
        action: Some(DragAction::Copy),
    }));
    app.settle(8);
    assert_eq!(
        *seen_actions.borrow(),
        [Some(DragAction::Move), Some(DragAction::Copy)]
    );
    // Decision did not change, so no redundant protocol roundtrip was sent.
    assert!(surface.take_drag_responses().is_empty());

    // 4. Move onto terminal -> forces Copy only.
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Moved {
        offer,
        position: at(10.0, 95.0),
    }));
    app.settle(8);
    assert_eq!(
        surface.take_drag_responses(),
        vec![(
            offer,
            DragDecision::accept("text/uri-list", DragActions::COPY, Some(DragAction::Copy)),
        )]
    );

    // 5. Drop onto folder -> DropHandle stays pending until business code completes it.
    let outcome = Arc::new(AtomicU8::new(0));
    let outcome_cb = Arc::clone(&outcome);
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Dropped {
        offer,
        mime_types: vec!["text/uri-list".to_owned()],
        source_actions: DragActions::COPY | DragActions::MOVE,
        action: Some(DragAction::Move),
        paths: vec![PathBuf::from("/tmp/moved.txt")],
        payload: Some(DragMime::new("text/uri-list", b"file:///tmp/moved.txt\r\n")),
        position: at(10.0, 55.0),
        handle: DropHandle::new(move |ok| {
            outcome_cb.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
        }),
    }));
    app.settle(8);
    assert_eq!(
        outcome.load(Ordering::Relaxed),
        0,
        "offer must not be finished before the drop handler completes its operation"
    );
    assert_eq!(
        *received_payload.borrow(),
        Some(DragMime::new("text/uri-list", b"file:///tmp/moved.txt\r\n",))
    );
    claimed_handle
        .borrow_mut()
        .take()
        .expect("folder claimed DropHandle")
        .complete();
    assert_eq!(outcome.load(Ordering::Relaxed), 1);

    // 6. Drop onto toolbar (unclaimed) -> DropHandle automatically rejects without finishing.
    let unclaimed_outcome = Arc::new(AtomicU8::new(0));
    let unclaimed_cb = Arc::clone(&unclaimed_outcome);
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Dropped {
        offer: DragOfferId::new(11),
        mime_types: vec!["text/uri-list".to_owned()],
        source_actions: DragActions::COPY,
        action: Some(DragAction::Copy),
        paths: vec![PathBuf::from("/tmp/ignored.txt")],
        payload: None,
        position: at(10.0, 15.0),
        handle: DropHandle::new(move |ok| {
            unclaimed_cb.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
        }),
    }));
    app.settle(8);
    assert_eq!(
        unclaimed_outcome.load(Ordering::Relaxed),
        2,
        "an unclaimed drop handle must reject rather than acknowledge success"
    );
}

#[test]
fn non_file_multi_mime_drop_negotiates_and_reads_secondary_mime() {
    let primary_bytes: Rc<RefCell<Option<Vec<u8>>>> = Rc::default();
    let primary_in_view = Rc::clone(&primary_bytes);
    let secondary_text: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
    let secondary_in_view = Arc::clone(&secondary_text);

    let mut app = support::app(NEGOTIATION_CSS, move |cx: &mut BuildCx<'_>| {
        let primary_slot = Rc::clone(&primary_in_view);
        let secondary_slot = Arc::clone(&secondary_in_view);
        Box::new(
            zgui_elements::column()
                .child(
                    zgui_elements::r#box()
                        .class("toolbar")
                        .on(events::DRAG_OVER, |ev| {
                            if let Some(mime) = ev.best_image_mime() {
                                ev.accept_drag(DragDecision::accept(
                                    mime,
                                    DragActions::COPY,
                                    Some(DragAction::Copy),
                                ));
                            }
                        })
                        .on(events::DROP, move |ev| {
                            assert!(ev.paths.is_empty());
                            *primary_slot.borrow_mut() =
                                ev.payload_for("image/png").map(<[u8]>::to_vec);
                            let handle = ev.take_drop_handle().expect("drop handle claimed");
                            let secondary = Arc::clone(&secondary_slot);
                            handle.read_mime("text/plain;charset=utf-8", 256, move |extra| {
                                *secondary.lock().expect("lock") =
                                    extra.map(|m| m.to_utf8_lossy().into_owned());
                            });
                            handle.complete();
                        }),
                )
                .into_view()
                .build(cx),
        )
    });
    app.settle(8);

    let surface = app.platform().offscreens()[0].clone();
    let offer = DragOfferId::new(20);
    let mimes = vec![
        "image/jpeg".to_owned(),
        "image/png".to_owned(),
        "text/plain;charset=utf-8".to_owned(),
    ];
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Entered {
        offer,
        mime_types: mimes.clone(),
        source_actions: DragActions::COPY,
        action: Some(DragAction::Copy),
        paths: Vec::new(),
        position: at(10.0, 15.0),
    }));
    app.settle(8);
    assert_eq!(
        surface.take_drag_responses(),
        vec![(
            offer,
            DragDecision::accept("image/png", DragActions::COPY, Some(DragAction::Copy)),
        )]
    );

    let png = vec![0x89, b'P', b'N', b'G', 1, 2, 3];
    let finished = Arc::new(AtomicU8::new(0));
    let finished_cb = Arc::clone(&finished);
    app.deliver_to_first(SurfaceEvent::Drag(DragEvent::Dropped {
        offer,
        mime_types: mimes,
        source_actions: DragActions::COPY,
        action: Some(DragAction::Copy),
        paths: Vec::new(),
        payload: Some(DragMime::new("image/png", png.clone())),
        position: at(10.0, 15.0),
        handle: DropHandle::new(move |ok| {
            finished_cb.store(if ok { 1 } else { 2 }, Ordering::Relaxed);
        })
        .with_reader(|mime, limit, then| {
            if mime.eq_ignore_ascii_case("text/plain;charset=utf-8") {
                let mut bytes = b"photo caption".to_vec();
                bytes.truncate(limit);
                then(Some(DragMime::new(mime, bytes)));
            } else {
                then(None);
            }
        }),
    }));
    app.settle(8);

    assert_eq!(*primary_bytes.borrow(), Some(png));
    assert_eq!(
        secondary_text.lock().expect("lock").as_deref(),
        Some("photo caption")
    );
    assert_eq!(finished.load(Ordering::Relaxed), 1);
}
