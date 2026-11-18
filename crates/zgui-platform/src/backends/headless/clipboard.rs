//! A clipboard that is an ordinary value in memory.

use std::sync::{Arc, Mutex};

use crate::app::WakeReason;
use crate::clipboard::{
    Clipboard, ClipboardData, ClipboardError, ClipboardFormat, ClipboardKind, ClipboardMime,
    ClipboardMimeOffer, ClipboardSerial, ClipboardSerials, ClipboardWriteOptions,
};
use crate::waker::Waker;

#[derive(Default)]
struct Slot {
    data: Option<ClipboardData>,
    mimes: Vec<ClipboardMime>,
    revision: u64,
}

/// A clipboard that is an ordinary value in memory.
///
/// The two clipboards are separate slots, because a backend that shared one would pass every test
/// written against copy and paste while destroying the user's selection on every copy.
#[derive(Default)]
pub(super) struct MemoryClipboard {
    serials: ClipboardSerials,
    standard: Mutex<Slot>,
    primary: Mutex<Slot>,
    /// How a started read delivers its answer.
    waker: Mutex<Option<Arc<dyn Waker>>>,
}

impl MemoryClipboard {
    /// Names how a read started through the loop delivers its answer.
    pub(super) fn attach_waker(&self, waker: Arc<dyn Waker>) {
        *self.waker.lock().expect("the clipboard is not poisoned") = Some(waker);
    }

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
