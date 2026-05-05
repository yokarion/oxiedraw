//! Layer filters: hue/saturation/value, invert, box blur, and unsharp
//! sharpen.
//!
//! [`FilterSpec`] is the single source of truth for filter parameters,
//! shared by the UI (popups) and the renderer (which turns each spec into
//! a chain of GPU passes). The actual pixel work runs on Vulkan; the
//! [`apply_cpu`] reference implementation here exists only so the filter
//! math can be unit-tested without a GPU and so the GPU integration tests
//! have something to compare against.
//!
//! All pixel buffers are full-canvas BGRA8, premultiplied alpha, row-major
//! with no padding - the same layout [`crate::canvas::Canvas::read_layer`]
//! returns.

/// A filter and its parameters. Copyable so the live-preview path can
/// stash the latest value behind a `Cell` without allocation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FilterSpec {
    /// Hue rotation (degrees), saturation multiplier, value multiplier.
    /// Identity is `{ 0.0, 1.0, 1.0 }`.
    Hsv {
        hue_degrees: f32,
        saturation: f32,
        value: f32,
    },
    /// Invert colors. No parameters.
    Invert,
    /// Box blur with independent horizontal / vertical radii in pixels.
    BoxBlur { radius_x: f32, radius_y: f32 },
    /// Unsharp-mask sharpen. `amount` of 0 leaves the image unchanged.
    Sharpen { amount: f32 },
}

impl FilterSpec {
    /// Identity HSV adjustment (no visible change).
    #[must_use]
    pub const fn hsv_identity() -> Self {
        Self::Hsv {
            hue_degrees: 0.0,
            saturation: 1.0,
            value: 1.0,
        }
    }

    /// Human-readable name for menus, history labels, and toasts.
    #[must_use]
    pub const fn display_name(&self) -> &'static str {
        match self {
            Self::Hsv { .. } => "Hue/Saturation/Value",
            Self::Invert => "Invert",
            Self::BoxBlur { .. } => "Blur",
            Self::Sharpen { .. } => "Sharpen",
        }
    }

    /// The fixed box-blur radius (pixels) used by the sharpen pass to build
    /// its blurred reference image. Wide enough that the unsharp difference is
    /// visible on soft/anti-aliased art, not just hard pixel edges.
    pub const SHARPEN_BLUR_RADIUS: f32 = 4.0;
}

// ---------------------------------------------------------------------------
// CPU reference implementation (testing / parity only - the live path is GPU)
// ---------------------------------------------------------------------------

/// Apply `spec` to `src` (BGRA8 premultiplied, `width` x `height`) on the
/// CPU and return a new buffer.
///
/// When `mask` is `Some` (R8, same dimensions) the effect is blended by the
/// mask: full strength where the mask is 255, untouched where it is 0. When
/// `None`, the whole layer is filtered.
///
/// This mirrors the GPU shaders closely but is intended for invariants and
/// parity checks, not pixel-exact equality (the GPU works in linear space
/// while this reference stays in 8-bit gamma space).
#[must_use]
pub fn apply_cpu(
    spec: FilterSpec,
    src: &[u8],
    width: u32,
    height: u32,
    mask: Option<&[u8]>,
) -> Vec<u8> {
    let w = width as usize;
    let h = height as usize;
    let filtered = match spec {
        FilterSpec::Hsv {
            hue_degrees,
            saturation,
            value,
        } => point_filter(src, |bgra| hsv_pixel(bgra, hue_degrees, saturation, value)),
        FilterSpec::Invert => point_filter(src, invert_pixel),
        FilterSpec::BoxBlur { radius_x, radius_y } => {
            box_blur(src, w, h, radius_x.round() as i32, radius_y.round() as i32)
        }
        FilterSpec::Sharpen { amount } => {
            let r = FilterSpec::SHARPEN_BLUR_RADIUS.round() as i32;
            let blurred = box_blur(src, w, h, r, r);
            sharpen(src, &blurred, amount)
        }
    };

    match mask {
        Some(m) => mask_mix(src, &filtered, m),
        None => filtered,
    }
}

fn point_filter(src: &[u8], f: impl Fn([u8; 4]) -> [u8; 4]) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    for (dst, px) in out.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        let r = f([px[0], px[1], px[2], px[3]]);
        dst.copy_from_slice(&r);
    }
    out
}

const fn invert_pixel(bgra: [u8; 4]) -> [u8; 4] {
    let a = bgra[3];
    // Premultiplied invert: out = a - channel.
    [
        a.saturating_sub(bgra[0]),
        a.saturating_sub(bgra[1]),
        a.saturating_sub(bgra[2]),
        a,
    ]
}

fn hsv_pixel(bgra: [u8; 4], hue_degrees: f32, sat: f32, val: f32) -> [u8; 4] {
    let a = f32::from(bgra[3]) / 255.0;
    if a <= 0.0 {
        return [0, 0, 0, 0];
    }
    // Unpremultiply to straight color (B, G, R order in the buffer).
    let b = (f32::from(bgra[0]) / 255.0 / a).clamp(0.0, 1.0);
    let g = (f32::from(bgra[1]) / 255.0 / a).clamp(0.0, 1.0);
    let r = (f32::from(bgra[2]) / 255.0 / a).clamp(0.0, 1.0);

    let (mut hh, mut ss, vv) = rgb_to_hsv(r, g, b);
    hh = (hh + hue_degrees / 360.0).rem_euclid(1.0);
    ss = (ss * sat).clamp(0.0, 1.0);
    let (nr, ng, nb) = hsv_to_rgb(hh, ss, vv);

    // Brightness: scale down below 1, additively lift above 1 (matches the
    // GPU shader so saturated pixels still brighten toward white).
    let lift = |c: f32| {
        if val >= 1.0 {
            (c + (val - 1.0)).clamp(0.0, 1.0)
        } else {
            (c * val).clamp(0.0, 1.0)
        }
    };

    [
        premul_u8(lift(nb), a),
        premul_u8(lift(ng), a),
        premul_u8(lift(nr), a),
        bgra[3],
    ]
}

fn premul_u8(straight: f32, alpha: f32) -> u8 {
    (straight * alpha * 255.0).round().clamp(0.0, 255.0) as u8
}

fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let v = max;
    let s = if max <= 0.0 { 0.0 } else { delta / max };
    let h = if delta <= 0.0 {
        0.0
    } else if (max - r).abs() < f32::EPSILON {
        ((g - b) / delta).rem_euclid(6.0) / 6.0
    } else if (max - g).abs() < f32::EPSILON {
        (((b - r) / delta) + 2.0) / 6.0
    } else {
        (((r - g) / delta) + 4.0) / 6.0
    };
    (h, s, v)
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let i = (h * 6.0).floor();
    let f = h.mul_add(6.0, -i);
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - f).mul_add(-s, 1.0);
    match (i as i32).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    }
}

fn box_blur(src: &[u8], w: usize, h: usize, radius_x: i32, radius_y: i32) -> Vec<u8> {
    let horizontal = blur_axis(src, w, h, radius_x.max(0), true);
    blur_axis(&horizontal, w, h, radius_y.max(0), false)
}

fn blur_axis(src: &[u8], w: usize, h: usize, radius: i32, horizontal: bool) -> Vec<u8> {
    if radius == 0 {
        return src.to_vec();
    }
    let mut out = vec![0u8; src.len()];
    let count = f32::from(u16::try_from(2 * radius + 1).unwrap_or(u16::MAX));
    for y in 0..h {
        for x in 0..w {
            let mut sum = [0.0f32; 4];
            for k in -radius..=radius {
                let (sx, sy) = if horizontal {
                    ((x as i32 + k).clamp(0, w as i32 - 1) as usize, y)
                } else {
                    (x, (y as i32 + k).clamp(0, h as i32 - 1) as usize)
                };
                let i = (sy * w + sx) * 4;
                for c in 0..4 {
                    sum[c] += f32::from(src[i + c]);
                }
            }
            let o = (y * w + x) * 4;
            for c in 0..4 {
                out[o + c] = (sum[c] / count).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    out
}

fn sharpen(src: &[u8], blurred: &[u8], amount: f32) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    for ((dst, s), b) in out
        .chunks_exact_mut(4)
        .zip(src.chunks_exact(4))
        .zip(blurred.chunks_exact(4))
    {
        let a = s[3];
        for c in 0..3 {
            let orig = f32::from(s[c]);
            let blur = f32::from(b[c]);
            let sharp = amount.mul_add(orig - blur, orig);
            dst[c] = sharp.round().clamp(0.0, f32::from(a)) as u8;
        }
        dst[3] = a;
    }
    out
}

fn mask_mix(original: &[u8], filtered: &[u8], mask: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; original.len()];
    for (idx, (dst, (o, f))) in out
        .chunks_exact_mut(4)
        .zip(original.chunks_exact(4).zip(filtered.chunks_exact(4)))
        .enumerate()
    {
        let m = f32::from(mask.get(idx).copied().unwrap_or(0)) / 255.0;
        for c in 0..4 {
            let mixed = f32::from(o[c]).mul_add(1.0 - m, f32::from(f[c]) * m);
            dst[c] = mixed.round().clamp(0.0, 255.0) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 2x2 premultiplied BGRA8 swatch with assorted opaque/translucent
    // colors. Opaque so premultiplied == straight for the simple channels.
    fn swatch() -> Vec<u8> {
        vec![
            // B, G, R, A
            10, 20, 200, 255, // reddish
            0, 180, 0, 255, // green
            220, 0, 0, 255, // blue
            128, 128, 128, 255, // grey
        ]
    }

    #[test]
    fn invert_is_an_involution() {
        let src = swatch();
        let once = apply_cpu(FilterSpec::Invert, &src, 2, 2, None);
        let twice = apply_cpu(FilterSpec::Invert, &once, 2, 2, None);
        assert_eq!(src, twice, "inverting twice must restore the original");
    }

    #[test]
    fn invert_flips_opaque_channels() {
        let src = vec![0u8, 0, 0, 255];
        let out = apply_cpu(FilterSpec::Invert, &src, 1, 1, None);
        assert_eq!(out, vec![255, 255, 255, 255], "black inverts to white");
    }

    #[test]
    fn hsv_identity_is_near_lossless() {
        let src = swatch();
        let out = apply_cpu(FilterSpec::hsv_identity(), &src, 2, 2, None);
        for (a, b) in src.iter().zip(out.iter()) {
            assert!(
                (i32::from(*a) - i32::from(*b)).abs() <= 2,
                "identity HSV drifted: {a} vs {b}"
            );
        }
    }

    #[test]
    fn hsv_value_zero_is_black() {
        let src = swatch();
        let out = apply_cpu(
            FilterSpec::Hsv {
                hue_degrees: 0.0,
                saturation: 1.0,
                value: 0.0,
            },
            &src,
            2,
            2,
            None,
        );
        for px in out.chunks_exact(4) {
            assert!(px[0] <= 1 && px[1] <= 1 && px[2] <= 1, "value 0 => black");
            assert_eq!(px[3], 255, "alpha preserved");
        }
    }

    #[test]
    fn hue_rotation_360_is_identity() {
        let src = swatch();
        let out = apply_cpu(
            FilterSpec::Hsv {
                hue_degrees: 360.0,
                saturation: 1.0,
                value: 1.0,
            },
            &src,
            2,
            2,
            None,
        );
        for (a, b) in src.iter().zip(out.iter()) {
            assert!((i32::from(*a) - i32::from(*b)).abs() <= 2);
        }
    }

    #[test]
    fn blur_of_constant_is_constant() {
        let src = vec![40u8, 80, 120, 255].repeat(16); // 4x4 solid
        let out = apply_cpu(
            FilterSpec::BoxBlur {
                radius_x: 2.0,
                radius_y: 2.0,
            },
            &src,
            4,
            4,
            None,
        );
        assert_eq!(out, src, "blurring a flat color must not change it");
    }

    #[test]
    fn blur_zero_radius_is_identity() {
        let src = swatch();
        let out = apply_cpu(
            FilterSpec::BoxBlur {
                radius_x: 0.0,
                radius_y: 0.0,
            },
            &src,
            2,
            2,
            None,
        );
        assert_eq!(out, src);
    }

    #[test]
    fn blur_averages_a_spike() {
        // Single bright pixel in a 3x3 black field; a radius-1 box blur
        // should spread it and lower the center.
        let mut src = vec![0u8; 3 * 3 * 4];
        let center = (1 * 3 + 1) * 4;
        src[center] = 255;
        src[center + 3] = 255; // alpha
        let out = apply_cpu(
            FilterSpec::BoxBlur {
                radius_x: 1.0,
                radius_y: 1.0,
            },
            &src,
            3,
            3,
            None,
        );
        assert!(out[center] < 255, "center should be reduced by blur");
        assert!(out[0] > 0, "corner should pick up some energy");
    }

    #[test]
    fn sharpen_zero_amount_is_identity() {
        let src = swatch();
        let out = apply_cpu(FilterSpec::Sharpen { amount: 0.0 }, &src, 2, 2, None);
        assert_eq!(out, src);
    }

    #[test]
    fn sharpen_of_constant_is_constant() {
        let src = vec![40u8, 80, 120, 255].repeat(16);
        let out = apply_cpu(FilterSpec::Sharpen { amount: 3.0 }, &src, 4, 4, None);
        assert_eq!(out, src, "sharpening a flat color is a no-op");
    }

    #[test]
    fn mask_zero_leaves_pixels_untouched() {
        let src = swatch();
        let mask = vec![0u8; 4]; // fully outside the selection
        let out = apply_cpu(FilterSpec::Invert, &src, 2, 2, Some(&mask));
        assert_eq!(out, src, "masked-out pixels must be unchanged");
    }

    #[test]
    fn mask_full_applies_everywhere() {
        let src = swatch();
        let mask = vec![255u8; 4];
        let masked = apply_cpu(FilterSpec::Invert, &src, 2, 2, Some(&mask));
        let unmasked = apply_cpu(FilterSpec::Invert, &src, 2, 2, None);
        assert_eq!(masked, unmasked, "full mask == no mask");
    }

    #[test]
    fn mask_partial_blends() {
        // Half-selected pixel: result is halfway between original and inverted.
        let src = vec![0u8, 0, 0, 255];
        let mask = vec![128u8];
        let out = apply_cpu(FilterSpec::Invert, &src, 1, 1, Some(&mask));
        // invert(black opaque) = white; blend at ~0.5 => ~128.
        assert!((i32::from(out[0]) - 128).abs() <= 2);
    }
}
