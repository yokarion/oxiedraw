//! Pixel-format conversions between premultiplied BGRA8 (the in-memory and
//! cairo `ARgb32` layout) and the straight RGBA8 / RGB8 layouts that image
//! encoders and decoders expect. All operate on flat row-major byte buffers
//! with no padding.

/// Premultiplied BGRA8 -> straight RGBA8.
#[must_use]
pub fn premul_bgra8_to_rgba8(bgra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bgra.len());
    for p in bgra.chunks_exact(4) {
        let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else if a == 255 {
            out.extend_from_slice(&[r, g, b, 255]);
        } else {
            let af = f32::from(a);
            let ru = (f32::from(r) * 255.0 / af).min(255.0) as u8;
            let gu = (f32::from(g) * 255.0 / af).min(255.0) as u8;
            let bu = (f32::from(b) * 255.0 / af).min(255.0) as u8;
            out.extend_from_slice(&[ru, gu, bu, a]);
        }
    }
    out
}

/// Premultiplied BGRA8 -> straight RGB8 composited over a white background.
#[must_use]
pub fn premul_bgra8_over_white_to_rgb8(bgra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bgra.len() / 4 * 3);
    for p in bgra.chunks_exact(4) {
        let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
        let inv = 255 - a;
        // premult + white * (1-alpha) = premult_chan + inv_alpha; saturating because GPU rounding
        // can produce a premultiplied channel 1 above its alpha, making the sum 256
        out.push(r.saturating_add(inv));
        out.push(g.saturating_add(inv));
        out.push(b.saturating_add(inv));
    }
    out
}

/// Straight RGBA8 -> premultiplied BGRA8 for cairo `ARgb32`.
#[must_use]
pub fn straight_rgba8_to_premul_bgra8(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for p in rgba.chunks_exact(4) {
        let (r, g, b, a) = (p[0], p[1], p[2], p[3]);
        if a == 255 {
            out.extend_from_slice(&[b, g, r, 255]);
        } else if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let af = f32::from(a);
            out.extend_from_slice(&[
                (f32::from(b) * af / 255.0).round() as u8,
                (f32::from(g) * af / 255.0).round() as u8,
                (f32::from(r) * af / 255.0).round() as u8,
                a,
            ]);
        }
    }
    out
}

/// Opaque RGB8 -> premultiplied BGRA8 (alpha=255) for cairo `ARgb32`.
#[must_use]
pub fn rgb8_to_opaque_bgra8(rgb: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgb.len() / 3 * 4);
    for p in rgb.chunks_exact(3) {
        out.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
    out
}
