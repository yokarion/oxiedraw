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

/// Parse a `#rrggbb` (or `rrggbb`) hex string into RGB channels.
#[must_use]
pub fn parse_hex_rgb(text: &str) -> Option<[u8; 3]> {
    let hex = text.trim().trim_start_matches('#');
    if hex.len() != 6 {
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
mod tests {
    use super::{linear_to_srgb, srgb_to_linear};

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
