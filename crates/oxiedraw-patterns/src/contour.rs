//! Finding the boundary of a rasterised silhouette, with marching squares.
//!
//! The soft-edge pass needs the boundary of the union of the shapes it filled,
//! which only exists once they are rasterised: feathering against the input
//! contours would carve a soft line through the middle of the mass wherever two
//! overlap. Segments come back loose, not stitched; nothing needs them ordered.

use oxiedraw_utils::geometry::Point;

use crate::raster::CoverageTile;

/// Coverage at or above this counts as inside the silhouette.
pub const INSIDE: u8 = 128;

/// One piece of the traced boundary, in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Segment {
    pub a: Point,
    pub b: Point,
}

impl Segment {
    #[must_use]
    pub fn midpoint(&self) -> Point {
        Point::new((self.a.x + self.b.x) * 0.5, (self.a.y + self.b.y) * 0.5)
    }

    #[must_use]
    pub fn length(&self) -> f32 {
        self.a.distance(self.b)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edge {
    Top,
    Right,
    Bottom,
    Left,
}

/// Coverage outside the tile counts as empty, so a shape touching the tile edge
/// still closes.
#[must_use]
pub fn trace(tile: &CoverageTile, level: u8) -> Vec<Segment> {
    if tile.is_empty() {
        return Vec::new();
    }
    let (w, h) = (tile.width as i32, tile.height as i32);
    trace_with(w, h, tile.x as f32, tile.y as f32, f32::from(level), |x, y| {
        f32::from(tile.data[(y * w + x) as usize])
    })
}

/// [`trace`] over any grid. `at` is only called for in-bounds coordinates;
/// everything outside counts as empty. `(ox, oy)` is the canvas position of
/// grid cell `(0, 0)`, whose sample sits at its centre.
pub(crate) fn trace_with(
    width: i32,
    height: i32,
    ox: f32,
    oy: f32,
    level: f32,
    at_in_bounds: impl Fn(i32, i32) -> f32,
) -> Vec<Segment> {
    let (w, h) = (width, height);
    if w <= 0 || h <= 0 {
        return Vec::new();
    }
    let at = |x: i32, y: i32| -> f32 {
        if x < 0 || y < 0 || x >= w || y >= h {
            return 0.0;
        }
        at_in_bounds(x, y)
    };
    // Grid coordinate (x, y) is the centre of pixel (x, y).
    let to_canvas = |gx: f32, gy: f32| Point::new(ox + gx + 0.5, oy + gy + 0.5);

    let mut out = Vec::new();
    for y in -1..h {
        for x in -1..w {
            let tl = at(x, y);
            let tr = at(x + 1, y);
            let br = at(x + 1, y + 1);
            let bl = at(x, y + 1);
            let index = usize::from(tl >= level) << 3
                | usize::from(tr >= level) << 2
                | usize::from(br >= level) << 1
                | usize::from(bl >= level);
            if index == 0 || index == 15 {
                continue;
            }
            // Where the boundary cuts each side of the cell.
            let cut = |edge: Edge| -> Point {
                let lerp = |a: f32, b: f32| {
                    let span = b - a;
                    if span.abs() < f32::EPSILON {
                        0.5
                    } else {
                        ((level - a) / span).clamp(0.0, 1.0)
                    }
                };
                let (fx, fy) = (x as f32, y as f32);
                let (gx, gy) = match edge {
                    Edge::Top => (fx + lerp(tl, tr), fy),
                    Edge::Right => (fx + 1.0, fy + lerp(tr, br)),
                    Edge::Bottom => (fx + lerp(bl, br), fy + 1.0),
                    Edge::Left => (fx, fy + lerp(tl, bl)),
                };
                to_canvas(gx, gy)
            };

            for &(from, to) in edges_for(index) {
                let (a, b) = (cut(from), cut(to));
                if a.distance(b) > f32::EPSILON {
                    out.push(Segment { a, b });
                }
            }
        }
    }
    out
}

/// Bits are `tl tr br bl`, high to low. The saddle cases (5 and 10) resolve the
/// same way every time, which keeps the trace deterministic.
fn edges_for(index: usize) -> &'static [(Edge, Edge)] {
    use Edge::{Bottom, Left, Right, Top};
    match index {
        1 | 14 => &[(Left, Bottom)],
        2 | 13 => &[(Bottom, Right)],
        3 | 12 => &[(Left, Right)],
        4 | 11 => &[(Top, Right)],
        5 => &[(Left, Top), (Bottom, Right)],
        6 | 9 => &[(Top, Bottom)],
        7 | 8 => &[(Left, Top)],
        10 => &[(Left, Bottom), (Top, Right)],
        _ => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(width: u32, height: u32, rect: (u32, u32, u32, u32)) -> CoverageTile {
        let mut data = vec![0_u8; (width * height) as usize];
        let (x0, y0, x1, y1) = rect;
        for y in y0..y1 {
            for x in x0..x1 {
                data[(y * width + x) as usize] = 255;
            }
        }
        CoverageTile {
            x: 0,
            y: 0,
            width,
            height,
            data,
        }
    }

    #[test]
    fn an_empty_tile_traces_nothing() {
        let tile = block(8, 8, (0, 0, 0, 0));
        assert!(trace(&tile, INSIDE).is_empty());
    }

    #[test]
    fn a_square_traces_its_four_sides() {
        let tile = block(20, 20, (5, 5, 15, 15));
        let segments = trace(&tile, INSIDE);
        assert!(!segments.is_empty());
        let perimeter: f32 = segments.iter().map(Segment::length).sum();
        // Ten pixels a side, traced through pixel centres.
        assert!(
            (perimeter - 36.0).abs() < 4.0,
            "perimeter {perimeter}, expected about 36"
        );
        for segment in &segments {
            let m = segment.midpoint();
            assert!(
                (4.0..=15.0).contains(&m.x) && (4.0..=15.0).contains(&m.y),
                "segment outside the square: {m:?}"
            );
        }
    }

    #[test]
    fn the_boundary_runs_between_inside_and_outside() {
        let tile = block(20, 20, (5, 5, 15, 15));
        for segment in trace(&tile, INSIDE) {
            let m = segment.midpoint();
            // Every boundary point sits within a pixel of the 5..15 edges.
            let on_edge = [4.5_f32, 14.5]
                .iter()
                .any(|e| (m.x - e).abs() < 1.01 || (m.y - e).abs() < 1.01);
            assert!(on_edge, "stray segment at {m:?}");
        }
    }

    #[test]
    fn a_shape_touching_the_tile_edge_still_closes() {
        let tile = block(20, 20, (0, 0, 10, 10));
        let segments = trace(&tile, INSIDE);
        assert!(!segments.is_empty());
        let touches_left = segments.iter().any(|s| s.midpoint().x < 0.5);
        let touches_top = segments.iter().any(|s| s.midpoint().y < 0.5);
        assert!(touches_left && touches_top, "the shape did not close");
    }

    #[test]
    fn tile_offset_lands_in_canvas_coordinates() {
        let mut tile = block(20, 20, (5, 5, 15, 15));
        tile.x = 100;
        tile.y = 40;
        let segments = trace(&tile, INSIDE);
        for segment in &segments {
            let m = segment.midpoint();
            assert!((104.0..=115.0).contains(&m.x), "x {}", m.x);
            assert!((44.0..=55.0).contains(&m.y), "y {}", m.y);
        }
    }
}
