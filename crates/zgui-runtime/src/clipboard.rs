//! The desktop's clipboards, reached from application code.
//!
//! The object that can touch a clipboard is borrowed from the platform and is valid only while the
//! loop is inside a callback, which is nowhere a component runs. So this is a queue: a component
//! asks, the request is carried out on the next turn of the loop with the platform in hand, and a
//! read's answer comes back the same way.
//!
//! Reads never block. A selection belongs to whichever application last set it, and on a desktop
//! that answers over its own socket the owner may be slow, stopped, or gone — so a read is started
//! and answered later rather than waited for.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll, Waker as TaskWaker};

use rustc_hash::FxHashMap;
use zgui_platform::{
    ClipboardData, ClipboardError, ClipboardFormat, ClipboardKind, ClipboardMime,
    ClipboardMimeOffer, ClipboardSerial, ClipboardWriteOptions, LazyMimeProvider, PlatformCx,
    WakeReason, Waker,
};
use zgui_reactive::prelude::*;
use zgui_reactive::{LocalStorage, RwSignal, Signal};
use zgui_vocab::SharedString;

/// What a started read calls when its answer arrives.
type ReadCallback = Box<dyn FnOnce(Option<String>)>;
/// What a started multi-MIME read calls when its answer arrives.
type ReadMimeCallback = Box<dyn FnOnce(Result<Option<ClipboardMimeOffer>, ClipboardError>)>;
/// What a queued multi-MIME write calls after running.
type WriteMimeCallback = Box<dyn FnOnce(Result<(), ClipboardError>)>;

struct OpSlot<T> {
    ready: Option<T>,
    waker: Option<TaskWaker>,
}

impl<T> OpSlot<T> {
    fn new_shared() -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(Self {
            ready: None,
            waker: None,
        }))
    }

    fn complete(slot: &Rc<RefCell<Self>>, value: T) {
        let mut inner = slot.borrow_mut();
        inner.ready = Some(value);
        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }
    }
}

struct OpFuture<T>(Rc<RefCell<OpSlot<T>>>);

impl<T> Future for OpFuture<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut inner = self.0.borrow_mut();
        if let Some(value) = inner.ready.take() {
            Poll::Ready(value)
        } else {
            inner.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// One thing asked of a clipboard.
enum ClipboardRequest {
    /// Put this on that clipboard.
    Write {
        /// Which clipboard.
        kind: ClipboardKind,
        /// What goes on it.
        data: ClipboardData,
        /// How it goes on.
        options: ClipboardWriteOptions,
    },
    /// Replace `kind` with `entries`, validating `expected_revision`.
    WriteMime {
        /// Which clipboard.
        kind: ClipboardKind,
        /// Entries to offer on the clipboard (empty clears it).
        entries: Vec<ClipboardMime>,
        /// Optional compare-and-swap revision.
        expected_revision: Option<u64>,
        /// Continuation notified with the write outcome.
        then: WriteMimeCallback,
    },
    /// Replace `kind` with eager `entries` and lazy `providers`, validating `expected_revision`.
    WriteMimeProviders {
        /// Which clipboard.
        kind: ClipboardKind,
        /// Eager entries to offer on the clipboard.
        entries: Vec<ClipboardMime>,
        /// Lazily produced entries evaluated on demand when a peer requests them.
        lazy: Vec<(String, LazyMimeProvider)>,
        /// Optional compare-and-swap revision.
        expected_revision: Option<u64>,
        /// Continuation notified with the write outcome.
        then: WriteMimeCallback,
    },
    /// Empty that clipboard.
    Clear {
        /// Which clipboard.
        kind: ClipboardKind,
    },
    /// Read that clipboard, and call this back with what was on it.
    Read {
        /// Which clipboard.
        kind: ClipboardKind,
        /// What to call with the answer.
        then: ReadCallback,
    },
    /// Read matching MIME payloads from a single clipboard offer.
    ReadMime {
        /// Which clipboard.
        kind: ClipboardKind,
        /// Requested MIME names in priority order.
        mime_types: Vec<String>,
        /// Cumulative payload byte cap.
        max_bytes: usize,
        /// Continuation notified with the matched offer.
        then: ReadMimeCallback,
    },
}

/// The desktop's clipboards.
///
/// Cheap to clone, and reachable from anywhere inside a running application through
/// [`use_clipboard`]. Bound to the UI thread: work on another thread reaches this by posting back
/// with [`task::ui`](zgui_reactive::task::ui).
///
/// Every operation this desktop cannot carry out does nothing rather than failing, which is what
/// keeps a caller from needing a branch per platform. The clipboard a desktop has no equivalent for
/// — [`ClipboardKind::Primary`], away from Linux — is the common case:
/// [`PlatformCapabilities::clipboard_primary_selection`](zgui_platform::PlatformCapabilities) says
/// whether it is there, for a caller that would rather offer nothing than offer a command that does
/// nothing.
#[derive(Clone, Default)]
pub struct Clipboards {
    /// The shared state.
    inner: Rc<RefCell<Inner>>,
}

// By hand: the queue holds callbacks, which are not `Debug`, and what a reader wants is how much is
// outstanding rather than what it is.
impl core::fmt::Debug for Clipboards {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let inner = self.inner.borrow();
        formatter
            .debug_struct("Clipboards")
            .field("queued", &inner.queue.len())
            .field("pending_reads", &inner.pending.len())
            .finish()
    }
}

/// What a [`Clipboards`] holds.
#[derive(Default)]
struct Inner {
    /// What has been asked for and not yet carried out.
    queue: VecDeque<ClipboardRequest>,
    /// Reads that have been started and not yet answered.
    pending: FxHashMap<ClipboardSerial, ReadCallback>,
    /// Multi-MIME reads that have been started and not yet answered.
    pending_mime: FxHashMap<ClipboardSerial, ReadMimeCallback>,
    /// How to wake the loop, once there is a loop to wake.
    platform: Option<Arc<dyn Waker>>,
}

impl Clipboards {
    /// An empty queue, with no loop to wake yet.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Puts `text` on `kind`.
    ///
    /// ```no_run
    /// use zgui_platform::ClipboardKind;
    /// use zgui_runtime::clipboard::use_clipboard;
    ///
    /// # fn example(selected: String) {
    /// // Copy-on-select, the way a terminal or an editor offers it on Linux.
    /// use_clipboard().set_text(ClipboardKind::Primary, selected);
    /// # }
    /// ```
    pub fn set_text(&self, kind: ClipboardKind, text: impl Into<SharedString>) {
        self.push(ClipboardRequest::Write {
            kind,
            data: ClipboardData::Text(text.into()),
            options: ClipboardWriteOptions::default(),
        });
    }

    /// Reads `kind`, and calls `then` with what was on it.
    ///
    /// The callback runs later, on the UI thread, once the desktop has answered. It is given `None`
    /// when the clipboard is empty, holds something that is not text, or belongs to a desktop that
    /// refuses this kind. An application that stops before the answer arrives drops the callback
    /// without calling it: no answer is better than a made-up one.
    ///
    /// ```no_run
    /// use zgui_platform::ClipboardKind;
    /// use zgui_runtime::clipboard::use_clipboard;
    ///
    /// # fn example() {
    /// use_clipboard().read_text(ClipboardKind::Standard, |text| {
    ///     if let Some(text) = text {
    ///         println!("pasted {text}");
    ///     }
    /// });
    /// # }
    /// ```
    pub fn read_text(&self, kind: ClipboardKind, then: impl FnOnce(Option<String>) + 'static) {
        self.push(ClipboardRequest::Read {
            kind,
            then: Box::new(then),
        });
    }

    /// Reads `kind` into a signal, which holds `None` until the answer arrives.
    ///
    /// The reactive shape of [`read_text`](Self::read_text), for a view that shows what was on the
    /// clipboard rather than acting on it. The signal keeps `None` when the read finds nothing, so
    /// a view cannot tell "not answered yet" from "nothing there" — a caller that must
    /// distinguish the two uses [`read_text`](Self::read_text) and writes its own state.
    pub fn read_text_signal(&self, kind: ClipboardKind) -> Signal<Option<String>, LocalStorage> {
        let answer = RwSignal::new_local(None);
        self.read_text(kind, move |text| {
            if let Some(text) = text {
                answer.set(Some(text));
            }
        });
        answer.into()
    }

    /// Empties `kind`.
    pub fn clear(&self, kind: ClipboardKind) {
        self.push(ClipboardRequest::Clear { kind });
    }

    /// Reads requested `mime_types` atomically from the current offer on `kind`.
    ///
    /// Resolves to `Ok(None)` when `kind` has no active selection offer.
    pub fn read_mime(
        &self,
        kind: ClipboardKind,
        mime_types: Vec<String>,
        max_bytes: usize,
    ) -> impl Future<Output = Result<Option<ClipboardMimeOffer>, ClipboardError>> + 'static {
        let slot = OpSlot::new_shared();
        let complete_slot = Rc::clone(&slot);
        self.push(ClipboardRequest::ReadMime {
            kind,
            mime_types,
            max_bytes,
            then: Box::new(move |result| OpSlot::complete(&complete_slot, result)),
        });
        OpFuture(slot)
    }

    /// Atomically replaces all offered MIME types on `kind`, checking `expected_revision` when
    /// provided.
    ///
    /// An empty `entries` vector clears the clipboard.
    pub fn write_mime(
        &self,
        kind: ClipboardKind,
        entries: Vec<ClipboardMime>,
        expected_revision: Option<u64>,
    ) -> impl Future<Output = Result<(), ClipboardError>> + 'static {
        let slot = OpSlot::new_shared();
        let complete_slot = Rc::clone(&slot);
        self.push(ClipboardRequest::WriteMime {
            kind,
            entries,
            expected_revision,
            then: Box::new(move |result| OpSlot::complete(&complete_slot, result)),
        });
        OpFuture(slot)
    }

    /// Atomically replaces all offered MIME types on `kind` with eager `entries` and lazily
    /// generated `lazy` providers, checking `expected_revision` when provided.
    ///
    /// When both `entries` and `lazy` are empty, clears the clipboard.
    pub fn write_mime_with_providers(
        &self,
        kind: ClipboardKind,
        entries: Vec<ClipboardMime>,
        lazy: Vec<(String, LazyMimeProvider)>,
        expected_revision: Option<u64>,
    ) -> impl Future<Output = Result<(), ClipboardError>> + 'static {
        let slot = OpSlot::new_shared();
        let complete_slot = Rc::clone(&slot);
        self.push(ClipboardRequest::WriteMimeProviders {
            kind,
            entries,
            lazy,
            expected_revision,
            then: Box::new(move |result| OpSlot::complete(&complete_slot, result)),
        });
        OpFuture(slot)
    }

    /// Queues one request and wakes the loop.
    fn push(&self, request: ClipboardRequest) {
        self.inner.borrow_mut().queue.push_back(request);
        self.ping();
    }

    /// Whether nothing is waiting to be carried out.
    pub(crate) fn is_empty(&self) -> bool {
        self.inner.borrow().queue.is_empty()
    }

    /// Installs the platform's waker, once there is a loop to wake.
    pub(crate) fn set_platform(&self, waker: Arc<dyn Waker>) {
        self.inner.borrow_mut().platform = Some(waker);
    }

    /// Carries out everything asked for so far.
    ///
    /// One request is taken at a time and the borrow released before the platform is called, since
    /// a backend is free to answer a read at once — and the answer runs a callback, which may ask
    /// for more.
    pub(crate) fn drain(&self, cx: &dyn PlatformCx) {
        loop {
            // A statement of its own, and not the scrutinee of a `while let`: a temporary in a
            // `while let` lives until the end of the body, so the borrow would still be held when
            // the platform answers a read at once and this reaches for it again.
            let Some(request) = self.inner.borrow_mut().queue.pop_front() else {
                break;
            };
            match request {
                ClipboardRequest::Write {
                    kind,
                    data,
                    options,
                } => {
                    if let Err(error) = cx.clipboard().write(kind, data, options) {
                        tracing::debug!(?kind, ?error, "this desktop did not take the copy");
                    }
                }
                ClipboardRequest::Clear { kind } => {
                    if let Err(error) = cx.clipboard().clear(kind) {
                        tracing::debug!(?kind, ?error, "this desktop did not empty the clipboard");
                    }
                }
                ClipboardRequest::WriteMime {
                    kind,
                    entries,
                    expected_revision,
                    then,
                } => {
                    let result = cx.clipboard().write_mime(kind, &entries, expected_revision);
                    then(result);
                }
                ClipboardRequest::WriteMimeProviders {
                    kind,
                    entries,
                    lazy,
                    expected_revision,
                    then,
                } => {
                    let result = cx.clipboard().write_mime_providers(
                        kind,
                        &entries,
                        &lazy,
                        expected_revision,
                    );
                    then(result);
                }
                ClipboardRequest::Read { kind, then } => {
                    let serial = cx.clipboard().read(kind, ClipboardFormat::Text);
                    // Recorded after the read rather than before, because the read is what names
                    // it. That is sound because a wake is delivered through the loop and a handler
                    // holds the runtime exclusively while it runs: the answer cannot be routed
                    // before this call returns.
                    self.inner.borrow_mut().pending.insert(serial, then);
                }
                ClipboardRequest::ReadMime {
                    kind,
                    mime_types,
                    max_bytes,
                    then,
                } => {
                    let serial = cx.clipboard().read_mime(kind, &mime_types, max_bytes);
                    self.inner.borrow_mut().pending_mime.insert(serial, then);
                }
            }
        }
    }

    /// Answers the read `serial` named.
    pub(crate) fn resolve(
        &self,
        serial: ClipboardSerial,
        result: Result<ClipboardData, ClipboardError>,
    ) {
        let Some(then) = self.inner.borrow_mut().pending.remove(&serial) else {
            tracing::debug!(
                ?serial,
                "an answer arrived for a read nothing was waiting on"
            );
            return;
        };
        // Outside the borrow: the callback may start another read.
        then(
            result
                .ok()
                .and_then(|data| data.as_text().map(str::to_owned)),
        );
    }

    /// Answers the multi-MIME read `serial` named.
    pub(crate) fn resolve_mime(
        &self,
        serial: ClipboardSerial,
        result: Result<ClipboardMimeOffer, ClipboardError>,
    ) {
        let Some(then) = self.inner.borrow_mut().pending_mime.remove(&serial) else {
            tracing::debug!(
                ?serial,
                "an answer arrived for a multi-MIME read nothing was waiting on"
            );
            return;
        };
        let mapped = match result {
            Ok(offer) => Ok(Some(offer)),
            Err(ClipboardError::Empty(_)) => Ok(None),
            Err(error) => Err(error),
        };
        then(mapped);
    }

    /// Wakes the loop, so that what was queued is carried out.
    fn ping(&self) {
        let platform = self.inner.borrow().platform.clone();
        if let Some(platform) = platform {
            platform.wake(WakeReason::AppWork);
        }
    }
}

/// The desktop's clipboards.
///
/// # Panics
///
/// Panics when called outside a running application.
pub fn use_clipboard() -> Clipboards {
    try_use_clipboard().expect("this code is not running inside an application")
}

/// The desktop's clipboards, if there is an application running.
pub fn try_use_clipboard() -> Option<Clipboards> {
    zgui_reactive::use_local_context::<Clipboards>()
}

#[cfg(test)]
mod tests {
    use super::Clipboards;
    use std::cell::RefCell;
    use std::rc::Rc;
    use zgui_platform::{ClipboardData, ClipboardError, ClipboardKind, ClipboardSerial};

    #[test]
    fn a_request_is_queued_until_it_is_drained() {
        let clipboards = Clipboards::new();
        assert!(clipboards.is_empty());
        clipboards.set_text(ClipboardKind::Standard, "copied");
        assert!(!clipboards.is_empty());
    }

    #[test]
    fn an_answer_reaches_the_callback_that_started_the_read() {
        let clipboards = Clipboards::new();
        let seen: Rc<RefCell<Option<Option<String>>>> = Rc::new(RefCell::new(None));

        // The read is started by `drain`; here the pending entry is made directly, because the
        // point is what `resolve` does with it.
        let answer = Rc::clone(&seen);
        clipboards.inner.borrow_mut().pending.insert(
            ClipboardSerial::new(1),
            Box::new(move |text| *answer.borrow_mut() = Some(text)),
        );
        clipboards.resolve(ClipboardSerial::new(1), Ok(ClipboardData::from("pasted")));

        assert_eq!(*seen.borrow(), Some(Some("pasted".to_owned())));
    }

    #[test]
    fn a_refused_read_answers_with_nothing_rather_than_failing() {
        let clipboards = Clipboards::new();
        let seen: Rc<RefCell<Option<Option<String>>>> = Rc::new(RefCell::new(None));

        let answer = Rc::clone(&seen);
        clipboards.inner.borrow_mut().pending.insert(
            ClipboardSerial::new(2),
            Box::new(move |text| *answer.borrow_mut() = Some(text)),
        );
        clipboards.resolve(
            ClipboardSerial::new(2),
            Err(ClipboardError::Empty(ClipboardKind::Standard)),
        );

        assert_eq!(*seen.borrow(), Some(None));
    }

    #[test]
    fn an_answer_nothing_is_waiting_on_calls_nothing() {
        let clipboards = Clipboards::new();
        // No pending entry, and no panic.
        clipboards.resolve(ClipboardSerial::new(9), Ok(ClipboardData::from("stray")));
    }

    #[test]
    fn a_read_the_application_stopped_before_answering_drops_its_callback_uncalled() {
        /// Flips a flag when it is dropped.
        struct Flag(Rc<RefCell<bool>>);
        impl Drop for Flag {
            fn drop(&mut self) {
                *self.0.borrow_mut() = true;
            }
        }

        let dropped = Rc::new(RefCell::new(false));
        let called = Rc::new(RefCell::new(false));
        {
            let clipboards = Clipboards::new();
            let flag = Flag(Rc::clone(&dropped));
            let called = Rc::clone(&called);
            clipboards.inner.borrow_mut().pending.insert(
                ClipboardSerial::new(3),
                Box::new(move |_| {
                    let _flag = &flag;
                    *called.borrow_mut() = true;
                }),
            );
        }

        assert!(*dropped.borrow(), "the callback was dropped");
        assert!(!*called.borrow(), "and no answer was made up for it");
    }

    #[test]
    fn mime_read_maps_empty_to_none_and_delivers_offer() {
        use std::pin::pin;
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        use zgui_platform::{ClipboardMime, ClipboardMimeOffer};

        struct NoopWake;
        impl Wake for NoopWake {
            fn wake(self: Arc<Self>) {}
        }

        let waker: Waker = Arc::new(NoopWake).into();
        let mut cx = Context::from_waker(&waker);
        let clipboards = Clipboards::new();

        let fut_empty =
            clipboards.read_mime(ClipboardKind::Standard, vec!["text/uri-list".into()], 64);
        let mut pinned_empty = pin!(fut_empty);
        assert!(matches!(pinned_empty.as_mut().poll(&mut cx), Poll::Pending));
        let req_empty = clipboards
            .inner
            .borrow_mut()
            .queue
            .pop_front()
            .expect("queued");
        if let super::ClipboardRequest::ReadMime { then, .. } = req_empty {
            clipboards
                .inner
                .borrow_mut()
                .pending_mime
                .insert(ClipboardSerial::new(10), then);
        }
        clipboards.resolve_mime(
            ClipboardSerial::new(10),
            Err(ClipboardError::Empty(ClipboardKind::Standard)),
        );
        assert_eq!(pinned_empty.poll(&mut cx), Poll::Ready(Ok(None)));

        let fut_offer =
            clipboards.read_mime(ClipboardKind::Standard, vec!["text/uri-list".into()], 64);
        let pinned_offer = pin!(fut_offer);
        let req_offer = clipboards
            .inner
            .borrow_mut()
            .queue
            .pop_front()
            .expect("queued");
        if let super::ClipboardRequest::ReadMime { then, .. } = req_offer {
            clipboards
                .inner
                .borrow_mut()
                .pending_mime
                .insert(ClipboardSerial::new(11), then);
        }
        let offer = ClipboardMimeOffer {
            revision: 4,
            offered_mime_types: vec!["text/uri-list".to_owned()],
            entries: vec![ClipboardMime::new("text/uri-list", b"file:///tmp/a\r\n")],
        };
        clipboards.resolve_mime(ClipboardSerial::new(11), Ok(offer.clone()));
        assert_eq!(pinned_offer.poll(&mut cx), Poll::Ready(Ok(Some(offer))));
    }

    #[test]
    fn write_mime_with_providers_queues_providers_request() {
        use std::sync::Arc;
        use zgui_platform::{ClipboardMime, LazyMimeProvider};

        let clipboards = Clipboards::new();
        let provider: LazyMimeProvider = Arc::new(|_| Some(b"k".to_vec()));
        let _fut = clipboards.write_mime_with_providers(
            ClipboardKind::Standard,
            vec![ClipboardMime::new("text/uri-list", b"file:///tmp/a\r\n")],
            vec![("application/vnd.portal.filetransfer".to_owned(), provider)],
            Some(3),
        );
        let req = clipboards
            .inner
            .borrow_mut()
            .queue
            .pop_front()
            .expect("queued");
        match req {
            super::ClipboardRequest::WriteMimeProviders {
                kind,
                entries,
                lazy,
                expected_revision,
                ..
            } => {
                assert_eq!(kind, ClipboardKind::Standard);
                assert_eq!(entries.len(), 1);
                assert_eq!(lazy.len(), 1);
                assert_eq!(lazy[0].0, "application/vnd.portal.filetransfer");
                assert_eq!(expected_revision, Some(3));
            }
            _ => panic!("expected WriteMimeProviders request"),
        }
    }
}
