//! Telling elements that something dragged from outside the window is over them, and reporting
//! their negotiation decision back to the platform.
//!
//! The platform reports a drag entering the surface, moving over it, changing negotiated action,
//! leaving it, and dropping on it. The drop is dispatched like any other input. The motion before
//! it becomes three events and one state, so an element can show that it will take the drop and
//! negotiate its MIME type and actions with the compositor:
//!
//! * `drag_enter` and `drag_leave`, one dispatch per element that the drag moved onto or off, as
//!   `pointer_enter` and `pointer_leave` are. They do not bubble.
//! * `drag_over` on the innermost element under the drag, on every move and action change. It
//!   bubbles.
//! * the drag-over state on every element under the drag: [`UiState::DRAG_OVER`] for code, and
//!   the custom state `drag-over` for style sheets, matched by `:state(drag-over)`. Servo's
//!   selector parser has no pseudo-class for the bit, so the state is the selector's way in.
//!
//! After `drag_enter` / `drag_over` handlers run, the innermost [`DragDecision`] recorded via
//! `EventCx::accept_drag` (or [`DragDecision::reject`] when no element claims the offer) is sent
//! to [`Surface::respond_drag_offer`](zgui_platform::Surface::respond_drag_offer).

use std::path::PathBuf;

use zgui_dom::NodeKey;
use zgui_geom::{Css, CssPx, Point};
use zgui_interned::Ident;
use zgui_platform::{DragEvent, DragOfferId};
use zgui_vocab::{
    DragAction, DragActions, DragDecision, DropEvent, EventKind, Payload, Timestamp, UiState,
};

use crate::window::Window;

/// The custom state an element carries while a drag is over it.
pub(crate) const DRAG_OVER_STATE: &str = "drag-over";

/// The elements a drag is over, root first, its advertised metadata, and where it was last.
#[derive(Debug, Default)]
pub(crate) struct DragOver {
    offer: Option<DragOfferId>,
    path: Vec<NodeKey>,
    paths: Vec<PathBuf>,
    mime_types: Vec<String>,
    source_actions: DragActions,
    action: Option<DragAction>,
    at: Point<CssPx, Css>,
    last_decision: Option<DragDecision>,
}

impl DragOver {
    fn clear(&mut self) {
        self.offer = None;
        self.paths.clear();
        self.mime_types.clear();
        self.source_actions = DragActions::NONE;
        self.action = None;
        self.last_decision = None;
    }

    fn payload(&self, position: Point<CssPx, Css>) -> Payload {
        Payload::Drop(DropEvent {
            paths: self.paths.clone(),
            mime_types: self.mime_types.clone(),
            payload: None,
            source_actions: self.source_actions,
            action: self.action,
            position,
        })
    }

    fn preferred_file_mime(&self) -> Option<&'static str> {
        if !self.paths.is_empty()
            || self
                .mime_types
                .iter()
                .any(|mime| mime.eq_ignore_ascii_case("text/uri-list"))
        {
            Some("text/uri-list")
        } else if self
            .mime_types
            .iter()
            .any(|mime| mime.eq_ignore_ascii_case("application/vnd.portal.filetransfer"))
        {
            Some("application/vnd.portal.filetransfer")
        } else {
            None
        }
    }
}

impl Window {
    /// Follows one drag event from the platform.
    pub(crate) fn drag_moved(&mut self, event: &DragEvent, timestamp: Timestamp) {
        match event {
            DragEvent::Entered {
                offer,
                mime_types,
                source_actions,
                action,
                paths,
                position,
            } => {
                self.drag.offer = Some(*offer);
                self.drag.mime_types.clone_from(mime_types);
                self.drag.source_actions = *source_actions;
                self.drag.action = *action;
                self.drag.paths.clone_from(paths);
                self.drag.last_decision = None;
                self.drag_to(Some(*position), true, timestamp);
            }
            DragEvent::Moved { offer, position } if self.drag.offer == Some(*offer) => {
                self.drag_to(Some(*position), true, timestamp);
            }
            DragEvent::ActionChanged { offer, action }
                if self.drag.offer == Some(*offer) && self.drag.action != *action =>
            {
                self.drag.action = *action;
                let at = self.drag.at;
                self.drag_to(Some(at), true, timestamp);
            }
            DragEvent::Left { .. } => {
                self.drag_to(None, true, timestamp);
                self.drag.clear();
            }
            DragEvent::Dropped { .. } => {
                self.drag_to(None, false, timestamp);
                self.drag.clear();
            }
            // A kind of drag this window does not know yet changes nothing it shows.
            _ => {}
        }
    }

    /// Moves the drag onto whatever is under `position`, or off everything.
    fn drag_to(
        &mut self,
        position: Option<Point<CssPx, Css>>,
        announce: bool,
        timestamp: Timestamp,
    ) {
        let (moved, target) = {
            let document = self.document.borrow();
            let layout = self.layout.borrow();
            let filter = self.engine.filter();
            let chain = position.map(|position| {
                let world = zgui_input::World {
                    document: &document,
                    layout: &layout,
                    hit: &self.hit,
                    clips: &self.scene.clips,
                    spatial: &self.scene.spatial,
                    scale: zgui_geom::Scale::new(self.scale),
                    filter: &filter,
                };
                world.chain_at(Point::new(
                    zgui_geom::DevicePx(position.x.0 * self.scale),
                    zgui_geom::DevicePx(position.y.0 * self.scale),
                ))
            });
            let to: Vec<NodeKey> = chain
                .as_ref()
                .map(|chain| chain.path().to_vec())
                .unwrap_or_default();
            let moved =
                zgui_input::move_bit(&document, &filter, UiState::DRAG_OVER, &self.drag.path, &to);
            if !moved.is_empty() {
                document
                    .edit(&filter, |edit| {
                        let changes = moved.left.iter().map(|key| (key, false));
                        for (key, on) in changes.chain(moved.entered.iter().map(|key| (key, true)))
                        {
                            if let Some(index) = edit.document().store().index_of(*key) {
                                edit.set_custom_state(index, Ident::new(DRAG_OVER_STATE), on);
                            }
                        }
                    })
                    .expect("the document is not poisoned");
            }
            self.drag.path = to;
            (moved, chain.and_then(|chain| chain.target()))
        };
        if !announce {
            return;
        }
        // Departures from the innermost outwards, then arrivals from the outermost inwards, as
        // pointer crossings are announced.
        let at = position.unwrap_or(self.drag.at);
        self.drag.at = at;
        let left: Vec<_> = moved
            .left
            .iter()
            .rev()
            .map(|n| (*n, self.drag.payload(at)))
            .collect();
        let entered: Vec<_> = moved
            .entered
            .iter()
            .map(|n| (*n, self.drag.payload(at)))
            .collect();
        let over = position.zip(target).map(|(p, t)| (t, self.drag.payload(p)));
        for (node, payload) in left {
            self.dispatch_synthetic(node, EventKind::DragLeave, payload, timestamp);
        }
        let mut enter_decision: Option<DragDecision> = None;
        let mut enter_prevented = false;
        for (node, payload) in entered {
            let result = self.dispatch_synthetic(node, EventKind::DragEnter, payload, timestamp);
            if let Some(decision) = result.drag_decision {
                enter_decision = Some(decision);
            }
            if !result.default_allowed {
                enter_prevented = true;
            }
        }
        let mut over_decision: Option<DragDecision> = None;
        let mut over_prevented = false;
        if let Some((node, payload)) = over {
            let result = self.dispatch_synthetic(node, EventKind::DragOver, payload, timestamp);
            over_decision = result.drag_decision;
            over_prevented = !result.default_allowed;
        }

        if position.is_some()
            && let Some(offer) = self.drag.offer
        {
            let decision = over_decision.or(enter_decision).unwrap_or_else(|| {
                if (over_prevented || enter_prevented)
                    && let Some(mime) = self.drag.preferred_file_mime()
                {
                    DragDecision::accept(mime, DragActions::COPY, Some(DragAction::Copy))
                } else {
                    DragDecision::reject()
                }
            });
            if self.drag.last_decision.as_ref() != Some(&decision) {
                self.drag.last_decision = Some(decision.clone());
                self.surface.respond_drag_offer(offer, decision);
            }
        }
    }
}
