//! Non-file multi-MIME incoming drags negotiate binary payloads on hover, deliver the negotiated
//! payload on drop, and allow reading additional MIME types from the live `DropHandle` before
//! acknowledging completion with `wl_data_offer.finish`.

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
    use std::sync::mpsc;

    use zgui_platform::{
        AppHandler, DragAction, DragActions, DragDecision, DragMime, IdlePolicy, PlatformCx,
        SurfaceEvent, SurfaceId, WakeReason,
    };
    use zgui_runtime::App;
    use zgui_view::event::events;
    use zgui_view::{BuildCx, IntoView, View};

    const CSS: &str = "root { display: block; width: 100%; height: 100% }
                       .canvas-drop { display: block; width: 100%; height: 100% }";

    const PNG_MIME: &str = "image/png";
    const PNG_BYTES: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3, 4];
    const TEXT_MIME: &str = "text/plain;charset=utf-8";
    const TEXT_BYTES: &[u8] = b"caption: sunset photo";
    const CUSTOM_MIME: &str = "application/x-zgui-card";
    const CUSTOM_BYTES: &[u8] = b"{\"id\":42}";
    const TO: (u32, u32) = (250, 500);

    #[derive(Default)]
    struct ReceivedDrop {
        primary: Option<DragMime>,
        caption: Option<DragMime>,
        custom: Option<DragMime>,
        missing: Option<DragMime>,
        had_paths: bool,
    }

    static RECEIVED: Mutex<ReceivedDrop> = Mutex::new(ReceivedDrop {
        primary: None,
        caption: None,
        custom: None,
        missing: None,
        had_paths: false,
    });
    static OUTCOME: Mutex<Option<Result<support::drag_source::Outcome, String>>> = Mutex::new(None);

    struct MimeDropOnWindow<A: AppHandler> {
        inner: A,
        started: bool,
        waker: Option<std::sync::Arc<dyn zgui_platform::Waker>>,
    }

    impl<A: AppHandler> AppHandler for MimeDropOnWindow<A> {
        fn surfaces_available(&mut self, cx: &dyn PlatformCx) {
            assert!(cx.capabilities().drop_mime);
            self.waker.get_or_insert_with(|| cx.waker());
            self.inner.surfaces_available(cx);
        }

        fn surface_event(&mut self, cx: &dyn PlatformCx, surface: SurfaceId, event: SurfaceEvent) {
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
                let outcome = support::drag_source::drag_entries(
                    TO,
                    &[
                        (PNG_MIME, PNG_BYTES),
                        (TEXT_MIME, TEXT_BYTES),
                        (CUSTOM_MIME, CUSTOM_BYTES),
                    ],
                );
                *OUTCOME.lock().expect("outcome lock") = Some(outcome);
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
        const PROPERTY: &str = "non-file multi-MIME drops transfer binary payloads and read additional MIME types via DropHandle";

        if !support::require_nested(PROPERTY) {
            return;
        }

        support::tracing();
        support::watchdog(PROPERTY);

        let runtime = App::new()
            .with_title("drag mime")
            .with_application_id(support::APP_ID)
            .with_size(400.0, 300.0)
            .with_stylesheet(CSS)
            .with_renderer(support::renderers())
            .into_handler(|cx: &mut BuildCx<'_>| {
                Box::new(
                    zgui_elements::column()
                        .class("canvas-drop")
                        .on(events::DRAG_OVER, |ev| {
                            if let Some(image_mime) = ev.best_image_mime() {
                                ev.accept_drag(DragDecision::accept(
                                    image_mime,
                                    DragActions::COPY,
                                    Some(DragAction::Copy),
                                ));
                            } else {
                                ev.accept_drag(DragDecision::reject());
                            }
                        })
                        .on(events::DROP, |ev| {
                            {
                                let mut record = RECEIVED.lock().expect("received lock");
                                record.primary = ev.payload.clone();
                                record.had_paths = !ev.paths.is_empty();
                            }
                            let handle = ev
                                .take_drop_handle()
                                .expect("drop listener should claim DropHandle");
                            assert!(
                                handle.can_read_mime(),
                                "Wayland DropHandle should support additional MIME reads"
                            );
                            // Exercise off-thread secondary MIME reads + delayed complete().
                            std::thread::spawn(move || {
                                let (tx_caption, rx_caption) = mpsc::channel();
                                handle.read_mime(TEXT_MIME, 4096, move |payload| {
                                    let _ = tx_caption.send(payload);
                                });
                                let caption = rx_caption.recv().ok().flatten();

                                let (tx_custom, rx_custom) = mpsc::channel();
                                handle.read_mime(CUSTOM_MIME, 4096, move |payload| {
                                    let _ = tx_custom.send(payload);
                                });
                                let custom = rx_custom.recv().ok().flatten();

                                let (tx_missing, rx_missing) = mpsc::channel();
                                handle.read_mime("application/x-not-offered", 4096, move |p| {
                                    let _ = tx_missing.send(p);
                                });
                                let missing = rx_missing.recv().ok().flatten();

                                {
                                    let mut record = RECEIVED.lock().expect("received lock");
                                    record.caption = caption;
                                    record.custom = custom;
                                    record.missing = missing;
                                }
                                handle.complete();
                            });
                        })
                        .into_view()
                        .build(cx),
                )
            });

        let handler = MimeDropOnWindow {
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

        let outcome = match OUTCOME.lock().expect("outcome lock").take() {
            Some(Ok(outcome)) => outcome,
            Some(Err(reason)) => {
                support::skipped(PROPERTY, &format!("no drag could start here: {reason}"));
                return;
            }
            None => panic!("the drag source thread never reported"),
        };

        assert!(
            outcome.performed && outcome.finished && !outcome.cancelled && !outcome.rejected,
            "multi-MIME drag should finish cleanly after all reads complete: {outcome:?}"
        );
        assert_eq!(
            outcome.sent_mimes,
            vec![
                PNG_MIME.to_owned(),
                TEXT_MIME.to_owned(),
                CUSTOM_MIME.to_owned(),
            ],
            "source should serve the negotiated PNG plus both secondary DropHandle reads: {outcome:?}"
        );

        let received = RECEIVED.lock().expect("received lock");
        assert!(!received.had_paths, "binary image drop must have no paths");
        assert_eq!(
            received.primary,
            Some(DragMime::new(PNG_MIME, PNG_BYTES)),
            "primary negotiated binary PNG payload did not match"
        );
        assert_eq!(
            received.caption,
            Some(DragMime::new(TEXT_MIME, TEXT_BYTES)),
            "secondary UTF-8 text payload read via DropHandle did not match"
        );
        assert_eq!(
            received.custom,
            Some(DragMime::new(CUSTOM_MIME, CUSTOM_BYTES)),
            "secondary custom MIME payload read via DropHandle did not match"
        );
        assert_eq!(
            received.missing, None,
            "unoffered MIME must return None without failing"
        );

        support::passed(
            PROPERTY,
            "received binary image/png + secondary text/plain and custom MIME via DropHandle before finish",
        );
    }
}
