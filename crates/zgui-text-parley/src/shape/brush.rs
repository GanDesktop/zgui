//! The brush a shaped run carries, in the shape the engine requires.

use zgui_scene::PaintSlot;
use zgui_text::Brush;

/// A paint slot and the requested CSS weight, wrapped so they travel through a shaped layout.
///
/// The engine requires a brush to have a default value; a paint slot deliberately does not, because
/// slot zero is an ordinary entry in the table and "no brush" is not a thing a run can be. The
/// wrapper supplies the default the engine needs without letting that default leak into the paint
/// table's own vocabulary, and carries the run's requested CSS weight so synthetic emboldening can
/// compare what was asked for against the face that matched.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotBrush {
    /// The paint slot the run draws with.
    pub slot: Brush,
    /// The CSS font weight the run asked for.
    pub weight: f32,
}

impl Default for SlotBrush {
    fn default() -> Self {
        Self {
            slot: PaintSlot(0),
            weight: 400.0,
        }
    }
}

impl From<Brush> for SlotBrush {
    fn from(slot: Brush) -> Self {
        Self {
            slot,
            weight: 400.0,
        }
    }
}
