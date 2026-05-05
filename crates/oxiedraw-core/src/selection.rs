//! CPU-side selection helpers: shape rasterisation into an R8 buffer
//! and marching-squares contour extraction for marching ants.
//!
//! All rasterisers produce row-major `Vec<u8>` of length `w*h`, with
//! 0 = outside the shape and 255 = inside. Coordinates are in canvas
//! pixels with `(0,0)` at the top-left.

use oxiedraw_utils::geometry::Point;

/// Rectangle defined by its top-left corner and size in canvas pixels.
/// `w` / `h` may be negative - `normalize()` fixes that.
#[derive(Debug, Clone, Copy)]
pub struct RectShape {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectShape {
    #[must_use]
    pub fn normalize(self) -> Self {
        let (x, w) = if self.w >= 0.0 {
            (self.x, self.w)
        } else {
            (self.x + self.w, -self.w)
        };
        let (y, h) = if self.h >= 0.0 {
            (self.y, self.h)
        } else {
            (self.y + self.h, -self.h)
        };
        Self { x, y, w, h }
    }
}

/// The shape variant for a single `apply_selection_shape` call.
#[derive(Debug, Clone)]
pub enum SelectionShape {
    Rect(RectShape),
    Ellipse(RectShape),
    Polygon(Vec<Point>),
    /// Pre-rasterised R8 mask, `canvas_w`*`canvas_h` bytes. Used by the history
    /// system to restore a selection from a saved snapshot.
    Mask(Vec<u8>),
}

#[must_use]
pub fn rasterise(shape: &SelectionShape, canvas_w: u32, canvas_h: u32) -> Vec<u8> {
    match shape {
        SelectionShape::Rect(r) => rasterise_rect(r.normalize(), canvas_w, canvas_h),
        SelectionShape::Ellipse(r) => rasterise_ellipse(r.normalize(), canvas_w, canvas_h),
        SelectionShape::Polygon(pts) => rasterise_polygon(pts, canvas_w, canvas_h),
        SelectionShape::Mask(bytes) => {
            let need = (canvas_w as usize) * (canvas_h as usize);
            if bytes.len() == need {
                bytes.clone()
            } else {
                let mut buf = vec![0u8; need];
                let copy = bytes.len().min(need);
                buf[..copy].copy_from_slice(&bytes[..copy]);
                buf
            }
        }
    }
}

fn rasterise_rect(r: RectShape, w: u32, h: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (w as usize) * (h as usize)];
    if r.w <= 0.0 || r.h <= 0.0 {
        return buf;
    }
    let x0 = r.x.max(0.0).floor() as i32;
    let y0 = r.y.max(0.0).floor() as i32;
    #[allow(clippy::cast_possible_truncation)]
    let x1 = ((r.x + r.w).min(w as f32)).ceil() as i32;
    #[allow(clippy::cast_possible_truncation)]
    let y1 = ((r.y + r.h).min(h as f32)).ceil() as i32;
    for y in y0..y1 {
        if y < 0 || (y as u32) >= h {
            continue;
        }
        let row = (y as usize) * (w as usize);
        for x in x0..x1 {
            if x < 0 || (x as u32) >= w {
                continue;
            }
            buf[row + x as usize] = 255;
        }
    }
    buf
}

fn rasterise_ellipse(r: RectShape, w: u32, h: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (w as usize) * (h as usize)];
    if r.w <= 0.0 || r.h <= 0.0 {
        return buf;
    }
    let cx = r.w.mul_add(0.5, r.x);
    let cy = r.h.mul_add(0.5, r.y);
    let rx = r.w * 0.5;
    let ry = r.h * 0.5;
    if rx <= 0.0 || ry <= 0.0 {
        return buf;
    }
    let x0 = r.x.max(0.0).floor() as i32;
    let y0 = r.y.max(0.0).floor() as i32;
    #[allow(clippy::cast_possible_truncation)]
    let x1 = ((r.x + r.w).min(w as f32)).ceil() as i32;
    #[allow(clippy::cast_possible_truncation)]
    let y1 = ((r.y + r.h).min(h as f32)).ceil() as i32;
    for y in y0..y1 {
        if y < 0 || (y as u32) >= h {
            continue;
        }
        let row = (y as usize) * (w as usize);
        #[allow(clippy::cast_precision_loss)]
        let yf = (y as f32) + 0.5;
        let dy = (yf - cy) / ry;
        let dy2 = dy * dy;
        if dy2 > 1.0 {
            continue;
        }
        for x in x0..x1 {
            if x < 0 || (x as u32) >= w {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let xf = (x as f32) + 0.5;
            let dx = (xf - cx) / rx;
            if dx.mul_add(dx, dy2) <= 1.0 {
                buf[row + x as usize] = 255;
            }
        }
    }
    buf
}

fn rasterise_polygon(points: &[Point], w: u32, h: u32) -> Vec<u8> {
    let mut buf = vec![0u8; (w as usize) * (h as usize)];
    if points.len() < 3 {
        return buf;
    }
    let n = points.len();
    let mut crossings: Vec<f32> = Vec::with_capacity(8);
    for y in 0..h {
        crossings.clear();
        #[allow(clippy::cast_precision_loss)]
        let yf = (y as f32) + 0.5;
        for i in 0..n {
            let p0 = points[i];
            let p1 = points[(i + 1) % n];
            let (y0, y1) = (p0.y, p1.y);
            // Standard even-odd: count crossings, treating the segment as
            // half-open at the top to avoid double-counting at vertices.
            if (y0 <= yf && y1 > yf) || (y1 <= yf && y0 > yf) {
                let t = (yf - y0) / (y1 - y0);
                crossings.push((p1.x - p0.x).mul_add(t, p0.x));
            }
        }
        crossings.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut i = 0;
        let row = (y as usize) * (w as usize);
        while i + 1 < crossings.len() {
            #[allow(clippy::cast_possible_truncation)]
            let xa = crossings[i].max(0.0) as i32;
            #[allow(clippy::cast_possible_truncation)]
            let xb = crossings[i + 1].min(w as f32) as i32;
            for x in xa..xb {
                if x >= 0 && (x as u32) < w {
                    buf[row + x as usize] = 255;
                }
            }
            i += 2;
        }
    }
    buf
}

/// Trace the boundary of a binary R8 mask as axis-aligned pixel-edge
/// segments and stitch them into contour polylines. Each emitted segment
/// is exactly 1 pixel long along an integer grid line, so the resulting
/// contour follows every pixel transition exactly - no diagonal/45deg
/// smoothing as you would get from marching squares.
///
/// Coordinates are in buffer-pixel space at integer grid lines: corners
/// of the pixel grid go from (0, 0) at the top-left of pixel (0, 0) to
/// (w, h) at the bottom-right of pixel (w-1, h-1).
///
/// Threshold is 128 (matches the rest of the selection pipeline).
#[must_use]
pub fn pixel_perfect_contours(buf: &[u8], w: u32, h: u32) -> Vec<Vec<Point>> {
    use std::collections::HashMap;

    if w == 0 || h == 0 || buf.len() < (w as usize) * (h as usize) {
        return Vec::new();
    }
    let iso: u8 = 128;
    let sel = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || (x as u32) >= w || (y as u32) >= h {
            return false;
        }
        buf[(y as usize) * (w as usize) + x as usize] >= iso
    };

    // Each segment: (start, end). Direction matters for stitching so we
    // can build oriented chains. We walk clockwise around each selected
    // pixel: top edge L->R, right edge T->B, bottom edge R->L, left edge B->T.
    // Endpoints are i32 grid coords; use them as HashMap keys.
    let mut next_seg: HashMap<(i32, i32), (i32, i32)> = HashMap::new();
    for y in 0..(h as i32) {
        for x in 0..(w as i32) {
            if !sel(x, y) {
                continue;
            }
            // Top edge: neighbour above is empty -> emit (x, y) -> (x+1, y).
            if !sel(x, y - 1) {
                next_seg.insert((x, y), (x + 1, y));
            }
            // Right edge: neighbour right is empty -> emit (x+1, y) -> (x+1, y+1).
            if !sel(x + 1, y) {
                next_seg.insert((x + 1, y), (x + 1, y + 1));
            }
            // Bottom edge: neighbour below is empty -> emit (x+1, y+1) -> (x, y+1).
            if !sel(x, y + 1) {
                next_seg.insert((x + 1, y + 1), (x, y + 1));
            }
            // Left edge: neighbour left is empty -> emit (x, y+1) -> (x, y).
            if !sel(x - 1, y) {
                next_seg.insert((x, y + 1), (x, y));
            }
        }
    }

    // Walk closed loops by following next_seg pointers until we cycle back.
    let mut out: Vec<Vec<Point>> = Vec::new();
    while let Some((&start, _)) = next_seg.iter().next() {
        let mut chain: Vec<Point> = Vec::new();
        let mut cur = start;
        loop {
            #[allow(clippy::cast_precision_loss)]
            chain.push(Point::new(cur.0 as f32, cur.1 as f32));
            let Some(nxt) = next_seg.remove(&cur) else {
                break;
            };
            if nxt == start {
                #[allow(clippy::cast_precision_loss)]
                chain.push(Point::new(nxt.0 as f32, nxt.1 as f32));
                break;
            }
            cur = nxt;
        }
        // Collapse co-linear runs so the cairo stroker doesn't waste work
        // drawing each 1-pixel segment as a separate line_to.
        let simplified = collapse_collinear(&chain);
        if simplified.len() >= 2 {
            out.push(simplified);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A single isolated selected pixel should produce one 4-corner
    /// closed-loop contour following its pixel boundary.
    #[test]
    fn single_pixel_traces_unit_square() {
        let mut buf = vec![0u8; 9];
        buf[4] = 0xFF; // centre of 3x3 grid
        let contours = pixel_perfect_contours(&buf, 3, 3);
        assert_eq!(contours.len(), 1);
        let c = &contours[0];
        // After collapse_collinear there should be 4 corners + closing point.
        // The collapse skips co-linear middle vertices so a unit square keeps
        // its 4 corners; the trace closes back to the start.
        assert!(c.len() >= 4, "contour: {:?}", c);
        // All vertices must lie on integer pixel-grid lines.
        for p in c {
            assert!((p.x - p.x.round()).abs() < 1e-3);
            assert!((p.y - p.y.round()).abs() < 1e-3);
        }
    }

    /// A 4x4 fully-selected square should produce one rectangular contour
    /// with exactly 4 distinct corners (after collinear collapse).
    #[test]
    fn solid_block_collapses_to_rectangle() {
        let w = 6_u32;
        let h = 6_u32;
        let mut buf = vec![0u8; (w * h) as usize];
        for y in 1..5 {
            for x in 1..5 {
                buf[y * w as usize + x] = 0xFF;
            }
        }
        let contours = pixel_perfect_contours(&buf, w, h);
        assert_eq!(contours.len(), 1);
        let c = &contours[0];
        assert!(c.len() <= 5, "expected <=5 vertices after collapse, got {}: {:?}", c.len(), c);
    }
}

/// Drop intermediate vertices that lie on a straight horizontal or
/// vertical run between their neighbours. Pixel-perfect contours have
/// long horizontal/vertical runs which compress dramatically.
///
/// Also collapses the wrap-around vertex of closed loops: when the
/// trace happens to start mid-edge, the start/end pair would survive
/// even though they sit on a co-linear run with their neighbours. We
/// detect a closed loop (`first == last`), strip the closing duplicate,
/// drop the start vertex if it's redundant, then re-close.
fn collapse_collinear(chain: &[Point]) -> Vec<Point> {
    if chain.len() <= 2 {
        return chain.to_vec();
    }
    // First pass: drop interior co-linear vertices.
    let mut out = Vec::with_capacity(chain.len());
    out.push(chain[0]);
    for i in 1..chain.len() - 1 {
        let a = chain[i - 1];
        let b = chain[i];
        let c = chain[i + 1];
        if !collinear(a, b, c) {
            out.push(b);
        }
    }
    out.push(chain[chain.len() - 1]);

    // Second pass: closed-loop wraparound. If start == end and the
    // start vertex is on a straight run with `out[len-2] -> out[0] -> out[1]`,
    // drop it.
    if out.len() >= 4 {
        let first = out[0];
        let last = out[out.len() - 1];
        if (first.x - last.x).abs() < 0.01 && (first.y - last.y).abs() < 0.01 {
            let prev = out[out.len() - 2];
            let next = out[1];
            if collinear(prev, first, next) {
                out.remove(out.len() - 1); // strip duplicate closer
                out.remove(0); // strip redundant start
                out.push(out[0]); // re-close on the new start
            }
        }
    }
    out
}

#[inline]
fn collinear(a: Point, b: Point, c: Point) -> bool {
    let on_h = (a.y - b.y).abs() < 0.01 && (b.y - c.y).abs() < 0.01;
    let on_v = (a.x - b.x).abs() < 0.01 && (b.x - c.x).abs() < 0.01;
    on_h || on_v
}

/// Run marching squares on a binary-ish R8 buffer at iso=128 and return
/// the contour polylines (closed loops or open paths). Coordinates are
/// in *buffer* pixel space; the caller multiplies by the downsample
/// factor to get canvas-space coordinates.
///
/// Kept for non-pixel-perfect uses (e.g. feathered selections, where a
/// smoothed isocontour reads better than the stair-stepped pixel
/// boundary). Phase 1 ants use `pixel_perfect_contours` instead.
#[must_use]
pub fn marching_squares(buf: &[u8], w: u32, h: u32) -> Vec<Vec<Point>> {
    if w < 2 || h < 2 || buf.len() < (w as usize) * (h as usize) {
        return Vec::new();
    }
    let mut segments: Vec<(Point, Point)> = Vec::new();
    let iso: u8 = 128;
    let sample = |x: u32, y: u32| -> u8 { buf[(y as usize) * (w as usize) + x as usize] };

    for y in 0..(h - 1) {
        for x in 0..(w - 1) {
            let tl = sample(x, y);
            let tr = sample(x + 1, y);
            let br = sample(x + 1, y + 1);
            let bl = sample(x, y + 1);
            let mut idx = 0u8;
            if tl >= iso {
                idx |= 1;
            }
            if tr >= iso {
                idx |= 2;
            }
            if br >= iso {
                idx |= 4;
            }
            if bl >= iso {
                idx |= 8;
            }
            #[allow(clippy::cast_precision_loss)]
            let (fx, fy) = (x as f32, y as f32);
            let top = Point::new(fx + 0.5, fy);
            let right = Point::new(fx + 1.0, fy + 0.5);
            let bottom = Point::new(fx + 0.5, fy + 1.0);
            let left = Point::new(fx, fy + 0.5);
            match idx {
                1 | 14 => segments.push((left, top)),
                2 | 13 => segments.push((top, right)),
                3 | 12 => segments.push((left, right)),
                4 | 11 => segments.push((bottom, right)),
                5 => {
                    segments.push((left, top));
                    segments.push((bottom, right));
                }
                6 | 9 => segments.push((top, bottom)),
                7 | 8 => segments.push((left, bottom)),
                10 => {
                    segments.push((top, right));
                    segments.push((left, bottom));
                }
                _ => {}
            }
        }
    }

    stitch_segments(segments)
}

/// Greedy endpoint matching: walk segments, joining ones whose endpoints
/// are pixel-close. Quadratic in segment count but the downsampled mask
/// keeps that small in practice.
#[inline]
fn distance_sq(a: Point, b: Point) -> f32 {
    let dx = a.x - b.x;
    let dy = a.y - b.y;
    dx.mul_add(dx, dy * dy)
}

fn stitch_segments(mut segments: Vec<(Point, Point)>) -> Vec<Vec<Point>> {
    let mut out: Vec<Vec<Point>> = Vec::new();
    let eps2 = 0.01_f32;
    while let Some((a, b)) = segments.pop() {
        let mut chain: Vec<Point> = vec![a, b];
        loop {
            let last = *chain.last().expect("chain non-empty");
            let mut found = None;
            for (i, &(p, q)) in segments.iter().enumerate() {
                if distance_sq(last, p) < eps2 {
                    found = Some((i, q));
                    break;
                }
                if distance_sq(last, q) < eps2 {
                    found = Some((i, p));
                    break;
                }
            }
            match found {
                Some((i, next)) => {
                    segments.swap_remove(i);
                    chain.push(next);
                }
                None => break,
            }
        }
        loop {
            let first = chain[0];
            let mut found = None;
            for (i, &(p, q)) in segments.iter().enumerate() {
                if distance_sq(first, p) < eps2 {
                    found = Some((i, q));
                    break;
                }
                if distance_sq(first, q) < eps2 {
                    found = Some((i, p));
                    break;
                }
            }
            match found {
                Some((i, prev)) => {
                    segments.swap_remove(i);
                    chain.insert(0, prev);
                }
                None => break,
            }
        }
        if chain.len() >= 2 {
            out.push(chain);
        }
    }
    out
}
