//! Repacking a rasterised glyph into the pixel layouts the rest of the pipeline uses.

use swash::scale::image::{Content, Image};
use zgui_geom::{Device, DevicePx, Point, Size};
use zgui_text::{GlyphFormat, GlyphImage};

/// Converts one rasterised glyph, applying winding-independent bitmap emboldening to coverage
/// masks when `bold > 0.0`.
///
/// Two of the three formats are a straight copy and the third is not: the rasteriser reports
/// subpixel coverage as four bytes per pixel with the fourth unused, while a subpixel glyph is
/// three coverage values. Carrying the padding byte all the way to the texture would cost a third
/// of the upload bandwidth of every text-heavy frame for nothing, so it is dropped here, once.
///
/// Synthetic bold is applied here in alpha space rather than through contour normal offsetting in
/// the outline scaler: composite TrueType CJK glyphs whose components carry opposite contour
/// winding erode inward under outline offsetting, whereas morphological alpha dilation is
/// winding-independent and guarantees `out_alpha >= in_alpha` at every existing pixel. Colour
/// glyphs are pictures and are never dilated.
pub(crate) fn convert(image: &Image, bold: f32) -> GlyphImage {
    let width = image.placement.width;
    let height = image.placement.height;
    let left = image.placement.left;
    let top = image.placement.top;

    match image.content {
        Content::Mask => {
            let (size, top, bytes) = dilate(width, height, top, image.data.clone(), 1, bold);
            GlyphImage {
                size,
                placement: Point::new(DevicePx(left as f32), DevicePx(top as f32)),
                format: GlyphFormat::Mono,
                bytes,
            }
        }
        Content::SubpixelMask => {
            let stripped = drop_fourth(&image.data);
            let (size, top, bytes) = dilate(width, height, top, stripped, 3, bold);
            GlyphImage {
                size,
                placement: Point::new(DevicePx(left as f32), DevicePx(top as f32)),
                format: GlyphFormat::Subpixel,
                bytes,
            }
        }
        Content::Color => GlyphImage {
            size: Size::new(width, height),
            placement: Point::new(DevicePx(left as f32), DevicePx(top as f32)),
            format: GlyphFormat::Color,
            bytes: image.data.clone(),
        },
    }
}

/// Keeps the first three bytes of every four.
fn drop_fourth(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    for pixel in data.as_chunks::<4>().0 {
        out.extend_from_slice(&pixel[..3]);
    }
    out
}

/// Fractional 2D alpha dilation for monochrome (`channels == 1`) and LCD subpixel (`channels == 3`)
/// coverage masks.
fn dilate(
    width: u32,
    height: u32,
    top: i32,
    bytes: Vec<u8>,
    channels: usize,
    bold: f32,
) -> (Size<u32, Device>, i32, Vec<u8>) {
    let expected = (width as usize)
        .saturating_mul(height as usize)
        .saturating_mul(channels);
    if !bold.is_finite() || bold <= 0.0 || width == 0 || height == 0 || bytes.len() != expected {
        return (Size::new(width, height), top, bytes);
    }

    let dx = bold.clamp(0.0, 8.0);
    let dy = (bold * 0.5).clamp(0.0, 4.0);
    let pad_x = dx.ceil() as u32;
    let pad_y = dy.ceil() as u32;
    let out_w = width + pad_x;
    let out_h = height + pad_y;
    let out_top = top.saturating_add(pad_y as i32);

    let in_row_bytes = width as usize * channels;
    let out_row_bytes = out_w as usize * channels;

    // Horizontal pass: dilate rightwards by `dx` pixels (`dx * channels` samples on subpixel LCD
    // masks so stems widen smoothly across adjacent R-G-B subpixels without colour fringes).
    let dx_samples = dx * channels as f32;
    let kx = dx_samples.floor() as usize;
    let fx = dx_samples - kx as f32;
    let mut mid = vec![0u8; out_row_bytes * height as usize];
    for y in 0..height as usize {
        let src = &bytes[y * in_row_bytes..(y + 1) * in_row_bytes];
        let dst = &mut mid[y * out_row_bytes..(y + 1) * out_row_bytes];
        for (x, sample) in dst.iter_mut().enumerate() {
            *sample = dilate_right(src, x, kx, fx);
        }
    }

    // Vertical pass: dilate upwards from the baseline by `dy` pixels (`out_top = top + pad_y`), so
    // output row `y` aligns with input row `y - pad_y` on screen and the baseline foot stays put.
    let ky = dy.floor() as isize;
    let fy = dy - ky as f32;
    let in_h = height as isize;
    let pad_y_isize = pad_y as isize;
    let mut out = vec![0u8; out_row_bytes * out_h as usize];
    for y in 0..out_h as usize {
        let u = y as isize - pad_y_isize;
        let r_start = u.max(0);
        let r_end = (u + ky).min(in_h - 1);
        let dst = &mut out[y * out_row_bytes..(y + 1) * out_row_bytes];
        for (col, sample) in dst.iter_mut().enumerate() {
            let mut best = 0u8;
            if r_start <= r_end {
                for r in r_start..=r_end {
                    best = best.max(mid[r as usize * out_row_bytes + col]);
                }
            }
            if fy > 0.0 {
                let r_a = u + ky;
                let r_b = r_a + 1;
                let a = if r_a >= 0 && r_a < in_h {
                    mid[r_a as usize * out_row_bytes + col]
                } else {
                    0
                };
                let b = if r_b >= 0 && r_b < in_h {
                    mid[r_b as usize * out_row_bytes + col]
                } else {
                    0
                };
                let interp = (f32::from(a) * (1.0 - fy) + f32::from(b) * fy).round() as u8;
                best = best.max(interp);
            }
            *sample = best;
        }
    }

    (Size::new(out_w, out_h), out_top, out)
}

/// One sample dilated rightwards by `kx + fx` samples.
fn dilate_right(row: &[u8], x: usize, kx: usize, fx: f32) -> u8 {
    let start = x.saturating_sub(kx);
    let mut best = 0u8;
    if !row.is_empty() && start < row.len() {
        let end = x.min(row.len() - 1);
        for &sample in &row[start..=end] {
            best = best.max(sample);
        }
    }
    if fx > 0.0 {
        let a = x
            .checked_sub(kx)
            .and_then(|index| row.get(index))
            .copied()
            .unwrap_or(0);
        let b = x
            .checked_sub(kx + 1)
            .and_then(|index| row.get(index))
            .copied()
            .unwrap_or(0);
        let interp = (f32::from(a) * (1.0 - fx) + f32::from(b) * fx).round() as u8;
        best = best.max(interp);
    }
    best
}

#[cfg(test)]
mod tests {
    use swash::scale::image::{Content, Image};
    use swash::zeno::Placement;
    use zgui_text::GlyphFormat;

    use super::convert;

    #[test]
    fn monochrome_dilation_strictly_preserves_or_increases_every_screen_pixel() {
        let src = Image {
            content: Content::Mask,
            placement: Placement {
                left: 2,
                top: 10,
                width: 3,
                height: 3,
            },
            data: vec![
                0, 180, 0, //
                120, 255, 90, //
                0, 200, 0,
            ],
            ..Image::default()
        };

        let plain = convert(&src, 0.0);
        assert_eq!(plain.size.width, 3);
        assert_eq!(plain.size.height, 3);
        assert_eq!(plain.placement.x.0, 2.0);
        assert_eq!(plain.placement.y.0, 10.0);
        assert_eq!(plain.bytes, src.data);

        for bold in [0.25_f32, 0.5, 1.0, 1.6] {
            let emboldened = convert(&src, bold);
            let pad_x = bold.clamp(0.0, 8.0).ceil() as u32;
            let pad_y = (bold * 0.5).clamp(0.0, 4.0).ceil() as u32;
            assert_eq!(emboldened.format, GlyphFormat::Mono);
            assert_eq!(emboldened.size.width, 3 + pad_x);
            assert_eq!(emboldened.size.height, 3 + pad_y);
            assert_eq!(emboldened.placement.x.0, 2.0);
            assert_eq!(emboldened.placement.y.0, (10 + pad_y as i32) as f32);

            for y in 0..3u32 {
                for x in 0..3u32 {
                    let in_alpha = plain.bytes[(y * 3 + x) as usize];
                    let out_y = y + pad_y;
                    let out_alpha = emboldened.bytes[(out_y * emboldened.size.width + x) as usize];
                    assert!(
                        out_alpha >= in_alpha,
                        "bold={bold} lost alpha at ({x}, {y}): {out_alpha} < {in_alpha}"
                    );
                }
            }
            let sum_in: u64 = plain.bytes.iter().map(|&b| u64::from(b)).sum();
            let sum_out: u64 = emboldened.bytes.iter().map(|&b| u64::from(b)).sum();
            assert!(sum_out > sum_in, "bold={bold}: {sum_out} <= {sum_in}");
        }
    }

    #[test]
    fn subpixel_dilation_widens_across_rgb_channels_and_colour_glyphs_stay_untouched() {
        let subpixel = Image {
            content: Content::SubpixelMask,
            placement: Placement {
                left: 1,
                top: 8,
                width: 2,
                height: 1,
            },
            data: vec![255, 0, 0, 0, 0, 0, 0, 0],
            ..Image::default()
        };
        let dilated = convert(&subpixel, 1.0 / 3.0);
        assert_eq!(dilated.format, GlyphFormat::Subpixel);
        assert_eq!(dilated.size.width, 3);
        assert_eq!(dilated.size.height, 2);
        // Shifting 1/3 px = 1 LCD subpixel moves coverage into G without jumping straight to next R.
        let bottom_row = &dilated.bytes[9..18];
        assert_eq!(bottom_row[0], 255);
        assert_eq!(bottom_row[1], 255);
        assert_eq!(bottom_row[3], 0);

        let color = Image {
            content: Content::Color,
            placement: Placement {
                left: 0,
                top: 12,
                width: 1,
                height: 1,
            },
            data: vec![10, 20, 30, 200],
            ..Image::default()
        };
        let converted = convert(&color, 1.5);
        assert_eq!(converted.format, GlyphFormat::Color);
        assert_eq!(converted.size.width, 1);
        assert_eq!(converted.size.height, 1);
        assert_eq!(converted.placement.y.0, 12.0);
        assert_eq!(converted.bytes, vec![10, 20, 30, 200]);
    }
}
