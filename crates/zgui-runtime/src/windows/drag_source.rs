//! What hears about lifecycle updates for native drags started from a window.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use zgui_platform::DragSourceEvent;

/// One callback told about every outgoing native drag update of a window.
type Callback = Rc<RefCell<dyn FnMut(DragSourceEvent)>>;

/// The callbacks one window tells about its outgoing native drag sessions.
#[derive(Default)]
pub(crate) struct DragSourceListeners {
    /// The callbacks, in the order they were registered, each with its number.
    callbacks: Vec<(u64, Callback)>,
    /// The number the next callback gets.
    next: u64,
}

impl DragSourceListeners {
    /// Registers `callback`, and answers with the number that removes it.
    fn insert(&mut self, callback: Callback) -> u64 {
        let id = self.next;
        self.next += 1;
        self.callbacks.push((id, callback));
        id
    }

    /// Removes the callback numbered `id`.
    fn remove(&mut self, id: u64) {
        self.callbacks.retain(|(held, _)| *held != id);
    }
}

/// Registers `callback` in `listeners`, and answers with the guard that keeps it registered.
pub(crate) fn listen(
    listeners: &Rc<RefCell<DragSourceListeners>>,
    callback: impl FnMut(DragSourceEvent) + 'static,
) -> DragSourceGuard {
    let id = listeners
        .borrow_mut()
        .insert(Rc::new(RefCell::new(callback)));
    DragSourceGuard {
        listeners: Rc::downgrade(listeners),
        id,
    }
}

/// Tells every callback in `listeners` about `event`.
///
/// The callbacks are copied out first, so a callback may register or remove another one. A
/// callback runs as a listener does: a signal it reads is read, not subscribed to.
pub(crate) fn tell(listeners: &Rc<RefCell<DragSourceListeners>>, event: &DragSourceEvent) {
    let callbacks: Vec<Callback> = listeners
        .borrow()
        .callbacks
        .iter()
        .map(|(_, callback)| Rc::clone(callback))
        .collect();
    let _zone = zgui_reactive::enter_non_reactive_zone();
    for callback in callbacks {
        if let Ok(mut callback) = callback.try_borrow_mut() {
            callback(event.clone());
        }
    }
}

/// What keeps a [`WindowHandle::on_drag_source`](crate::windows::WindowHandle::on_drag_source)
/// callback registered.
///
/// Dropping it removes the callback.
#[must_use = "dropping the guard removes the callback"]
pub struct DragSourceGuard {
    /// Where the callback is registered, while the window is alive.
    listeners: Weak<RefCell<DragSourceListeners>>,
    /// Which callback it is.
    id: u64,
}

impl Drop for DragSourceGuard {
    fn drop(&mut self) {
        if let Some(listeners) = self.listeners.upgrade() {
            listeners.borrow_mut().remove(self.id);
        }
    }
}

impl core::fmt::Debug for DragSourceGuard {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("DragSourceGuard")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::{DragSourceListeners, listen, tell};
    use std::cell::RefCell;
    use std::rc::Rc;
    use zgui_platform::{DragAction, DragSessionId, DragSourceEvent};

    #[test]
    fn every_callback_hears_drag_source_events_until_its_guard_goes() {
        let listeners = Rc::new(RefCell::new(DragSourceListeners::default()));
        let heard = Rc::new(RefCell::new(Vec::new()));
        let first = {
            let heard = Rc::clone(&heard);
            listen(&listeners, move |event| {
                heard.borrow_mut().push(("a", event))
            })
        };
        let second = {
            let heard = Rc::clone(&heard);
            listen(&listeners, move |event| {
                heard.borrow_mut().push(("b", event))
            })
        };
        let session = DragSessionId::new(1);
        let dropped = DragSourceEvent::Dropped {
            session,
            action: Some(DragAction::Copy),
        };
        let finished = DragSourceEvent::Finished {
            session,
            action: Some(DragAction::Copy),
        };
        tell(&listeners, &dropped);
        drop(first);
        tell(&listeners, &finished);
        assert_eq!(
            *heard.borrow(),
            [("a", dropped.clone()), ("b", dropped), ("b", finished)]
        );
        drop(second);
        assert!(listeners.borrow().callbacks.is_empty());
    }
}
