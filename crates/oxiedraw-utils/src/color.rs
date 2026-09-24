//! Dependency-free sRGB color math: HSV conversions, hex parsing/formatting,
//! and the sRGB -> linear transfer function. These operate on plain `u8`/`f32`
//! channels so they can back the higher-level color types in `oxiedraw-core`.

use crate::math::clamp01;

/// Convert an sRGB-encoded 8-bit channel to a linear float in `[0, 1]`.
///
/// Matches the IEC 61966-2-1 piecewise curve the GPU uses on sRGB-format
/// reads/writes (the canvas attachment is `R8G8B8A8_SRGB`, whose composite
/// pipeline expects linear input).
#[inline]
#[must_use]
pub fn srgb_to_linear(c: u8) -> f32 {
    let s = f32::from(c) / 255.0;
    if s <= 0.040_45 {
        s / 12.92
    } else {
        ((s + 0.055) / 1.055).powf(2.4)
    }
}

/// Convert a linear float channel in `[0, 1]` to an sRGB-encoded 8-bit value.
///
/// Inverse of [`srgb_to_linear`], matching the same IEC 61966-2-1 curve the GPU
/// applies on sRGB-format writes (and `present_convert.frag` applies by hand).
/// Inputs outside `[0, 1]` are clamped.
#[inline]
#[must_use]
pub fn linear_to_srgb(c: f32) -> u8 {
    let l = clamp01(c);
    let s = if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    // Round-to-nearest so it inverts `srgb_to_linear` exactly across 0..=255.
    (s * 255.0).round() as u8
}

/// Convert HSV (each in `[0, 1]`, hue wrapping) to 8-bit sRGB RGB.
#[allow(clippy::many_single_char_names)]
#[must_use]
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let h = h.rem_euclid(1.0) * 6.0;
    let s = clamp01(s);
    let v = clamp01(v);
    let c = v * s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let to_u8 = |chan: f32| ((chan + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [to_u8(r), to_u8(g), to_u8(b)]
}

/// Convert 8-bit sRGB RGB to HSV, each component in `[0, 1]`.
#[allow(clippy::many_single_char_names)]
#[must_use]
pub fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = f32::from(r) / 255.0;
    let g = f32::from(g) / 255.0;
    let b = f32::from(b) / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let h = if d <= f32::EPSILON {
        0.0
    } else if (max - r).abs() < f32::EPSILON {
        ((g - b) / d).rem_euclid(6.0)
    } else if (max - g).abs() < f32::EPSILON {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    let h = h / 6.0;
    let s = if max <= f32::EPSILON { 0.0 } else { d / max };
    (h, s, max)
}

/// Rec. 709 luma of an sRGB colour, in `[0, 255]`.
#[inline]
#[must_use]
pub fn luma(r: u8, g: u8, b: u8) -> f32 {
    0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)
}

/// Convert sRGB to OKLab `[L, a, b]`, where euclidean distance tracks how
/// different two colours look. `L` runs 0..1; `a` and `b` stay inside +/-0.35
/// for anything in the sRGB gamut.
#[must_use]
pub fn rgb_to_oklab(r: u8, g: u8, b: u8) -> [f32; 3] {
    linear_rgb_to_oklab([srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b)])
}

/// [`rgb_to_oklab`] from channels that are already linear. Hot loops tabulate
/// the transfer function and come in here, so the matrix lives in one place.
#[must_use]
pub fn linear_rgb_to_oklab([r, g, b]: [f32; 3]) -> [f32; 3] {
    let l = (0.412_221_5 * r + 0.536_332_55 * g + 0.051_445_995 * b).cbrt();
    let m = (0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b).cbrt();
    let s = (0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b).cbrt();
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// Euclidean OKLab distance: 0 is identical, and roughly 1 spans black to white.
#[inline]
#[must_use]
pub fn oklab_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (dl, da, db) = (a[0] - b[0], a[1] - b[1], a[2] - b[2]);
    dl.mul_add(dl, da.mul_add(da, db * db)).sqrt()
}

/// Parse a `#rrggbb` (or `rrggbb`) hex string into RGB channels.
#[must_use]
pub fn parse_hex_rgb(text: &str) -> Option<[u8; 3]> {
    let hex = text.trim().trim_start_matches('#');
    // Byte-indexed below, so a multi-byte character of the right byte length
    // would slice mid-character and panic.
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some([r, g, b])
}

/// Format RGB channels as a lowercase `#rrggbb` hex string.
#[must_use]
pub fn rgb_to_hex(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::{linear_to_srgb, oklab_distance, rgb_to_oklab, srgb_to_linear};

    #[test]
    fn oklab_anchors_black_white_and_neutrals() {
        let black = rgb_to_oklab(0, 0, 0);
        let white = rgb_to_oklab(255, 255, 255);
        assert!(black[0].abs() < 1e-4, "black L is {}", black[0]);
        assert!((white[0] - 1.0).abs() < 1e-3, "white L is {}", white[0]);
        for level in [0u8, 64, 128, 200, 255] {
            let grey = rgb_to_oklab(level, level, level);
            assert!(grey[1].abs() < 1e-3 && grey[2].abs() < 1e-3, "grey {level}: {grey:?}");
        }
    }

    #[test]
    fn oklab_lightness_rises_and_chroma_stays_in_range() {
        let mut previous = -1.0_f32;
        for level in 0u8..=255 {
            let lab = rgb_to_oklab(level, level, level);
            assert!(lab[0] >= previous, "L fell at {level}");
            previous = lab[0];
        }
        for r in (0u8..=255).step_by(17) {
            for g in (0u8..=255).step_by(17) {
                for b in (0u8..=255).step_by(17) {
                    let lab = rgb_to_oklab(r, g, b);
                    assert!((0.0..=1.001).contains(&lab[0]), "L out of range: {lab:?}");
                    assert!(lab[1].abs() <= 0.35 && lab[2].abs() <= 0.35, "ab out of range: {lab:?}");
                }
            }
        }
    }

    #[test]
    fn oklab_distance_is_zero_only_for_the_same_color() {
        let red = rgb_to_oklab(220, 40, 40);
        assert_eq!(oklab_distance(red, red), 0.0);
        let near = rgb_to_oklab(222, 42, 42);
        let far = rgb_to_oklab(40, 60, 220);
        assert!(oklab_distance(red, near) < oklab_distance(red, far));
        assert!(oklab_distance(rgb_to_oklab(0, 0, 0), rgb_to_oklab(255, 255, 255)) > 0.9);
    }

    #[test]
    fn srgb_round_trips_through_linear_for_every_byte() {
        for c in 0u8..=255 {
            let back = linear_to_srgb(srgb_to_linear(c));
            assert_eq!(back, c, "byte {c} round-tripped to {back}");
        }
    }

    #[test]
    fn linear_to_srgb_anchors() {
        assert_eq!(linear_to_srgb(0.0), 0);
        assert_eq!(linear_to_srgb(1.0), 255);
        // The curve lifts midtones well above mid grey.
        assert_eq!(linear_to_srgb(0.5), 188);
    }

    #[test]
    fn linear_to_srgb_uses_linear_segment_near_black() {
        // Below the 0.0031308 cutoff the curve is a plain 12.92x ramp.
        assert_eq!(linear_to_srgb(0.001), (0.001_f32 * 12.92 * 255.0).round() as u8);
        assert_eq!(linear_to_srgb(0.003), (0.003_f32 * 12.92 * 255.0).round() as u8);
    }

    #[test]
    fn linear_to_srgb_clamps_out_of_range() {
        assert_eq!(linear_to_srgb(-1.0), 0);
        assert_eq!(linear_to_srgb(2.0), 255);
        assert_eq!(linear_to_srgb(f32::NAN), 0);
    }
}
