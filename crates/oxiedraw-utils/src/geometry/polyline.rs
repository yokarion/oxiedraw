//! Arc-length helpers over polylines (sequences of [`Point`]s).

use super::Point;

pub fn bounding_box(points: &[Point]) -> (f32, f32, f32, f32) {
    let min_x = points.iter().map(|p| p.x).fold(f32::MAX, f32::min);
    let min_y = points.iter().map(|p| p.y).fold(f32::MAX, f32::min);
    let max_x = points.iter().map(|p| p.x).fold(f32::MIN, f32::max);
    let max_y = points.iter().map(|p| p.y).fold(f32::MIN, f32::max);
    (min_x, min_y, max_x, max_y)
}

pub fn arc_length(points: &[Point]) -> f32 {
    points.windows(2).map(|w| w[0].distance(w[1])).sum()
}

/// Re-place every point of `src` onto `dst`, preserving each point's
/// normalized arc-length position. Output has `src.len()` points, each
/// lying on `dst` at the same fraction of total length that the matching
/// `src` point sat at along `src`.
///
/// Used by shape correction to morph a freehand stroke onto a corrected
/// shape without disturbing the original sample distribution (so per-sample
/// pen dynamics and timing stay intact and the stroke renders smoothly).
///
/// Returns an empty `Vec` when either input is empty.
pub fn morph_path(src: &[Point], dst: &[Point]) -> Vec<Point> {
    if src.is_empty() || dst.is_empty() {
        return Vec::new();
    }
    if dst.len() == 1 {
        return vec![dst[0]; src.len()];
    }

    let mut dst_cum = vec![0.0_f32; dst.len()];
    for i in 1..dst.len() {
        dst_cum[i] = dst_cum[i - 1] + dst[i - 1].distance(dst[i]);
    }
    let dst_total = dst_cum[dst.len() - 1];
    let src_total = arc_length(src);
    if src_total < f32::EPSILON || dst_total < f32::EPSILON {
        return vec![dst[0]; src.len()];
    }

    let mut result = Vec::with_capacity(src.len());
    let mut src_acc = 0.0_f32;
    let mut seg = 0_usize;
    for i in 0..src.len() {
        if i > 0 {
            src_acc += src[i - 1].distance(src[i]);
        }
        let target = (src_acc / src_total).clamp(0.0, 1.0) * dst_total;
        while seg + 1 < dst.len() - 1 && dst_cum[seg + 1] <= target {
            seg += 1;
        }
        let seg_len = dst_cum[seg + 1] - dst_cum[seg];
        let t = if seg_len < f32::EPSILON {
            0.0
        } else {
            (target - dst_cum[seg]) / seg_len
        };
        result.push(dst[seg].lerp(dst[seg + 1], t));
    }
    result
}

/// Resample a polyline to exactly `n` evenly arc-length spaced points.
///
/// Returns an empty `Vec` when `n == 0` or `points` is empty.
pub fn resample(points: &[Point], n: usize) -> Vec<Point> {
    if n == 0 || points.is_empty() {
        return Vec::new();
    }
    if n == 1 || points.len() == 1 {
        return vec![points[0]; n];
    }
    let total = arc_length(points);
    if total < f32::EPSILON {
        return vec![points[0]; n];
    }

    // Build cumulative arc-length table.
    let mut cum = vec![0.0_f32; points.len()];
    for i in 1..points.len() {
        cum[i] = cum[i - 1] + points[i - 1].distance(points[i]);
    }

    #[allow(clippy::cast_precision_loss)]
    let step = total / (n - 1) as f32;
    let mut result = Vec::with_capacity(n);
    let mut seg = 0_usize;

    for i in 0..n {
        if i == n - 1 {
            result.push(*points.last().expect("non-empty"));
            break;
        }
        #[allow(clippy::cast_precision_loss)]
        let target = step * i as f32;
        while seg + 1 < points.len() - 1 && cum[seg + 1] <= target {
            seg += 1;
        }
        let seg_len = cum[seg + 1] - cum[seg];
        let t = if seg_len < f32::EPSILON {
            0.0
        } else {
            (target - cum[seg]) / seg_len
        };
        result.push(points[seg].lerp(points[seg + 1], t));
    }

    result
}
