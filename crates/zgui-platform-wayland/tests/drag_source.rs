//! A native drag started from a zgui window transfers its payload to another Wayland client
//! while leaving the existing clipboard selection intact.

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

    use std::sync::Mutex;
    use std::sync::atomic::Ordering;

    use wayland_client::protocol::wl_data_device_manager::DndAction;
    use zgui_geom::{CssPx, Point};
    use zgui_platform::{
        AppHandler, ClipboardData, ClipboardKind, ClipboardWriteOptions, DragAction, DragActions,
        DragIcon, DragMime, DragSessionId, DragSourceEvent, DragSourceRequest, IdlePolicy,
        PlatformCx, SurfaceEvent, SurfaceId, WakeReason, WindowIcon,
    };
    use zgui_vocab::PointerAction;

    const CLIPBOARD_TEXT: &str = "clipboard survives drag";
    const DRAG_MIME: &str = "text/uri-list";
    const DRAG_PAYLOAD: &str = "file:///tmp/zgui%20dragged.txt\r\n";
    const DRAG_TEXT_MIME: &str = "text/plain;charset=utf-8";
    const DRAG_TEXT_PAYLOAD: &str = "/tmp/zgui dragged.txt";

    static SOURCE_EVENTS: Mutex<Vec<DragSourceEvent>> = Mutex::new(Vec::new());
    static SESSION: Mutex<Option<DragSessionId>> = Mutex::new(None);
    static RECEIVED: Mutex<Option<Result<support::drop_target::Received, String>>> =
        Mutex::new(None);

    struct DragFromWindow<A: AppHandler> {
        inner: A,
        spawned: bool,
        clipboard_written: bool,
        presses: u32,
        drag_started: bool,
        waker: Option<std::sync::Arc<dyn zgui_platform::Waker>>,
    }

    impl<A: AppHandler> AppHandler for DragFromWindow<A> {
        fn surfaces_available(&mut self, cx: &dyn PlatformCx) {
            assert!(
                cx.capabilities().drag_source,
                "Wayland backend should advertise drag_source when wl_data_device is available"
            );
            self.waker.get_or_insert_with(|| cx.waker());
            self.inner.surfaces_available(cx);
        }

        fn surface_event(&mut self, cx: &dyn PlatformCx, surface: SurfaceId, event: SurfaceEvent) {
            let drawn = matches!(event, SurfaceEvent::RedrawRequested);
            match &event {
                SurfaceEvent::Pointer {
                    action: PointerAction::Pressed,
                    ..
                } => {
                    self.presses += 1;
                    if !self.clipboard_written {
                        let write = cx.clipboard().write(
                            ClipboardKind::Standard,
                            ClipboardData::Text(CLIPBOARD_TEXT.into()),
                            ClipboardWriteOptions::default(),
                        );
                        assert!(write.is_ok(), "pre-drag clipboard write failed: {write:?}");
                        self.clipboard_written = true;
                    }
                }
                SurfaceEvent::Pointer {
                    action: PointerAction::Moved,
                    ..
                } if self.presses >= 2 && !self.drag_started => {
                    if let Some(target_surface) = cx.surface(surface) {
                        // Attach a translucent 32x32 HiDPI (2x) icon with a non-zero hotspot to
                        // exercise SHM upload, premultiplication, set_buffer_scale, and v5+ offset.
                        let mut rgba = Vec::with_capacity(32 * 32 * 4);
                        for _ in 0..(32 * 32) {
                            rgba.extend_from_slice(&[64, 144, 255, 192]);
                        }
                        let icon = DragIcon::new(
                            WindowIcon::from_rgba(rgba, 32, 32).expect("valid drag icon"),
                        )
                        .with_hotspot(Point::new(CssPx(8.0), CssPx(6.0)))
                        .with_scale(2);

                        let session = target_surface
                            .start_drag(
                                DragSourceRequest::new(
                                    vec![
                                        DragMime::new(DRAG_MIME, DRAG_PAYLOAD.as_bytes()),
                                        DragMime::new(DRAG_TEXT_MIME, DRAG_TEXT_PAYLOAD.as_bytes()),
                                    ],
                                    DragActions::COPY,
                                )
                                .with_icon(icon),
                            )
                            .expect("start_drag should succeed on an active pointer press");
                        *SESSION.lock().expect("session lock") = Some(session);
                        self.drag_started = true;
                    }
                }
                SurfaceEvent::DragSource(source_event) => {
                    SOURCE_EVENTS
                        .lock()
                        .expect("events lock")
                        .push(source_event.clone());
                }
                _ => {}
            }

            self.inner.surface_event(cx, surface, event);

            if !drawn || self.spawned {
                return;
            }
            self.spawned = true;
            let Some(waker) = self.waker.clone() else {
                return;
            };
            std::thread::spawn(move || {
                let result = support::drop_target::receive_drag(DRAG_MIME);
                *RECEIVED.lock().expect("received lock") = Some(result);
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
        const PROPERTY: &str = "a drag started from a zgui window transfers its payload without touching the clipboard";

        if !support::require_nested(PROPERTY) {
            return;
        }

        support::tracing();
        support::watchdog(PROPERTY);

        let handler = DragFromWindow {
            inner: match support::application("drag source") {
                Ok(runtime) => runtime,
                Err(error) => {
                    support::skipped(PROPERTY, &format!("the runtime would not install: {error}"));
                    return;
                }
            },
            spawned: false,
            clipboard_written: false,
            presses: 0,
            drag_started: false,
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

        let received = match RECEIVED.lock().expect("received lock").take() {
            Some(Ok(received)) => received,
            Some(Err(reason)) => {
                support::skipped(
                    PROPERTY,
                    &format!("no drop target could run here: {reason}"),
                );
                return;
            }
            None => panic!("the drop target thread never reported"),
        };

        let session = SESSION
            .lock()
            .expect("session lock")
            .expect("start_drag should have produced a session id");
        let events = SOURCE_EVENTS.lock().expect("events lock").clone();

        assert!(
            received.offered_mimes.iter().any(|mime| mime == DRAG_MIME),
            "target did not see {DRAG_MIME}: {received:?}"
        );
        assert!(
            received
                .offered_mimes
                .iter()
                .any(|mime| mime == DRAG_TEXT_MIME),
            "target did not see {DRAG_TEXT_MIME}: {received:?}"
        );
        assert_eq!(
            received.source_actions,
            Some(DndAction::Copy),
            "target did not see Copy action: {received:?}"
        );
        assert_eq!(
            received.selected_action,
            Some(DndAction::Copy),
            "compositor did not settle on Copy: {received:?}"
        );
        assert_eq!(
            received.payload.as_deref(),
            Some(DRAG_PAYLOAD.as_bytes()),
            "target did not receive the exact drag payload: {received:?}"
        );
        assert_eq!(
            received.clipboard_after.as_deref(),
            Some(CLIPBOARD_TEXT),
            "dragging must leave the pre-existing clipboard selection intact: {received:?}"
        );
        assert!(
            events.contains(&DragSourceEvent::TargetMime {
                session,
                mime_type: Some(DRAG_MIME.to_owned()),
            }),
            "source did not observe TargetMime: {events:?}"
        );
        assert!(
            events.contains(&DragSourceEvent::ActionChanged {
                session,
                action: Some(DragAction::Copy),
            }),
            "source did not observe ActionChanged(Copy): {events:?}"
        );
        assert!(
            events.contains(&DragSourceEvent::Dropped {
                session,
                action: Some(DragAction::Copy),
            }),
            "source did not observe Dropped: {events:?}"
        );
        assert!(
            events.contains(&DragSourceEvent::Finished {
                session,
                action: Some(DragAction::Copy),
            }),
            "source did not observe Finished: {events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, DragSourceEvent::Cancelled { .. })),
            "source should not report Cancelled after Finished: {events:?}"
        );

        support::passed(
            PROPERTY,
            "target read text/uri-list with multi-MIME + 2x drag icon, source observed TargetMime/ActionChanged/Dropped/Finished, clipboard intact",
        );
    }
}
