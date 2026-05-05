/// Crop a flat BGRA8 image buffer.
///
/// `crop_x`/`crop_y` are the top-left of the desired output region in
/// source coordinates; they may be negative. Pixels outside the source
/// bounds are transparent (zero). Output is exactly `w * h * 4` bytes.
#[must_use]
pub fn crop_bgra8(
    raw: &[u8],
    src_w: u32,
    src_h: u32,
    crop_x: i64,
    crop_y: i64,
    w: u32,
    h: u32,
) -> Vec<u8> {
    let mut out = vec![0u8; (w * h * 4) as usize];
    for dst_row in 0..h {
        let src_row = crop_y + i64::from(dst_row);
        if src_row < 0 || src_row >= i64::from(src_h) {
            continue;
        }
        let src_col_lo = crop_x.max(0).min(i64::from(src_w));
        let src_col_hi = (crop_x + i64::from(w)).max(0).min(i64::from(src_w));
        if src_col_lo >= src_col_hi {
            continue;
        }
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let cols = (src_col_hi - src_col_lo) as u32;
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let dst_col_offset = (src_col_lo - crop_x) as u32;

        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let src_start = (src_row as u32 * src_w + src_col_lo as u32) as usize * 4;
        let dst_start = (dst_row * w + dst_col_offset) as usize * 4;
        let len = cols as usize * 4;
        out[dst_start..dst_start + len].copy_from_slice(&raw[src_start..src_start + len]);
    }
    out
}
