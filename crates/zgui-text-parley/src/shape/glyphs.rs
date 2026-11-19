//! Reading positioned glyphs out of a broken layout.
//!
//! # Why the positions are shifted
//!
//! The engine positions every glyph relative to the *paragraph's* origin, and folds the line's
//! alignment offset and its inline minimum coordinate into that position. A line box, on the other
//! hand, starts exactly where those two put it. Subtracting them therefore moves the glyphs into
//! the line box's own space, which is the space everything downstream places a line in — and it is
//! the only conversion here, because the engine is already working in device pixels.

use zgui_text::{Brush, FontSource, SYNTHETIC_BOLD_RATIO, ShapedGlyph, ShapedRun};

use crate::shape::brush::SlotBrush;
use crate::shape::engine::{Form, ShapedLayout};
use crate::system::FontSystem;

/// Visits the style-uniform runs of one line, positioned against the line box's top-left corner.
pub(crate) fn visit_line(
    fonts: &FontSystem,
    shaped: &ShapedLayout,
    line: u16,
    visit: &mut dyn FnMut(ShapedRun<'_>),
) {
    let layout = match &shaped.form {
        Form::Live(live) => &live.layout,
        Form::Frozen(frozen) => return frozen.visit_line(fonts, line, visit),
    };
    let Some(line) = layout.get(line as usize) else {
        return;
    };
    let metrics = *line.metrics();
    let left = metrics.offset + metrics.inline_min_coord;
    let top = crate::shape::lines::top(&metrics);

    // One buffer for the whole line rather than one per run: a line of prose is a handful of runs
    // and every one of them would otherwise allocate.
    let mut glyphs: Vec<ShapedGlyph> = Vec::new();
    for item in line.items() {
        let parley::PositionedLayoutItem::GlyphRun(run) = item else {
            continue;
        };
        glyphs.clear();
        glyphs.extend(run.positioned_glyphs().map(|glyph| ShapedGlyph {
            glyph: glyph.id as u16,
            x: glyph.x - left,
            y: glyph.y - top,
        }));
        if glyphs.is_empty() {
            continue;
        }
        visit(described(fonts, &run, &glyphs, shaped.prefix));
    }
}

/// How much a run's stems are thickened to stand in for a weight the matched face lacks, as a
/// fraction of its font size.
///
/// Follows the standard browser and toolkit threshold (Blink, Gecko, Pango):
/// - Variable faces instance the weight axis directly (`embolden == false`) and never synthesise.
/// - Requests below `600` (such as CSS `500` Medium falling back to `400` Regular) stay clean
///   rather than receiving synthetic bold.
/// - Requests at or above `600` that already matched a semi-bold or bold face (`>= 600`) need no
///   synthesis.
/// - Requests at or above `600` that fell back below `600` scale [`SYNTHETIC_BOLD_RATIO`]
///   proportionally to the weight deficit.
pub(crate) fn synthetic_bold_ratio(
    embolden: bool,
    requested_weight: f32,
    matched_weight: f32,
) -> f32 {
    if !(embolden
        && requested_weight >= 600.0
        && matched_weight < 600.0
        && requested_weight > matched_weight)
    {
        return 0.0;
    }
    ((requested_weight - matched_weight) / 300.0).clamp(0.0, 1.5) * SYNTHETIC_BOLD_RATIO
}

/// One engine run, described the way a rasteriser needs it.
///
/// `prefix` is how many bytes of directional control lead the engine's string; the run's text
/// offset is reported in the caller's string, which does not hold them.
fn described<'a>(
    fonts: &FontSystem,
    run: &parley::GlyphRun<'_, SlotBrush>,
    glyphs: &'a [ShapedGlyph],
    prefix: usize,
) -> ShapedRun<'a> {
    let text = run.run();
    let text_start = text.text_range().start.saturating_sub(prefix);
    let synthesis = text.synthesis();
    let brush = run.style().brush;
    run_of(
        fonts,
        text.font(),
        text.normalized_coords(),
        text.font_size(),
        synthesis.embolden(),
        brush.weight,
        synthesis.skew().unwrap_or(0.0),
        brush.slot,
        glyphs,
        u32::try_from(text_start).unwrap_or(u32::MAX),
    )
}

/// One run of glyphs in one face, described the way a rasteriser needs it.
///
/// `embolden` and `skew` are the engine's synthesis. `text_start` is in the caller's string.
#[expect(
    clippy::too_many_arguments,
    reason = "the fields of one engine run, read once"
)]
pub(crate) fn run_of<'a>(
    fonts: &FontSystem,
    font: &parley::FontData,
    coords: &[i16],
    size: f32,
    embolden: bool,
    requested_weight: f32,
    skew: f32,
    brush: Brush,
    glyphs: &'a [ShapedGlyph],
    text_start: u32,
) -> ShapedRun<'a> {
    let face = fonts.face_for(font);
    let record = fonts.face(face);
    let matched_weight = record.as_ref().map_or(400.0, |record| record.weight);
    ShapedRun {
        face,
        size,
        instance: fonts.variation_instance(coords),
        synthetic_bold: synthetic_bold_ratio(embolden, requested_weight, matched_weight),
        synthetic_slant: skew,
        has_color: record.is_some_and(|record| record.has_color),
        brush,
        glyphs,
        text_start,
    }
}

#[cfg(test)]
mod tests {
    use zgui_text::SYNTHETIC_BOLD_RATIO;

    use super::synthetic_bold_ratio;

    #[test]
    fn synthetic_bold_requires_bold_threshold_and_scales_with_deficit() {
        // Variable faces or exact matches never embolden.
        assert_eq!(synthetic_bold_ratio(false, 700.0, 400.0), 0.0);
        assert_eq!(synthetic_bold_ratio(true, 400.0, 400.0), 0.0);

        // Medium (500) falling back to Regular (400) stays clean 400.
        assert_eq!(synthetic_bold_ratio(true, 500.0, 400.0), 0.0);
        assert_eq!(synthetic_bold_ratio(true, 599.0, 400.0), 0.0);

        // Semi-bold (600) and Bold (700) on a 400 face scale proportionally.
        let at_600 = synthetic_bold_ratio(true, 600.0, 400.0);
        let expected_600 = (200.0 / 300.0) * SYNTHETIC_BOLD_RATIO;
        assert!((at_600 - expected_600).abs() <= 1e-7);

        let at_700 = synthetic_bold_ratio(true, 700.0, 400.0);
        assert!((at_700 - SYNTHETIC_BOLD_RATIO).abs() <= 1e-7);

        // A family that already matched >= 600 does not synthesise further.
        assert_eq!(synthetic_bold_ratio(true, 700.0, 600.0), 0.0);
        assert_eq!(synthetic_bold_ratio(true, 900.0, 700.0), 0.0);

        // Heavy weights clamp at 1.5x the standard bold ratio.
        let at_900 = synthetic_bold_ratio(true, 900.0, 400.0);
        assert!((at_900 - 1.5 * SYNTHETIC_BOLD_RATIO).abs() <= 1e-7);
    }
}
