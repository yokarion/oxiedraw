//! Single-pixel samplers over flat BGRA8 buffers.

/// Nearest-neighbor sample of a BGRA8 buffer.
#[must_use]
pub fn sample_nearest(src: &[u8], w: u32, h: u32, fx: f32, fy: f32) -> [u8; 4] {
    #[allow(clippy::cast_possible_wrap)]
    let w_i = w as i32;
    #[allow(clippy::cast_possible_wrap)]
    let h_i = h as i32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let x = (fx.floor() as i32).clamp(0, w_i - 1) as u32;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let y = (fy.floor() as i32).clamp(0, h_i - 1) as u32;
    let idx = ((y * w + x) * 4) as usize;
    [src[idx], src[idx + 1], src[idx + 2], src[idx + 3]]
}

/// Bilinear sample of a BGRA8 buffer.
#[must_use]
pub fn sample_bilinear(src: &[u8], w: u32, h: u32, fx: f32, fy: f32) -> [u8; 4] {
    let x0 = (fx - 0.5).floor();
    let y0 = (fy - 0.5).floor();
    let tx = (fx - 0.5) - x0;
    let ty = (fy - 0.5) - y0;
    #[allow(clippy::cast_possible_truncation)]
    let x0i = x0 as i32;
    #[allow(clippy::cast_possible_truncation)]
    let y0i = y0 as i32;
    #[allow(clippy::cast_possible_wrap)]
    let w_i = w as i32;
    #[allow(clippy::cast_possible_wrap)]
    let h_i = h as i32;

    let get = |xi: i32, yi: i32| -> [f32; 4] {
        #[allow(clippy::cast_sign_loss)]
        let x = xi.clamp(0, w_i - 1) as u32;
        #[allow(clippy::cast_sign_loss)]
        let y = yi.clamp(0, h_i - 1) as u32;
        let idx = ((y * w + x) * 4) as usize;
        [
            f32::from(src[idx]),
            f32::from(src[idx + 1]),
            f32::from(src[idx + 2]),
            f32::from(src[idx + 3]),
        ]
    };

    let c00 = get(x0i, y0i);
    let c10 = get(x0i + 1, y0i);
    let c01 = get(x0i, y0i + 1);
    let c11 = get(x0i + 1, y0i + 1);

    let mut out = [0u8; 4];
    for i in 0..4 {
        let top = c00[i].mul_add(1.0 - tx, c10[i] * tx);
        let bot = c01[i].mul_add(1.0 - tx, c11[i] * tx);
        let v = top.mul_add(1.0 - ty, bot * ty);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let v_u8 = v.round().clamp(0.0, 255.0) as u8;
        out[i] = v_u8;
    }
    out
}
