//! A clipboard that is an ordinary value in memory.

use std::sync::{Arc, Mutex};

use zgui_platform::{
    Clipboard, ClipboardData, ClipboardError, ClipboardFormat, ClipboardKind, ClipboardMime,
    ClipboardMimeOffer, ClipboardSerial, ClipboardSerials, ClipboardWriteOptions, WakeReason,
    Waker,
};

/// One clipboard slot's stored data, MIME payloads, and revision counter.
#[derive(Debug, Default)]
struct Slot {
    data: Option<ClipboardData>,
    mimes: Vec<ClipboardMime>,
    revision: u64,
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
        let slot = self
            .slot(kind)
            .lock()
            .expect("the clipboard is not poisoned");
        if slot.mimes.is_empty() && slot.data.is_none() {
            return Err(ClipboardError::Empty(kind));
        }
        let mut total_bytes = 0_usize;
        let mut entries = Vec::new();
        for mime in mime_types {
            if let Some(found) = slot.mimes.iter().find(|entry| entry.mime_type == *mime) {
                total_bytes = total_bytes.saturating_add(found.bytes.len());
                if total_bytes > max_bytes {
                    return Err(ClipboardError::Backend(format!(
                        "clipboard payload exceeds {max_bytes} bytes"
                    )));
                }
                entries.push(found.clone());
            }
        }
        Ok(ClipboardMimeOffer {
            revision: slot.revision,
            entries,
        })
    }

    fn write_mime(
        &self,
        kind: ClipboardKind,
        entries: &[ClipboardMime],
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
        if entries.is_empty() {
            slot.data = None;
            slot.mimes.clear();
        } else {
            slot.data = mimes_to_data(entries);
            slot.mimes = entries.to_vec();
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
}
