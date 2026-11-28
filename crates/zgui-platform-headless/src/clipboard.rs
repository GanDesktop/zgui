//! A clipboard that is an ordinary value in memory.

use std::sync::{Arc, Mutex};

use zgui_platform::{
    Clipboard, ClipboardData, ClipboardError, ClipboardFormat, ClipboardKind, ClipboardMime,
    ClipboardMimeOffer, ClipboardSerial, ClipboardSerials, ClipboardWriteOptions, LazyMimeProvider,
    WakeReason, Waker,
};

/// One clipboard slot's stored data, MIME payloads, and revision counter.
#[derive(Default)]
struct Slot {
    data: Option<ClipboardData>,
    mimes: Vec<ClipboardMime>,
    lazy: Vec<(String, LazyMimeProvider)>,
    revision: u64,
}

impl core::fmt::Debug for Slot {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let lazy_mimes: Vec<&str> = self.lazy.iter().map(|(mime, _)| mime.as_str()).collect();
        formatter
            .debug_struct("Slot")
            .field("data", &self.data)
            .field("mimes", &self.mimes)
            .field("lazy_mimes", &lazy_mimes)
            .field("revision", &self.revision)
            .finish()
    }
}

/// A clipboard that is an ordinary value in memory.
///
/// The standard clipboard and the selection are separate slots. A backend that shared one would
/// pass every test written against copy and paste while destroying the user's selection on every
/// copy, which is a fault nothing but a person notices.
///
/// ```
/// use zgui_platform::{Clipboard, ClipboardData, ClipboardKind, ClipboardWriteOptions};
/// use zgui_platform_headless::MemoryClipboard;
///
/// let clipboard = MemoryClipboard::default();
/// clipboard
///     .write(
///         ClipboardKind::Standard,
///         ClipboardData::from("copied"),
///         ClipboardWriteOptions::default(),
///     )
///     .expect("an in-memory clipboard always accepts text");
///
/// assert!(clipboard.read_blocking(ClipboardKind::Primary, zgui_platform::ClipboardFormat::Text).is_err());
/// ```
#[derive(Default)]
pub struct MemoryClipboard {
    /// Identifiers handed to reads that are answered through the loop.
    serials: ClipboardSerials,
    /// What was last copied.
    standard: Mutex<Slot>,
    /// What was last selected.
    primary: Mutex<Slot>,
    /// How a started read delivers its answer.
    waker: Mutex<Option<Arc<dyn Waker>>>,
}

// By hand: a waker is not `Debug`, and the two slots are what a test wants to see anyway.
impl core::fmt::Debug for MemoryClipboard {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("MemoryClipboard")
            .field("standard", &self.standard)
            .field("primary", &self.primary)
            .finish_non_exhaustive()
    }
}

impl MemoryClipboard {
    /// Names how a read started through the loop delivers its answer.
    ///
    /// Without one a read takes an identity and is never answered, which is what a real backend
    /// does when it is asked before the loop can hear the reply.
    pub fn attach_waker(&self, waker: Arc<dyn Waker>) {
        *self.waker.lock().expect("the clipboard is not poisoned") = Some(waker);
    }

    /// Which slot a clipboard kind is held in.
    fn slot(&self, kind: ClipboardKind) -> &Mutex<Slot> {
        match kind {
            ClipboardKind::Primary => &self.primary,
            _ => &self.standard,
        }
    }

    fn wake_with(&self, reason: WakeReason) {
        let waker = self
            .waker
            .lock()
            .expect("the clipboard is not poisoned")
            .clone();
        // With no waker the answer is dropped, which is what a real backend does when a read is
        // started before the loop can hear the reply.
        if let Some(waker) = waker {
            waker.wake(reason);
        }
    }
}

fn data_to_mimes(data: &ClipboardData) -> Vec<ClipboardMime> {
    match data {
        ClipboardData::Text(text) => vec![ClipboardMime::new(
            "text/plain;charset=utf-8",
            text.as_bytes().to_vec(),
        )],
        ClipboardData::Html { markup, text } => {
            let mut entries = vec![ClipboardMime::new("text/html", markup.as_bytes().to_vec())];
            if let Some(text) = text {
                entries.push(ClipboardMime::new(
                    "text/plain;charset=utf-8",
                    text.as_bytes().to_vec(),
                ));
            }
            entries
        }
        ClipboardData::FileList(paths) => {
            let body = paths
                .iter()
                .map(|path| format!("file://{}\r\n", path.display()))
                .collect::<String>();
            vec![ClipboardMime::new("text/uri-list", body.into_bytes())]
        }
        ClipboardData::Image { media_type, bytes } => {
            vec![ClipboardMime::new(media_type.as_str(), bytes.clone())]
        }
        _ => Vec::new(),
    }
}

fn mimes_to_data(entries: &[ClipboardMime]) -> Option<ClipboardData> {
    if let Some(text) = entries
        .iter()
        .find(|entry| {
            matches!(
                entry.mime_type.as_str(),
                "text/plain;charset=utf-8" | "text/plain"
            )
        })
        .and_then(|entry| std::str::from_utf8(&entry.bytes).ok())
    {
        return Some(ClipboardData::from(text));
    }
    if let Some(image) = entries
        .iter()
        .find(|entry| entry.mime_type.starts_with("image/"))
    {
        return Some(ClipboardData::Image {
            media_type: image.mime_type.clone().into(),
            bytes: image.bytes.clone(),
        });
    }
    None
}

impl Clipboard for MemoryClipboard {
    fn read(&self, kind: ClipboardKind, format: ClipboardFormat) -> ClipboardSerial {
        let serial = self.serials.take();
        let result = self.read_blocking(kind, format);
        self.wake_with(WakeReason::ClipboardRead { serial, result });
        serial
    }

    fn read_blocking(
        &self,
        kind: ClipboardKind,
        format: ClipboardFormat,
    ) -> Result<ClipboardData, ClipboardError> {
        let held = self
            .slot(kind)
            .lock()
            .expect("the clipboard is not poisoned")
            .data
            .clone()
            .ok_or(ClipboardError::Empty(kind))?;
        if held.format() == format {
            Ok(held)
        } else {
            Err(ClipboardError::UnavailableFormat(format))
        }
    }

    fn write(
        &self,
        kind: ClipboardKind,
        data: ClipboardData,
        _options: ClipboardWriteOptions,
    ) -> Result<(), ClipboardError> {
        let mut slot = self
            .slot(kind)
            .lock()
            .expect("the clipboard is not poisoned");
        slot.revision = slot.revision.saturating_add(1);
        slot.mimes = data_to_mimes(&data);
        slot.lazy.clear();
        slot.data = Some(data);
        Ok(())
    }

    fn read_mime(
        &self,
        kind: ClipboardKind,
        mime_types: &[String],
        max_bytes: usize,
    ) -> ClipboardSerial {
        let serial = self.serials.take();
        let result = self.read_mime_blocking(kind, mime_types, max_bytes);
        self.wake_with(WakeReason::ClipboardMimeRead { serial, result });
        serial
    }

    fn read_mime_blocking(
        &self,
        kind: ClipboardKind,
        mime_types: &[String],
        max_bytes: usize,
    ) -> Result<ClipboardMimeOffer, ClipboardError> {
        enum Planned {
            Static(ClipboardMime),
            Lazy {
                wanted: String,
                offered: String,
                provider: LazyMimeProvider,
            },
        }

        let (revision, offered_mime_types, planned) = {
            let slot = self
                .slot(kind)
                .lock()
                .expect("the clipboard is not poisoned");
            if slot.mimes.is_empty() && slot.lazy.is_empty() && slot.data.is_none() {
                return Err(ClipboardError::Empty(kind));
            }
            let mut offered_mime_types = Vec::with_capacity(slot.mimes.len() + slot.lazy.len());
            for entry in &slot.mimes {
                if !offered_mime_types
                    .iter()
                    .any(|seen: &String| seen.eq_ignore_ascii_case(&entry.mime_type))
                {
                    offered_mime_types.push(entry.mime_type.clone());
                }
            }
            for (mime_type, _) in &slot.lazy {
                if !offered_mime_types
                    .iter()
                    .any(|seen: &String| seen.eq_ignore_ascii_case(mime_type))
                {
                    offered_mime_types.push(mime_type.clone());
                }
            }

            let mut planned = Vec::new();
            for wanted in mime_types {
                if let Some(found) = slot
                    .mimes
                    .iter()
                    .find(|entry| entry.mime_type == *wanted)
                    .or_else(|| {
                        slot.mimes
                            .iter()
                            .find(|entry| entry.mime_type.eq_ignore_ascii_case(wanted))
                    })
                {
                    planned.push(Planned::Static(ClipboardMime::new(
                        wanted.clone(),
                        found.bytes.clone(),
                    )));
                } else if let Some((offered, provider)) = slot
                    .lazy
                    .iter()
                    .find(|(mime_type, _)| mime_type == wanted)
                    .or_else(|| {
                        slot.lazy
                            .iter()
                            .find(|(mime_type, _)| mime_type.eq_ignore_ascii_case(wanted))
                    })
                {
                    planned.push(Planned::Lazy {
                        wanted: wanted.clone(),
                        offered: offered.clone(),
                        provider: Arc::clone(provider),
                    });
                }
            }
            (slot.revision, offered_mime_types, planned)
        };

        let mut total_bytes = 0_usize;
        let mut entries = Vec::with_capacity(planned.len());
        for item in planned {
            let entry = match item {
                Planned::Static(entry) => entry,
                Planned::Lazy {
                    wanted,
                    offered,
                    provider,
                } => {
                    let Some(bytes) = provider(&offered) else {
                        continue;
                    };
                    ClipboardMime::new(wanted, bytes)
                }
            };
            total_bytes = total_bytes.saturating_add(entry.bytes.len());
            if total_bytes > max_bytes {
                return Err(ClipboardError::Backend(format!(
                    "clipboard payload exceeds {max_bytes} bytes"
                )));
            }
            entries.push(entry);
        }
        Ok(ClipboardMimeOffer {
            revision,
            offered_mime_types,
            entries,
        })
    }

    fn write_mime(
        &self,
        kind: ClipboardKind,
        entries: &[ClipboardMime],
        expected_revision: Option<u64>,
    ) -> Result<(), ClipboardError> {
        self.write_mime_providers(kind, entries, &[], expected_revision)
    }

    fn write_mime_providers(
        &self,
        kind: ClipboardKind,
        entries: &[ClipboardMime],
        lazy: &[(String, LazyMimeProvider)],
        expected_revision: Option<u64>,
    ) -> Result<(), ClipboardError> {
        let mut slot = self
            .slot(kind)
            .lock()
            .expect("the clipboard is not poisoned");
        if expected_revision.is_some_and(|revision| revision != slot.revision) {
            return Err(ClipboardError::Changed(kind));
        }
        slot.revision = slot.revision.saturating_add(1);
        if entries.is_empty() && lazy.is_empty() {
            slot.data = None;
            slot.mimes.clear();
            slot.lazy.clear();
        } else {
            slot.data = mimes_to_data(entries);
            slot.mimes = entries.to_vec();
            slot.lazy = lazy.to_vec();
        }
        Ok(())
    }

    fn clear(&self, kind: ClipboardKind) -> Result<(), ClipboardError> {
        let mut slot = self
            .slot(kind)
            .lock()
            .expect("the clipboard is not poisoned");
        slot.revision = slot.revision.saturating_add(1);
        slot.data = None;
        slot.mimes.clear();
        slot.lazy.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryClipboard;
    use zgui_platform::{
        Clipboard, ClipboardData, ClipboardFormat, ClipboardKind, ClipboardWriteOptions,
    };

    #[test]
    fn copying_does_not_overwrite_the_selection() {
        let clipboard = MemoryClipboard::default();
        clipboard
            .write(
                ClipboardKind::Primary,
                ClipboardData::from("selected"),
                ClipboardWriteOptions::default(),
            )
            .expect("accepted");
        clipboard
            .write(
                ClipboardKind::Standard,
                ClipboardData::from("copied"),
                ClipboardWriteOptions::default(),
            )
            .expect("accepted");

        let selected = clipboard
            .read_blocking(ClipboardKind::Primary, ClipboardFormat::Text)
            .expect("still there");
        assert_eq!(selected.as_text(), Some("selected"));
    }

    #[test]
    fn a_read_started_through_the_loop_is_answered_by_a_wake_naming_it() {
        use std::sync::Arc;
        use zgui_platform::{WakeReason, Waker};

        let waker = Arc::new(crate::waker::RecordingWaker::default());
        let clipboard = MemoryClipboard::default();
        clipboard.attach_waker(Arc::clone(&waker) as Arc<dyn Waker>);
        clipboard
            .write(
                ClipboardKind::Standard,
                ClipboardData::from("held"),
                ClipboardWriteOptions::default(),
            )
            .expect("accepted");

        let serial = clipboard.read(ClipboardKind::Standard, ClipboardFormat::Text);
        let woken = waker.drain();
        assert_eq!(woken.len(), 1, "one read, one answer");
        match &woken[0] {
            WakeReason::ClipboardRead {
                serial: named,
                result,
            } => {
                assert_eq!(*named, serial, "the answer names the read that started it");
                assert_eq!(
                    result.as_ref().ok().and_then(|data| data.as_text()),
                    Some("held")
                );
            }
            other => panic!("a read is answered by a clipboard wake, not {other:?}"),
        }
    }

    #[test]
    fn a_read_of_an_empty_clipboard_is_answered_with_the_reason_it_failed() {
        use std::sync::Arc;
        use zgui_platform::{WakeReason, Waker};

        let waker = Arc::new(crate::waker::RecordingWaker::default());
        let clipboard = MemoryClipboard::default();
        clipboard.attach_waker(Arc::clone(&waker) as Arc<dyn Waker>);

        clipboard.read(ClipboardKind::Standard, ClipboardFormat::Text);
        let woken = waker.drain();
        assert_eq!(woken.len(), 1);
        match &woken[0] {
            WakeReason::ClipboardRead { result, .. } => assert!(
                matches!(result, Err(zgui_platform::ClipboardError::Empty(_))),
                "an empty clipboard answers, and says it was empty"
            ),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_read_with_no_loop_to_answer_it_takes_an_identity_and_nothing_else() {
        let clipboard = MemoryClipboard::default();
        // No waker attached: the read is started and its answer is dropped.
        let first = clipboard.read(ClipboardKind::Standard, ClipboardFormat::Text);
        let second = clipboard.read(ClipboardKind::Standard, ClipboardFormat::Text);
        assert_ne!(first, second);
    }

    #[test]
    fn every_read_started_through_the_loop_gets_its_own_identity() {
        let clipboard = MemoryClipboard::default();
        let first = clipboard.read(ClipboardKind::Standard, ClipboardFormat::Text);
        let second = clipboard.read(ClipboardKind::Standard, ClipboardFormat::Text);
        assert_ne!(first, second);
    }

    #[test]
    fn multi_mime_read_write_and_revision_cas() {
        let clipboard = MemoryClipboard::default();
        clipboard
            .write_mime(
                ClipboardKind::Standard,
                &[
                    zgui_platform::ClipboardMime::new("image/png", [0x89_u8, b'P', b'N', b'G']),
                    zgui_platform::ClipboardMime::new("text/plain;charset=utf-8", b"img".to_vec()),
                ],
                Some(0),
            )
            .expect("initial cas write succeeds");

        let offer = clipboard
            .read_mime_blocking(
                ClipboardKind::Standard,
                &[
                    "text/plain;charset=utf-8".to_owned(),
                    "image/png".to_owned(),
                    "text/uri-list".to_owned(),
                ],
                64,
            )
            .expect("read mime succeeds");
        assert_eq!(offer.revision, 1);
        assert_eq!(offer.entries.len(), 2);
        assert_eq!(offer.entries[0].mime_type, "text/plain;charset=utf-8");
        assert_eq!(offer.entries[1].mime_type, "image/png");

        let stale = clipboard.write_mime(
            ClipboardKind::Standard,
            &[zgui_platform::ClipboardMime::new("text/plain", b"new")],
            Some(0),
        );
        assert_eq!(
            stale,
            Err(zgui_platform::ClipboardError::Changed(
                ClipboardKind::Standard
            ))
        );

        clipboard
            .write_mime(ClipboardKind::Standard, &[], Some(offer.revision))
            .expect("empty slice clears");
        assert_eq!(
            clipboard.read_mime_blocking(ClipboardKind::Standard, &["text/plain".to_owned()], 64,),
            Err(zgui_platform::ClipboardError::Empty(
                ClipboardKind::Standard
            ))
        );
    }

    #[test]
    fn lazy_mime_provider_is_advertised_without_being_invoked_until_requested() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use zgui_platform::LazyMimeProvider;

        let clipboard = MemoryClipboard::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_calls = Arc::clone(&calls);
        let provider: LazyMimeProvider = Arc::new(move |_| {
            provider_calls.fetch_add(1, Ordering::SeqCst);
            Some(b"portal-key-42".to_vec())
        });
        let declining: LazyMimeProvider = Arc::new(|_| None);

        clipboard
            .write_mime_providers(
                ClipboardKind::Standard,
                &[zgui_platform::ClipboardMime::new(
                    "text/uri-list",
                    b"file:///tmp/a.txt\r\n",
                )],
                &[
                    ("application/vnd.portal.filetransfer".to_owned(), provider),
                    ("application/x-declined".to_owned(), declining),
                ],
                None,
            )
            .expect("write with lazy providers succeeds");

        // Probing offered MIME types or reading static MIMEs must never invoke lazy providers.
        let probe = clipboard
            .read_mime_blocking(ClipboardKind::Standard, &["text/uri-list".to_owned()], 1024)
            .expect("probe succeeds");
        assert!(probe.offers_mime("application/vnd.portal.filetransfer"));
        assert!(probe.offers_mime("text/uri-list"));
        assert_eq!(probe.entries.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        // Explicitly requesting the lazy MIME invokes the provider once.
        let fetched = clipboard
            .read_mime_blocking(
                ClipboardKind::Standard,
                &[
                    "application/vnd.portal.filetransfer".to_owned(),
                    "application/x-declined".to_owned(),
                ],
                1024,
            )
            .expect("lazy read succeeds");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(fetched.entries.len(), 1);
        assert_eq!(
            fetched.entries[0].mime_type,
            "application/vnd.portal.filetransfer"
        );
        assert_eq!(fetched.entries[0].bytes, b"portal-key-42");
    }
}
