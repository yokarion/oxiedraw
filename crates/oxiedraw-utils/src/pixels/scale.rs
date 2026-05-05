//! Bilinear resampling of flat BGRA8 buffers.

use crate::math::clamp01;

/// Scale a BGRA8 buffer by `factor`, returning the resized buffer and its
/// new dimensions. A factor of 1.0 (or one that rounds to the source size)
/// returns a copy unchanged.
#[must_use]
pub fn scale(bgra8: &[u8], sw: u32, sh: u32, factor: f32) -> (Vec<u8>, u32, u32) {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let dw = ((sw as f32 * factor).round() as u32).max(1);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let dh = ((sh as f32 * factor).round() as u32).max(1);
    if dw == sw && dh == sh {
        return (bgra8.to_vec(), sw, sh);
    }
    (scale_bgra8_bilinear(bgra8, sw, sh, dw, dh), dw, dh)
}

/// Resample a `sw x sh` BGRA8 buffer to `dw x dh` using bilinear filtering.
#[must_use]
pub fn scale_bgra8_bilinear(src: &[u8], sw: u32, sh: u32, dw: u32, dh: u32) -> Vec<u8> {
    let mut dst = vec![0u8; (dw * dh * 4) as usize];
    #[allow(clippy::cast_precision_loss)]
    let x_ratio = sw as f32 / dw as f32;
    #[allow(clippy::cast_precision_loss)]
    let y_ratio = sh as f32 / dh as f32;

    for dy in 0..dh {
        for dx in 0..dw {
            #[allow(clippy::cast_precision_loss)]
            let sx = (dx as f32 + 0.5).mul_add(x_ratio, -0.5);
            #[allow(clippy::cast_precision_loss)]
            let sy = (dy as f32 + 0.5).mul_add(y_ratio, -0.5);

            let x0 = (sx.floor() as i32).max(0).min(sw as i32 - 1) as u32;
            let y0 = (sy.floor() as i32).max(0).min(sh as i32 - 1) as u32;
            let x1 = (x0 + 1).min(sw - 1);
            let y1 = (y0 + 1).min(sh - 1);

            let tx = clamp01(sx - sx.floor());
            let ty = clamp01(sy - sy.floor());
            let itx = 1.0 - tx;
            let ity = 1.0 - ty;

            let p00 = (y0 * sw + x0) as usize * 4;
            let p10 = (y0 * sw + x1) as usize * 4;
            let p01 = (y1 * sw + x0) as usize * 4;
            let p11 = (y1 * sw + x1) as usize * 4;
            let di = (dy * dw + dx) as usize * 4;

            for c in 0..4 {
                let v = (f32::from(src[p11 + c]) * tx).mul_add(
                    ty,
                    (f32::from(src[p01 + c]) * itx).mul_add(
                        ty,
                        (f32::from(src[p00 + c]) * itx)
                            .mul_add(ity, f32::from(src[p10 + c]) * tx * ity),
                    ),
                );
                dst[di + c] = v.round() as u8;
            }
        }
    }
    dst
}
