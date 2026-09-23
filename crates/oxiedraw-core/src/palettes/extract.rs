//! Pulling a palette out of an image.
//!
//! One pass over the pixels into a coarse RGB histogram, then the loudest
//! buckets are merged by colour distance and trimmed to the count asked for.
//! Cheap enough to re-run on every slider move, which is what the Extract
//! Palette window does to keep its preview live.

use crate::color::Color;
use crate::enum_meta::EnumMeta;
use oxiedraw_utils::color as color_math;

/// Bits kept per channel when bucketing: 32 levels per axis, fine enough to
/// keep neighbouring shades apart and coarse enough to stay a few thousand.
const BUCKET_BITS: u32 = 5;
const BUCKET_LEVELS: usize = 1 << BUCKET_BITS;
const BUCKET_COUNT: usize = BUCKET_LEVELS * BUCKET_LEVELS * BUCKET_LEVELS;

/// Pixels sampled at most. Larger images are strided down to roughly this.
const SAMPLE_BUDGET: usize = 262_144;

/// Alpha below which a pixel is treated as empty canvas.
const ALPHA_FLOOR: u8 = 24;

/// Lightness bands dropped by `skip_extremes`.
const NEAR_BLACK: f32 = 0.06;
const NEAR_WHITE: f32 = 0.96;

/// Distance a full merge slider allows between two colours that get folded
/// together. Past roughly this, distinct hues start collapsing.
const MAX_MERGE_DISTANCE: f32 = 0.45;

pub const MIN_EXTRACTED_COLORS: usize = 2;
pub const MAX_EXTRACTED_COLORS: usize = 24;

/// How the extracted colours are laid out in the finished palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractOrder {
    Frequency,
    Hue,
    Lightness,
}

impl EnumMeta for ExtractOrder {
    const ALL: &'static [Self] = &[Self::Frequency, Self::Hue, Self::Lightness];

    fn label(self) -> &'static str {
        match self {
            Self::Frequency => "Frequency",
            Self::Hue => "Hue",
            Self::Lightness => "Lightness",
        }
    }
}

/// Knobs the Extract Palette window exposes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtractOptions {
    pub max_colors: usize,
    /// 0 keeps every bucket apart, 1 folds anything within
    /// [`MAX_MERGE_DISTANCE`] together.
    pub merge_similar: f32,
    /// Saturation a pixel needs to count at all.
    pub min_saturation: f32,
    pub order: ExtractOrder,
    /// Drop the near-black and near-white ends, which otherwise dominate any
    /// drawing on a white background.
    pub skip_extremes: bool,
}

impl Default for ExtractOptions {
    fn default() -> Self {
        Self {
            max_colors: 12,
            merge_similar: 0.34,
            min_saturation: 0.08,
            order: ExtractOrder::Frequency,
            skip_extremes: false,
        }
    }
}

/// One colour of the result, with the share of sampled pixels behind it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExtractedColor {
    pub color: Color,
    /// Fraction of the surviving samples, summing to 1 across the result.
    pub weight: f32,
}

#[derive(Clone, Copy, Default)]
struct Bucket {
    count: u32,
    r: u64,
    g: u64,
    b: u64,
}

impl Bucket {
    fn mean(self) -> Color {
        let n = u64::from(self.count).max(1);
        Color::new(
            (self.r / n).min(255) as u8,
            (self.g / n).min(255) as u8,
            (self.b / n).min(255) as u8,
        )
    }
}

/// Extract a palette from premultiplied BGRA8 pixels, the layout the canvas
/// reads back in. `mask` is one R8 byte per pixel; only non-zero ones count.
pub fn extract_palette(
    bgra: &[u8],
    width: u32,
    mask: Option<&[u8]>,
    options: &ExtractOptions,
) -> Vec<ExtractedColor> {
    let pixels = bgra.len() / 4;
    let width = width as usize;
    if pixels == 0 || width == 0 {
        return Vec::new();
    }
    let height = pixels / width;
    // A square grid, not every Nth pixel of the flat buffer: a stride that
    // divides the width lands on the same few columns forever. A mask sizes
    // the grid by what it covers, so a small selection is still sampled finely.
    let covered = mask.map_or(pixels, |m| m.iter().filter(|byte| **byte != 0).count());
    if covered == 0 {
        return Vec::new();
    }
    let step = (covered.div_ceil(SAMPLE_BUDGET) as f64).sqrt().ceil() as usize;
    let step = step.max(1);

    let mut buckets = vec![Bucket::default(); BUCKET_COUNT];
    let mut sampled = 0_u64;
    for y in (0..height).step_by(step) {
        for x in (0..width).step_by(step) {
            let index = y * width + x;
            if mask.is_some_and(|m| m.get(index).copied().unwrap_or(0) == 0) {
                continue;
            }
            let p = &bgra[index * 4..index * 4 + 4];
            let alpha = p[3];
            if alpha < ALPHA_FLOOR {
                continue;
            }
            let (r, g, b) = (
                straighten(p[2], alpha),
                straighten(p[1], alpha),
                straighten(p[0], alpha),
            );
            if !keeps(r, g, b, options) {
                continue;
            }
            let bucket = &mut buckets[bucket_index(r, g, b)];
            bucket.count += 1;
            bucket.r += u64::from(r);
            bucket.g += u64::from(g);
            bucket.b += u64::from(b);
            sampled += 1;
        }
    }
    if sampled == 0 {
        return Vec::new();
    }

    let mut ranked: Vec<(Color, u32)> = buckets
        .iter()
        .filter(|b| b.count > 0)
        .map(|b| (b.mean(), b.count))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| rgb_key(a.0).cmp(&rgb_key(b.0))));

    // Re-ranked after merging: an accepted colour's share grows with everything
    // folded into it, so a gradient spread over many small buckets can outweigh
    // a flat colour that was louder on its own.
    let mut merged = merge(&ranked, options.merge_similar * MAX_MERGE_DISTANCE);
    merged.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| rgb_key(a.0).cmp(&rgb_key(b.0))));
    let kept: Vec<(Color, u32)> = merged
        .into_iter()
        .take(options.max_colors.clamp(MIN_EXTRACTED_COLORS, MAX_EXTRACTED_COLORS))
        .collect();

    let total: f32 = kept.iter().map(|(_, n)| *n as f32).sum();
    let mut out: Vec<ExtractedColor> = kept
        .into_iter()
        .map(|(color, count)| ExtractedColor {
            color,
            weight: if total > 0.0 { count as f32 / total } else { 0.0 },
        })
        .collect();
    order(&mut out, options.order);
    out
}

/// Un-premultiply one channel back to its straight value.
fn straighten(channel: u8, alpha: u8) -> u8 {
    if alpha == 0 || alpha == 255 {
        return channel;
    }
    // The cast saturates, which is the clamp for a channel above its alpha.
    (f32::from(channel) * 255.0 / f32::from(alpha)) as u8
}

fn rgb_key(color: Color) -> (u8, u8, u8) {
    (color.r, color.g, color.b)
}

fn keeps(r: u8, g: u8, b: u8, options: &ExtractOptions) -> bool {
    let (_, saturation, _) = color_math::rgb_to_hsv(r, g, b);
    if saturation < options.min_saturation {
        return false;
    }
    // Lightness, not HSV value: value is the brightest channel, so it calls
    // pure red and yellow "near-white" and throws the vivid colours away.
    let lightness = super::lightness(Color::new(r, g, b));
    !options.skip_extremes || (lightness > NEAR_BLACK && lightness < NEAR_WHITE)
}

const fn bucket_index(r: u8, g: u8, b: u8) -> usize {
    let shift = 8 - BUCKET_BITS;
    let (r, g, b) = (
        (r >> shift) as usize,
        (g >> shift) as usize,
        (b >> shift) as usize,
    );
    (r * BUCKET_LEVELS + g) * BUCKET_LEVELS + b
}

/// Accepted colours kept while merging. Past this the rest fold into their
/// nearest, bounding a scan that was buckets x buckets on a photo-like canvas.
const MERGE_CAP: usize = 64;

/// Fold each bucket into the first accepted colour within `threshold`. The
/// weights of the folded buckets carry over, so nothing loses its share.
fn merge(ranked: &[(Color, u32)], threshold: f32) -> Vec<(Color, u32)> {
    let mut accepted: Vec<(Color, u32)> = Vec::new();
    for &(color, count) in ranked {
        let nearest = accepted
            .iter()
            .enumerate()
            .map(|(index, (kept, _))| (index, distance(*kept, color)))
            .min_by(|a, b| a.1.total_cmp(&b.1));
        match nearest {
            Some((index, gap)) if gap <= threshold || accepted.len() >= MERGE_CAP => {
                accepted[index].1 += count;
            }
            _ => accepted.push((color, count)),
        }
    }
    accepted
}

/// Luma-weighted RGB distance, normalised so 0 is identical and 1 is the
/// black-to-white span.
fn distance(a: Color, b: Color) -> f32 {
    let dr = (f32::from(a.r) - f32::from(b.r)) / 255.0;
    let dg = (f32::from(a.g) - f32::from(b.g)) / 255.0;
    let db = (f32::from(a.b) - f32::from(b.b)) / 255.0;
    (0.30 * dr * dr + 0.59 * dg * dg + 0.11 * db * db).sqrt()
}

fn order(colors: &mut [ExtractedColor], order: ExtractOrder) {
    match order {
        ExtractOrder::Frequency => colors.sort_by(|a, b| b.weight.total_cmp(&a.weight)),
        ExtractOrder::Hue => colors.sort_by(|a, b| {
            super::hue_rank(a.color).total_cmp(&super::hue_rank(b.color))
        }),
        ExtractOrder::Lightness => colors.sort_by(|a, b| {
            super::lightness(a.color).total_cmp(&super::lightness(b.color))
        }),
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::unwrap_used)]
mod tests {
    use super::*;

    /// One opaque row of premultiplied BGRA, which for alpha 255 is just the
    /// channels swapped.
    fn image(colors: &[(Color, usize)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (color, count) in colors {
            for _ in 0..*count {
                out.extend_from_slice(&[color.b, color.g, color.r, 255]);
            }
        }
        out
    }

    fn width_of(pixels: &[u8]) -> u32 {
        (pixels.len() / 4) as u32
    }

    fn extract(pixels: &[u8], options: &ExtractOptions) -> Vec<ExtractedColor> {
        extract_palette(pixels, width_of(pixels), None, options)
    }

    #[test]
    fn the_loudest_colors_come_out_in_frequency_order() {
        let red = Color::new(220, 40, 40);
        let blue = Color::new(40, 60, 220);
        let green = Color::new(40, 200, 80);
        let pixels = image(&[(red, 50), (blue, 30), (green, 10)]);
        let options = ExtractOptions {
            max_colors: 3,
            merge_similar: 0.0,
            order: ExtractOrder::Frequency,
            ..ExtractOptions::default()
        };

        let out = extract(&pixels, &options);
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].color, red);
        assert_eq!(out[1].color, blue);
        assert_eq!(out[2].color, green);
        assert!((out.iter().map(|c| c.weight).sum::<f32>() - 1.0).abs() < 1e-4);
    }

    #[test]
    fn merging_folds_neighbouring_shades_into_one_entry() {
        let a = Color::new(200, 60, 60);
        let b = Color::new(206, 66, 66);
        let pixels = image(&[(a, 40), (b, 40)]);

        let apart = extract(
            &pixels,
            &ExtractOptions {
                merge_similar: 0.0,
                min_saturation: 0.0,
                ..ExtractOptions::default()
            },
        );
        let together = extract(
            &pixels,
            &ExtractOptions {
                merge_similar: 0.5,
                min_saturation: 0.0,
                ..ExtractOptions::default()
            },
        );
        assert!(apart.len() >= together.len());
        assert_eq!(together.len(), 1);
        assert_eq!(together[0].weight, 1.0);
    }

    #[test]
    fn transparent_pixels_never_count() {
        let mut pixels = image(&[(Color::new(180, 90, 40), 20)]);
        pixels.extend_from_slice(&[10, 200, 10, 0]);

        let out = extract(&pixels, &ExtractOptions::default());
        assert_eq!(out.len(), 1, "only the opaque tone survives");
        assert_eq!(out[0].color, Color::new(180, 90, 40));
    }

    #[test]
    fn skipping_extremes_drops_paper_and_ink_but_keeps_vivid_colors() {
        let red = Color::new(255, 0, 0);
        let yellow = Color::new(255, 255, 0);
        let pixels = image(&[
            (red, 10),
            (yellow, 10),
            (Color::WHITE, 10),
            (Color::BLACK, 10),
        ]);
        let options = ExtractOptions {
            skip_extremes: true,
            min_saturation: 0.0,
            merge_similar: 0.0,
            ..ExtractOptions::default()
        };

        let kept: Vec<Color> = extract(&pixels, &options).iter().map(|c| c.color).collect();
        // Both are at the top of the HSV value range, which is what used to
        // throw them out with the paper.
        assert!(kept.contains(&red), "pure red survives: {kept:?}");
        assert!(kept.contains(&yellow), "pure yellow survives: {kept:?}");
        assert!(!kept.contains(&Color::WHITE));
        assert!(!kept.contains(&Color::BLACK));
    }

    #[test]
    fn premultiplied_pixels_come_back_at_their_straight_color() {
        // Mid-grey at half alpha: premultiplied stores ~64, straight is ~128.
        let pixels = vec![64, 64, 64, 128];
        let out = extract_palette(&pixels, 1, None, &ExtractOptions {
            min_saturation: 0.0,
            ..ExtractOptions::default()
        });
        assert_eq!(out.len(), 1);
        assert!(out[0].color.r > 120, "un-premultiplied: {:?}", out[0].color);
    }

    #[test]
    fn a_mask_limits_sampling_to_the_selection() {
        let inside = Color::new(30, 120, 200);
        let outside = Color::new(200, 120, 30);
        let pixels = image(&[(inside, 4), (outside, 4)]);
        let mask = vec![255, 255, 255, 255, 0, 0, 0, 0];

        let out = extract_palette(&pixels, 8, Some(&mask), &ExtractOptions::default());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].color, inside);
    }

    /// A flat stride landed on the same few columns of a wide canvas, so a
    /// narrow selection could fall between them and report nothing to sample.
    #[test]
    fn a_narrow_selection_on_a_wide_canvas_is_still_found() {
        let width = 4096_usize;
        let height = 4096_usize;
        let ink = Color::new(30, 120, 200);
        let mut pixels = vec![0_u8; width * height * 4];
        let mut mask = vec![0_u8; width * height];
        for y in 0..height {
            for x in 65..115 {
                let index = y * width + x;
                pixels[index * 4..index * 4 + 4]
                    .copy_from_slice(&[ink.b, ink.g, ink.r, 255]);
                mask[index] = 255;
            }
        }

        let out = extract_palette(&pixels, width as u32, Some(&mask), &ExtractOptions::default());
        assert_eq!(out.len(), 1, "the selection is sampled");
        assert_eq!(out[0].color, ink);
    }

    /// Colours are trimmed to the count by merged share, so a colour spread
    /// over many small buckets outranks a flat colour that was louder alone.
    #[test]
    fn a_spread_out_color_survives_the_trim() {
        let mut bands: Vec<(Color, usize)> = Vec::new();
        for step in 0..30_u8 {
            bands.push((Color::new(20 + step * 3, 60 + step * 3, 200), 40));
        }
        let flat = Color::new(240, 40, 40);
        bands.push((flat, 300));
        let pixels = image(&bands);

        let out = extract(&pixels, &ExtractOptions {
            max_colors: 2,
            merge_similar: 0.5,
            min_saturation: 0.0,
            order: ExtractOrder::Frequency,
            ..ExtractOptions::default()
        });
        assert_eq!(out.len(), 2);
        assert!(
            out[0].weight > out[1].weight,
            "the merged gradient leads: {out:?}"
        );
        assert!(out.iter().any(|c| distance(c.color, flat) < 0.1));
    }

    #[test]
    fn an_empty_or_fully_masked_image_yields_nothing() {
        assert!(extract_palette(&[], 0, None, &ExtractOptions::default()).is_empty());
        let pixels = image(&[(Color::new(120, 60, 30), 4)]);
        let mask = vec![0, 0, 0, 0];
        assert!(
            extract_palette(&pixels, 4, Some(&mask), &ExtractOptions::default()).is_empty()
        );
    }

    #[test]
    fn lightness_order_runs_dark_to_light() {
        let pixels = image(&[
            (Color::new(230, 200, 120), 10),
            (Color::new(60, 40, 20), 10),
            (Color::new(140, 110, 60), 10),
        ]);
        let out = extract(
            &pixels,
            &ExtractOptions {
                order: ExtractOrder::Lightness,
                ..ExtractOptions::default()
            },
        );
        assert_eq!(out.len(), 3);
        for pair in out.windows(2) {
            assert!(super::super::lightness(pair[0].color) <= super::super::lightness(pair[1].color));
        }
    }
}
