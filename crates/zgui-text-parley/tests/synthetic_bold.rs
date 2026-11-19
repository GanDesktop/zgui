//! Standard synthetic bold threshold policy and winding-independent bitmap dilation.

mod support;

use std::sync::Arc;

use zgui_geom::CssPx;
use zgui_interned::Ident;
use zgui_scene::PaintSlot;
use zgui_text::{
    BreakRequest, FaceQuery, FontSource, GlyphFormat, GlyphImage, GlyphKey, GlyphRaster,
    ParagraphShaper, RasterStyle, SYNTHETIC_BOLD_RATIO, ShapedRunOwned, SubpixelOffset,
};
use zgui_text_parley::{Controls, FontSystem, FontSystemOptions, LineRequest, Rasteriser, Shaper};
use zgui_text_style::{Direction, FamilyName, FontFamilyList, TextStyle, WrapMode};

fn sum_ink(image: &GlyphImage) -> u64 {
    image.bytes.iter().map(|&byte| u64::from(byte)).sum()
}

fn shape_paragraph_runs(
    fonts: &Arc<FontSystem>,
    family: &str,
    weight: f32,
    freeze: bool,
) -> Vec<ShapedRunOwned> {
    let mut shaper = Shaper::with_controls(fonts.clone(), Controls::Verbatim).freezing(freeze);
    let mut fixture = support::Fixture::new("Button 500", Direction::LeftToRight);
    fixture.runs[0].style = Arc::new(TextStyle {
        family: FontFamilyList::from_iter([FamilyName::Named(Ident::new(family))]),
        size: CssPx(20.0),
        weight,
        wrap_mode: if freeze {
            WrapMode::NoWrap
        } else {
            WrapMode::Wrap
        },
        ..TextStyle::initial()
    });
    let content = fixture.content();
    let mut shaped = shaper.shape(&content);
    if freeze {
        assert!(shaped.engine.is_frozen(), "expected frozen paragraph");
    } else {
        assert!(!shaped.engine.is_frozen(), "expected live paragraph");
    }
    shaper.break_lines(&mut shaped, &BreakRequest::new(&content, None));
    let mut runs = Vec::new();
    shaper.visit_line(&shaped, 0, &mut |run| runs.push(ShapedRunOwned::from(run)));
    runs
}

#[test]
fn weight_500_on_a_regular_400_face_stays_clean_while_600_and_700_synthesise_proportionally() {
    let fonts = support::fonts();
    let family = Ident::new(support::LATIN);

    for freeze in [false, true] {
        let w400 = &shape_paragraph_runs(&fonts, support::LATIN, 400.0, freeze)[0];
        let w500 = &shape_paragraph_runs(&fonts, support::LATIN, 500.0, freeze)[0];
        let w600 = &shape_paragraph_runs(&fonts, support::LATIN, 600.0, freeze)[0];
        let w700 = &shape_paragraph_runs(&fonts, support::LATIN, 700.0, freeze)[0];

        assert_eq!(w400.synthetic_bold, 0.0, "freeze={freeze}");
        assert_eq!(
            w500.synthetic_bold, 0.0,
            "CSS 500 falling back to 400 Regular must not apply synthetic bold (freeze={freeze})"
        );
        let expected_600 = (200.0 / 300.0) * SYNTHETIC_BOLD_RATIO;
        assert!(
            (w600.synthetic_bold - expected_600).abs() <= 1e-7,
            "600 on a 400 face scales proportionally: got {}, expected {expected_600}",
            w600.synthetic_bold
        );
        assert!(
            (w700.synthetic_bold - SYNTHETIC_BOLD_RATIO).abs() <= 1e-7,
            "700 on a 400 face applies full SYNTHETIC_BOLD_RATIO: got {}",
            w700.synthetic_bold
        );
    }

    // Single-line cell shaping (`LineRequest`) obeys the exact same policy.
    let mut shaper = Shaper::with_controls(fonts, Controls::Verbatim);
    let families = [family];
    let line_at = |shaper: &mut Shaper, weight: u16| {
        shaper.shape_line(
            "Button 500",
            &LineRequest {
                families: &families,
                weight,
                italic: false,
                size_device_px: 20.0,
                letter_spacing: 0.0,
                ligatures: true,
            },
        )[0]
        .synthetic_bold
    };
    assert_eq!(line_at(&mut shaper, 400), 0.0);
    assert_eq!(line_at(&mut shaper, 500), 0.0);
    assert!((line_at(&mut shaper, 600) - (200.0 / 300.0) * SYNTHETIC_BOLD_RATIO).abs() <= 1e-7);
    assert!((line_at(&mut shaper, 700) - SYNTHETIC_BOLD_RATIO).abs() <= 1e-7);
}

#[test]
fn variable_faces_never_synthesise_bold_at_any_weight() {
    let fonts = Arc::new(FontSystem::new(FontSystemOptions::registered_only()));
    fonts
        .register(support::face("RubikSubset-Variable.ttf"), None)
        .expect("variable face registers");

    for weight in [400.0, 500.0, 600.0, 700.0, 900.0] {
        for freeze in [false, true] {
            let mut shaper =
                Shaper::with_controls(fonts.clone(), Controls::Verbatim).freezing(freeze);
            let mut fixture = support::Fixture::new("HOW", Direction::LeftToRight);
            fixture.runs[0].style = Arc::new(TextStyle {
                family: FontFamilyList::from_iter([FamilyName::Named(Ident::new("Rubik"))]),
                size: CssPx(24.0),
                weight,
                wrap_mode: if freeze {
                    WrapMode::NoWrap
                } else {
                    WrapMode::Wrap
                },
                ..TextStyle::initial()
            });
            let content = fixture.content();
            let mut shaped = shaper.shape(&content);
            shaper.break_lines(&mut shaped, &BreakRequest::new(&content, None));
            shaper.visit_line(&shaped, 0, &mut |run| {
                assert_eq!(
                    run.synthetic_bold, 0.0,
                    "variable face synthesised bold at weight {weight} (freeze={freeze})"
                );
            });
        }
    }
}

#[test]
fn synthetic_bold_rasterisation_dilates_coverage_monotonically_and_widens_bounds() {
    let fonts = support::fonts();
    let raster = Rasteriser::new(fonts.clone());

    let regular_runs = shape_paragraph_runs(&fonts, support::LATIN, 400.0, false);
    let semibold_runs = shape_paragraph_runs(&fonts, support::LATIN, 600.0, false);
    let bold_runs = shape_paragraph_runs(&fonts, support::LATIN, 700.0, false);

    let regular_run = regular_runs[0].as_run(PaintSlot(0));
    let semibold_run = semibold_runs[0].as_run(PaintSlot(0));
    let bold_run = bold_runs[0].as_run(PaintSlot(0));

    for style in [RasterStyle::Grayscale, RasterStyle::Subpixel] {
        let channels: u32 = match style {
            RasterStyle::Grayscale => 1,
            RasterStyle::Subpixel => 3,
            RasterStyle::Color => unreachable!(),
        };
        for &glyph in regular_run.glyphs {
            let reg_key = regular_run.key_at(glyph.glyph, SubpixelOffset(0), style);
            let semi_key = semibold_run.key_at(glyph.glyph, SubpixelOffset(0), style);
            let bold_key = bold_run.key_at(glyph.glyph, SubpixelOffset(0), style);

            let Some(reg_img) = raster.raster(&reg_key) else {
                continue;
            };
            if reg_img.is_empty() {
                continue;
            }
            let semi_img = raster.raster(&semi_key).expect("semibold rasterises");
            let bold_img = raster.raster(&bold_key).expect("bold rasterises");

            let bold_px = f32::from_bits(bold_key.synthetic_bold_bits);
            let pad_x = bold_px.clamp(0.0, 8.0).ceil() as u32;
            let pad_y = (bold_px * 0.5).clamp(0.0, 4.0).ceil() as u32;

            assert_eq!(bold_img.size.width, reg_img.size.width + pad_x);
            assert_eq!(bold_img.size.height, reg_img.size.height + pad_y);
            assert_eq!(bold_img.placement.x, reg_img.placement.x);
            assert_eq!(bold_img.placement.y.0, reg_img.placement.y.0 + pad_y as f32);

            // Every single existing pixel on screen must satisfy `out_alpha >= in_alpha`.
            let in_stride = (reg_img.size.width * channels) as usize;
            let out_stride = (bold_img.size.width * channels) as usize;
            for y in 0..reg_img.size.height as usize {
                let out_y = y + pad_y as usize;
                for col in 0..in_stride {
                    let in_a = reg_img.bytes[y * in_stride + col];
                    let out_a = bold_img.bytes[out_y * out_stride + col];
                    assert!(
                        out_a >= in_a,
                        "glyph {} lost coverage at ({col}, {y}): {out_a} < {in_a}",
                        glyph.glyph
                    );
                }
            }

            assert!(sum_ink(&semi_img) > sum_ink(&reg_img));
            assert!(sum_ink(&bold_img) > sum_ink(&semi_img));
        }
    }
}

#[test]
fn colour_glyphs_ignore_synthetic_bold_dilation() {
    let fonts = Arc::new(FontSystem::new(FontSystemOptions::registered_only()));
    fonts
        .register(
            support::face("NotoZnamennyMusicalNotation-Regular.ttf"),
            None,
        )
        .expect("colour face registers");
    let style = TextStyle {
        family: FontFamilyList::from_iter([FamilyName::Named(Ident::new(support::COLOR))]),
        ..TextStyle::initial()
    };
    let face = fonts
        .resolve(&FaceQuery::of(&style))
        .expect("colour face resolves");
    let raster = Rasteriser::new(fonts);

    let (plain_key, plain_img) = (1..64u16)
        .find_map(|glyph| {
            let key = GlyphKey::new(face, glyph, 48.0, SubpixelOffset(0), RasterStyle::Color);
            let image = raster.raster(&key)?;
            (image.format == GlyphFormat::Color && !image.is_empty()).then_some((key, image))
        })
        .expect("finds a colour glyph");

    let bold_key = GlyphKey {
        synthetic_bold_bits: 1.5_f32.to_bits(),
        ..plain_key
    };
    let bold_img = raster.raster(&bold_key).expect("colour glyph rasterises");
    assert_eq!(bold_img, plain_img);
}
