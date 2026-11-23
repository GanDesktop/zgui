//! Incoming Wayland drags negotiate MIME/actions dynamically through real view `drag_over`
//! listeners without pre-reading the pipe on hover, and only send `wl_data_offer.finish` once
//! the `drop` listener explicitly completes the `DropHandle` claimed via `EventCx::take_drop_handle`.

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
#[path = "support/mod.rs"]
mod support;

fn main() {
    #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
    ))]
    supported::main();
}

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
mod supported {
    use super::support;

    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use wayland_client::protocol::wl_data_device_manager::DndAction;
    use zgui_platform::{
        AppHandler, DragAction, DragActions, DragDecision, DragEvent, IdlePolicy, PlatformCx,
        SurfaceEvent, SurfaceId, WakeReason,
    };
    use zgui_runtime::App;
    use zgui_view::event::events;
    use zgui_view::{BuildCx, IntoView, View};

    const CSS: &str = "root { display: block; width: 100%; height: 100% }
                       .drop-zone { display: block; width: 100%; height: 100% }";

    const TEXT: &str = "a few dragged words";
    const PAYLOAD_REJECTED: &str = "file:///tmp/zgui%20cancelled-move\r\n";
    const PAYLOAD_ACCEPTED: &str = "file:///tmp/zgui%20dropped\r\n";
    const DROPPED_REJECTED: &str = "/tmp/zgui cancelled-move";
    const DROPPED_ACCEPTED: &str = "/tmp/zgui dropped";
    const TO: (u32, u32) = (250, 500);

    static ENTERED: AtomicBool = AtomicBool::new(false);
    static DROPPED_RECORDS: Mutex<Vec<(Option<DragAction>, Vec<PathBuf>)>> = Mutex::new(Vec::new());
    static OUTCOMES: Mutex<Option<Result<Outcomes, String>>> = Mutex::new(None);

    type Outcomes = (
        support::drag_source::Outcome,
        support::drag_source::Outcome,
        support::drag_source::Outcome,
    );

    struct DropOnWindow<A: AppHandler> {
        inner: A,
        started: bool,
        waker: Option<std::sync::Arc<dyn zgui_platform::Waker>>,
    }

    impl<A: AppHandler> AppHandler for DropOnWindow<A> {
        fn surfaces_available(&mut self, cx: &dyn PlatformCx) {
            assert!(
                cx.capabilities().drop_mime,
                "Wayland backend should advertise drop_mime when wl_data_device is available"
            );
            self.waker.get_or_insert_with(|| cx.waker());
            self.inner.surfaces_available(cx);
        }

        fn surface_event(&mut self, cx: &dyn PlatformCx, surface: SurfaceId, event: SurfaceEvent) {
            if let SurfaceEvent::Drag(DragEvent::Entered { ref paths, .. }) = event {
                ENTERED.store(true, Ordering::Relaxed);
                assert!(
                    paths.is_empty(),
                    "Wayland Entered must not pre-read paths over the pipe"
                );
            }
            let drawn = matches!(event, SurfaceEvent::RedrawRequested);
            self.inner.surface_event(cx, surface, event);
            if !drawn || self.started {
                return;
            }
            self.started = true;
            let Some(waker) = self.waker.clone() else {
                return;
            };
            std::thread::spawn(move || {
                let outcomes = support::drag_source::drag(TO, "text/plain;charset=utf-8", TEXT)
                    .and_then(|text| {
                        support::drag_source::drag(TO, "text/uri-list", PAYLOAD_REJECTED).and_then(
                            |rejected_move| {
                                support::drag_source::drag(TO, "text/uri-list", PAYLOAD_ACCEPTED)
                                    .map(|accepted_copy| (text, rejected_move, accepted_copy))
                            },
                        )
                    });
                *OUTCOMES.lock().expect("the record is not poisoned") = Some(outcomes);
                zgui_platform::Waker::wake(waker.as_ref(), WakeReason::DeviceLost);
            });
        }

        fn wake(&mut self, cx: &dyn PlatformCx, reason: WakeReason) {
            if matches!(reason, WakeReason::DeviceLost) {
                cx.request_exit();
            }
            self.inner.wake(cx, reason);
        }

        fn idle(&mut self, cx: &dyn PlatformCx) -> IdlePolicy {
            self.inner.idle(cx)
        }

        fn deadline_reached(&mut self, cx: &dyn PlatformCx) {
            self.inner.deadline_reached(cx);
        }

        fn shutting_down(&mut self, cx: &dyn PlatformCx) {
            self.inner.shutting_down(cx);
        }
    }

    pub(super) fn main() {
        const PROPERTY: &str = "incoming drags negotiate dynamically and only finish when DropHandle::complete is called";

        if !support::require_nested(PROPERTY) {
            return;
        }

        support::tracing();
        support::watchdog(PROPERTY);

        let runtime = App::new()
            .with_title("file drop")
            .with_application_id(support::APP_ID)
            .with_size(400.0, 300.0)
            .with_stylesheet(CSS)
            .with_renderer(support::renderers())
            .into_handler(|cx: &mut BuildCx<'_>| {
                Box::new(
                    zgui_elements::column()
                        .class("drop-zone")
                        .on(events::DRAG_OVER, |ev| {
                            if !ev.offers_files() {
                                ev.accept_drag(DragDecision::reject());
                                return;
                            }
                            let seen = DROPPED_RECORDS.lock().expect("records lock").len();
                            if seen == 0 {
                                // First file drag negotiates Move, then rejects on drop.
                                ev.accept_drag(DragDecision::accept(
                                    "text/uri-list",
                                    DragActions::COPY | DragActions::MOVE,
                                    Some(DragAction::Move),
                                ));
                            } else {
                                // Second file drag negotiates Copy, then completes on drop.
                                ev.accept_drag(DragDecision::accept(
                                    "text/uri-list",
                                    DragActions::COPY,
                                    Some(DragAction::Copy),
                                ));
                            }
                        })
                        .on(events::DROP, |ev| {
                            let handle = ev
                                .take_drop_handle()
                                .expect("first drop listener should claim DropHandle");
                            assert!(
                                ev.take_drop_handle().is_none(),
                                "DropHandle must only be claimed once"
                            );
                            let count = {
                                let mut records = DROPPED_RECORDS.lock().expect("records lock");
                                records.push((ev.action, ev.paths.clone()));
                                records.len()
                            };
                            if count == 1 {
                                handle.reject();
                            } else {
                                handle.complete();
                            }
                        })
                        .into_view()
                        .build(cx),
                )
            });

        let handler = DropOnWindow {
            inner: match runtime {
                Ok(runtime) => runtime,
                Err(error) => {
                    support::skipped(PROPERTY, &format!("the runtime would not install: {error}"));
                    return;
                }
            },
            started: false,
            waker: None,
        };

        let Some(mut app) = support::loop_for(PROPERTY, handler) else {
            return;
        };
        app.run().expect("the loop ran");

        if support::NO_DEVICE.load(Ordering::Relaxed) {
            support::skipped(
                PROPERTY,
                "this machine has no graphics device to draw through",
            );
            return;
        }
        let (text, rejected_move, accepted_copy) =
            match OUTCOMES.lock().expect("the record is not poisoned").take() {
                Some(Ok(outcomes)) => outcomes,
                Some(Err(reason)) => {
                    support::skipped(PROPERTY, &format!("no drag could start here: {reason}"));
                    return;
                }
                None => panic!("the source never reported"),
            };

        assert!(
            !text.claimed && text.cancelled && !text.sent,
            "unaccepted text drag must stay unclaimed and transfer zero bytes: {text:?}"
        );
        assert!(
            ENTERED.load(Ordering::Relaxed),
            "no drag entered the window"
        );
        assert!(
            rejected_move.performed && rejected_move.rejected && !rejected_move.finished,
            "rejecting a Move DropHandle must cancel without sending dnd_finished: {rejected_move:?}"
        );
        assert_eq!(
            rejected_move.action,
            Some(DndAction::Move),
            "first file drag should have negotiated Move: {rejected_move:?}"
        );
        assert!(
            !accepted_copy.cancelled && accepted_copy.performed && accepted_copy.finished,
            "completing a Copy DropHandle must send dnd_finished: {accepted_copy:?}"
        );
        assert_eq!(
            accepted_copy.action,
            Some(DndAction::Copy),
            "second file drag should have negotiated Copy: {accepted_copy:?}"
        );

        let records = DROPPED_RECORDS.lock().expect("records lock").clone();
        assert_eq!(
            records,
            vec![
                (
                    Some(DragAction::Move),
                    vec![PathBuf::from(DROPPED_REJECTED)]
                ),
                (
                    Some(DragAction::Copy),
                    vec![PathBuf::from(DROPPED_ACCEPTED)]
                ),
            ],
            "destination did not observe the expected negotiated actions and paths"
        );
        support::passed(
            PROPERTY,
            "text rejected with 0 bytes sent; rejected Move never sent finish; completed Copy finished cleanly",
        );
    }
}
