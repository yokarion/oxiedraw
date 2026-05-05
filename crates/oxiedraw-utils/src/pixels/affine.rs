//! Multi-threaded affine remap of BGRA8 buffers.

use super::{sample_bilinear, sample_nearest};
use crate::geometry::{TransformFilter, TransformRect};

/// Render `src` into an `out_w x out_h` output, remapping content from
/// `original_rect` (src coordinates) to `current_rect` (output coords).
///
/// Output rows are computed in parallel via `std::thread::scope`. Out-of
/// -range samples clamp to the nearest source edge.
#[allow(clippy::too_many_arguments, clippy::similar_names)]
#[must_use]
pub fn transform_bgra8(
    src: &[u8],
    src_w: u32,
    src_h: u32,
    out_w: u32,
    out_h: u32,
    original_rect: TransformRect,
    current_rect: TransformRect,
    filter: TransformFilter,
) -> Vec<u8> {
    let mut out = vec![0u8; (out_w as usize) * (out_h as usize) * 4];
    if out.is_empty() {
        return out;
    }

    let cur_hw = current_rect.half_w();
    let cur_hh = current_rect.half_h();
    let (sin_cur, cos_cur) = current_rect.angle.sin_cos();
    let (sin_orig, cos_orig) = original_rect.angle.sin_cos();

    let row_bytes = (out_w as usize) * 4;
    let num_threads = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(4)
        .min(out_h as usize)
        .max(1);
    let rows_per_band = (out_h as usize).div_ceil(num_threads);

    let chunks: Vec<&mut [u8]> = out.chunks_mut(rows_per_band * row_bytes).collect();

    std::thread::scope(|s| {
        let mut y_start = 0usize;
        for chunk in chunks {
            let band_y_start = y_start;
            let band_rows = chunk.len() / row_bytes;
            y_start += band_rows;

            s.spawn(move || {
                for local_y in 0..band_rows {
                    #[allow(clippy::cast_precision_loss)]
                    let py = (band_y_start + local_y) as f32 + 0.5;
                    let row_start = local_y * row_bytes;
                    for dst_x in 0..out_w {
                        #[allow(clippy::cast_precision_loss)]
                        let px = dst_x as f32 + 0.5;

                        let dx = px - current_rect.cx;
                        let dy = py - current_rect.cy;
                        let cur_lx = dx.mul_add(cos_cur, dy * sin_cur);
                        let cur_ly = (-dx).mul_add(sin_cur, dy * cos_cur);

                        if cur_lx.abs() > cur_hw || cur_ly.abs() > cur_hh {
                            continue;
                        }

                        let u = (cur_lx + cur_hw) / current_rect.w;
                        let v = (cur_ly + cur_hh) / current_rect.h;

                        let orig_lx = (u - 0.5) * original_rect.w;
                        let orig_ly = (v - 0.5) * original_rect.h;

                        let sample_x =
                            orig_lx.mul_add(cos_orig, -(orig_ly * sin_orig)) + original_rect.cx;
                        let sample_y =
                            orig_lx.mul_add(sin_orig, orig_ly * cos_orig) + original_rect.cy;

                        let pixel = match filter {
                            TransformFilter::NearestNeighbor => {
                                sample_nearest(src, src_w, src_h, sample_x, sample_y)
                            }
                            TransformFilter::Bilinear => {
                                sample_bilinear(src, src_w, src_h, sample_x, sample_y)
                            }
                        };
                        let idx = row_start + (dst_x as usize) * 4;
                        chunk[idx..idx + 4].copy_from_slice(&pixel);
                    }
                }
            });
        }
    });

    out
}
