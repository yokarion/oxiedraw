//! Turning generated geometry into an 8-bit coverage tile.
//!
//! Coverage only, never color: the canvas tints it at composite time as it does
//! a brush stroke. Elements union rather than accumulate, so a tuft of twenty
//! overlapping spikes reads as one silhouette rather than a stack.

use crate::geom::{CanvasElement, canvas_bounds};

/// Sub-scanlines per pixel row when filling a contour.
const FILL_SUBSAMPLES: usize = 4;

/// Coverage over a canvas-aligned rectangle, one byte per pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageTile {
    /// Top-left corner in canvas pixels.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// `width * height` coverage bytes, row-major.
    pub data: Vec<u8>,
}

impl CoverageTile {
    /// `0` outside the tile.
    #[must_use]
    pub fn coverage_at(&self, x: i32, y: i32) -> u8 {
        if x < self.x || y < self.y {
            return 0;
        }
        let (lx, ly) = ((x - self.x) as u32, (y - self.y) as u32);
        if lx >= self.width || ly >= self.height {
            return 0;
        }
        self.data[(ly * self.width + lx) as usize]
    }

    pub fn is_empty(&self) -> bool {
        self.width == 0 || self.height == 0
    }
}

/// Widest edge falloff worth rasterising, in pixels.
const MAX_EDGE: f32 = 24.0;

/// Stroke-level, not pattern-level: every generator gets the same edge
/// treatment, so the control belongs here rather than in each schema.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RasterOptions {
    /// `None` rasterises the full extent of the geometry.
    pub canvas: Option<(u32, u32)>,
    /// Width of the falloff at the silhouette edge, in pixels. Below 1.0 the
    /// edge goes crisp (0.0 is hard and aliased); above it the edge softens.
    pub edge: f32,
}

impl Default for RasterOptions {
    fn default() -> Self {
        Self {
            canvas: None,
            edge: 1.0,
        }
    }
}

/// Back ranks first. `None` when there is nothing to draw or the geometry falls
/// entirely outside the canvas.
#[must_use]
pub fn rasterize(elements: &[CanvasElement], options: &RasterOptions) -> Option<CoverageTile> {
    let edge = options.edge.clamp(0.0, MAX_EDGE);
    // A soft edge reaches half its width past the geometry.
    let margin = (edge * 0.5).ceil() as i32 + 1;
    let (min_x, min_y, max_x, max_y) = canvas_bounds(elements)?;
    let mut x0 = min_x.floor() as i32 - margin;
    let mut y0 = min_y.floor() as i32 - margin;
    let mut x1 = max_x.ceil() as i32 + margin;
    let mut y1 = max_y.ceil() as i32 + margin;
    if let Some((cw, ch)) = options.canvas {
        x0 = x0.max(0);
        y0 = y0.max(0);
        x1 = x1.min(cw as i32);
        y1 = y1.min(ch as i32);
    }
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    let width = (x1 - x0) as u32;
    let height = (y1 - y0) as u32;
    let tile = TileRect {
        x0,
        y0,
        width: width as usize,
        height: height as usize,
    };

    // Elements sharing a depth, kind and opacity are one surface and rasterise
    // together. Combining them afterwards leaves a hairline crack wherever two
    // anti-aliased edges abut, since two 50% edges max to 50% and never close.
    let mut order: Vec<usize> = (0..elements.len())
        .filter(|&i| !elements[i].verts.is_empty())
        .collect();
    order.sort_by(|&a, &b| {
        let (ea, eb) = (&elements[a], &elements[b]);
        ea.depth
            .cmp(&eb.depth)
            .then(ea.fill.cmp(&eb.fill))
            .then(ea.alpha.total_cmp(&eb.alpha))
    });
    let same_surface = |a: &CanvasElement, b: &CanvasElement| {
        a.depth == b.depth && a.fill == b.fill && a.alpha.to_bits() == b.alpha.to_bits()
    };

    let mut acc = vec![0.0_f32; tile.width * tile.height];
    // In one step: compositing the parts individually darkens every overlap.
    let mut rank = vec![0.0_f32; tile.width * tile.height];
    let mut rank_dirty: Option<TileRect> = None;
    let mut rank_depth: Option<i16> = None;
    let mut scratch: Vec<f32> = Vec::new();
    let mut group: Vec<&CanvasElement> = Vec::new();

    let mut start = 0;
    while start < order.len() {
        let first = &elements[order[start]];
        let mut end = start + 1;
        while end < order.len() && same_surface(first, &elements[order[end]]) {
            end += 1;
        }

        group.clear();
        let mut local: Option<TileRect> = None;
        for &i in &order[start..end] {
            let element = &elements[i];
            if let Some(rect) = element_rect(element, &tile, margin) {
                local = Some(local.map_or(rect, |current| union_rect(&current, &rect)));
                group.push(element);
            }
        }
        start = end;
        let (Some(local), false) = (local, group.is_empty()) else {
            continue;
        };

        if rank_depth.is_some_and(|d| d != first.depth)
            && let Some(dirty) = rank_dirty.take()
        {
            flush_rank(&mut acc, &mut rank, &tile, &dirty);
        }
        rank_depth = Some(first.depth);
        scratch.clear();
        scratch.resize(local.width * local.height, 0.0);
        if first.fill {
            fill_contours(&mut scratch, &local, &group, edge);
        } else {
            for element in &group {
                stroke_ribbon(&mut scratch, &local, element, edge);
            }
        }
        union_into(&mut rank, &tile, &scratch, &local, first.alpha);
        rank_dirty = Some(rank_dirty.map_or(local, |d| union_rect(&d, &local)));
    }
    if let Some(dirty) = rank_dirty {
        flush_rank(&mut acc, &mut rank, &tile, &dirty);
    }

    let mut data = Vec::with_capacity(acc.len());
    let mut any = false;
    for value in &acc {
        let byte = (value.clamp(0.0, 1.0) * 255.0).round() as u8;
        any |= byte > 0;
        data.push(byte);
    }
    if !any {
        return None;
    }
    Some(CoverageTile {
        x: x0,
        y: y0,
        width,
        height,
        data,
    })
}

#[derive(Debug, Clone, Copy)]
struct TileRect {
    x0: i32,
    y0: i32,
    width: usize,
    height: usize,
}

impl TileRect {
    const fn contains_row(&self, y: i32) -> bool {
        y >= self.y0 && y < self.y0 + self.height as i32
    }

    const fn index(&self, x: i32, y: i32) -> Option<usize> {
        if x < self.x0 || y < self.y0 {
            return None;
        }
        let (lx, ly) = ((x - self.x0) as usize, (y - self.y0) as usize);
        if lx >= self.width || ly >= self.height {
            return None;
        }
        Some(ly * self.width + lx)
    }
}

/// The part of `tile` one element can touch, including its edge falloff.
fn element_rect(element: &CanvasElement, tile: &TileRect, margin: i32) -> Option<TileRect> {
    let mut bounds: Option<(f32, f32, f32, f32)> = None;
    for v in &element.verts {
        let w = if element.fill { 0.0 } else { v.half_width };
        let b = (v.pos.x - w, v.pos.y - w, v.pos.x + w, v.pos.y + w);
        bounds = Some(bounds.map_or(b, |c| {
            (c.0.min(b.0), c.1.min(b.1), c.2.max(b.2), c.3.max(b.3))
        }));
    }
    let (min_x, min_y, max_x, max_y) = bounds?;
    let x0 = (min_x.floor() as i32 - margin).max(tile.x0);
    let y0 = (min_y.floor() as i32 - margin).max(tile.y0);
    let x1 = (max_x.ceil() as i32 + margin).min(tile.x0 + tile.width as i32);
    let y1 = (max_y.ceil() as i32 + margin).min(tile.y0 + tile.height as i32);
    if x1 <= x0 || y1 <= y0 {
        return None;
    }
    Some(TileRect {
        x0,
        y0,
        width: (x1 - x0) as usize,
        height: (y1 - y0) as usize,
    })
}

/// `edge` is the width of the falloff at the boundary, in pixels.
fn stroke_ribbon(out: &mut [f32], rect: &TileRect, element: &CanvasElement, edge: f32) {
    let reach = edge * 0.5;
    for pair in element.verts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let widest = a.half_width.max(b.half_width) + reach;
        let sx0 = ((a.pos.x.min(b.pos.x) - widest).floor() as i32 - 1).max(rect.x0);
        let sy0 = ((a.pos.y.min(b.pos.y) - widest).floor() as i32 - 1).max(rect.y0);
        let sx1 = ((a.pos.x.max(b.pos.x) + widest).ceil() as i32 + 1)
            .min(rect.x0 + rect.width as i32);
        let sy1 = ((a.pos.y.max(b.pos.y) + widest).ceil() as i32 + 1)
            .min(rect.y0 + rect.height as i32);
        let (dx, dy) = (b.pos.x - a.pos.x, b.pos.y - a.pos.y);
        let len_sq = dx.mul_add(dx, dy * dy);
        for py in sy0..sy1 {
            for px in sx0..sx1 {
                let (sample_x, sample_y) = (px as f32 + 0.5, py as f32 + 0.5);
                let (vx, vy) = (sample_x - a.pos.x, sample_y - a.pos.y);
                let t = if len_sq < f32::EPSILON {
                    0.0
                } else {
                    (vx.mul_add(dx, vy * dy) / len_sq).clamp(0.0, 1.0)
                };
                let dist = (vx - dx * t).hypot(vy - dy * t);
                let half_width = a.half_width + (b.half_width - a.half_width) * t;
                let coverage = if edge <= f32::EPSILON {
                    f32::from(dist <= half_width)
                } else {
                    ((half_width + reach - dist) / edge).clamp(0.0, 1.0)
                };
                if coverage <= 0.0 {
                    continue;
                }
                if let Some(i) = rect.index(px, py) {
                    out[i] = out[i].max(coverage);
                }
            }
        }
    }
}

/// One non-zero-winding path, then the edge treatment over the union boundary.
fn fill_contours(out: &mut [f32], rect: &TileRect, contours: &[&CanvasElement], edge: f32) {
    scanline_fill(out, rect, contours);
    if edge <= f32::EPSILON {
        for value in out.iter_mut() {
            *value = f32::from(*value >= 0.5);
        }
        return;
    }
    // Sub-scanline sampling already lands within a pixel of the true edge;
    // anything wider needs a real distance falloff.
    if edge > 1.0 {
        feather_boundary(out, rect, edge);
    }
}

/// Replace coverage near the boundary with a falloff `edge` pixels wide, centred
/// on it so softening does not move the silhouette. The boundary is traced back
/// out of the filled coverage rather than taken from the input contours; see
/// [`crate::contour`].
fn feather_boundary(out: &mut [f32], rect: &TileRect, edge: f32) {
    let reach = edge * 0.5;
    let boundary = {
        let filled = &*out;
        let stride = rect.width;
        crate::contour::trace_with(
            rect.width as i32,
            rect.height as i32,
            rect.x0 as f32,
            rect.y0 as f32,
            0.5,
            |x, y| filled[y as usize * stride + x as usize],
        )
    };
    let mut nearest = vec![f32::INFINITY; out.len()];
    for segment in &boundary {
        let (a, b) = (segment.a, segment.b);
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let len_sq = dx.mul_add(dx, dy * dy);
        let sx0 = ((a.x.min(b.x) - reach).floor() as i32 - 1).max(rect.x0);
        let sy0 = ((a.y.min(b.y) - reach).floor() as i32 - 1).max(rect.y0);
        let sx1 = ((a.x.max(b.x) + reach).ceil() as i32 + 1).min(rect.x0 + rect.width as i32);
        let sy1 = ((a.y.max(b.y) + reach).ceil() as i32 + 1).min(rect.y0 + rect.height as i32);
        for py in sy0..sy1 {
            for px in sx0..sx1 {
                let (sample_x, sample_y) = (px as f32 + 0.5, py as f32 + 0.5);
                let (vx, vy) = (sample_x - a.x, sample_y - a.y);
                let t = if len_sq < f32::EPSILON {
                    0.0
                } else {
                    (vx.mul_add(dx, vy * dy) / len_sq).clamp(0.0, 1.0)
                };
                let dist = (vx - dx * t).hypot(vy - dy * t);
                if dist > reach {
                    continue;
                }
                if let Some(i) = rect.index(px, py)
                    && dist < nearest[i]
                {
                    nearest[i] = dist;
                }
            }
        }
    }
    for (value, dist) in out.iter_mut().zip(nearest) {
        if !dist.is_finite() {
            continue;
        }
        // The scanline pass decides which side of the boundary we are on.
        let signed = if *value >= 0.5 { dist } else { -dist };
        *value = (0.5 + signed / edge).clamp(0.0, 1.0);
    }
}

/// Every contour at once, so overlapping shapes come out as their exact union
/// with no seam between them.
fn scanline_fill(out: &mut [f32], rect: &TileRect, contours: &[&CanvasElement]) {
    let share = 1.0 / FILL_SUBSAMPLES as f32;
    let total: usize = contours.iter().map(|c| c.verts.len()).sum();
    let mut crossings: Vec<(f32, i32)> = Vec::with_capacity(total);
    for py in rect.y0..rect.y0 + rect.height as i32 {
        for sub in 0..FILL_SUBSAMPLES {
            let y = py as f32 + (sub as f32 + 0.5) * share;
            crossings.clear();
            for contour in contours {
                let verts = &contour.verts;
                for i in 0..verts.len() {
                    let a = verts[i].pos;
                    let b = verts[(i + 1) % verts.len()].pos;
                    if (a.y <= y) == (b.y <= y) {
                        continue;
                    }
                    let span = b.y - a.y;
                    if span.abs() < f32::EPSILON {
                        continue;
                    }
                    let x = a.x + (y - a.y) / span * (b.x - a.x);
                    crossings.push((x, if span > 0.0 { 1 } else { -1 }));
                }
            }
            if crossings.len() < 2 {
                continue;
            }
            crossings.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut winding = 0;
            for pair in crossings.windows(2) {
                winding += pair[0].1;
                if winding == 0 {
                    continue;
                }
                add_span(out, rect, py, pair[0].0, pair[1].0, share);
            }
        }
    }
}

/// Weighted by how much of each pixel the span `[xs, xe)` actually crosses.
fn add_span(out: &mut [f32], rect: &TileRect, py: i32, xs: f32, xe: f32, share: f32) {
    if !rect.contains_row(py) || xe <= xs {
        return;
    }
    let first = (xs.floor() as i32).max(rect.x0);
    let last = (xe.ceil() as i32).min(rect.x0 + rect.width as i32);
    for px in first..last {
        let overlap = (xe.min(px as f32 + 1.0) - xs.max(px as f32)).clamp(0.0, 1.0);
        if overlap <= 0.0 {
            continue;
        }
        if let Some(i) = rect.index(px, py) {
            out[i] += share * overlap;
        }
    }
}

fn union_into(rank: &mut [f32], tile: &TileRect, scratch: &[f32], rect: &TileRect, alpha: f32) {
    for ly in 0..rect.height {
        for lx in 0..rect.width {
            let value = scratch[ly * rect.width + lx];
            if value <= 0.0 {
                continue;
            }
            let src = value.clamp(0.0, 1.0) * alpha;
            let Some(i) = tile.index(rect.x0 + lx as i32, rect.y0 + ly as i32) else {
                continue;
            };
            rank[i] = rank[i].max(src);
        }
    }
}

/// Touches only the region that rank actually wrote.
fn flush_rank(acc: &mut [f32], rank: &mut [f32], tile: &TileRect, dirty: &TileRect) {
    for ly in 0..dirty.height {
        for lx in 0..dirty.width {
            let Some(i) = tile.index(dirty.x0 + lx as i32, dirty.y0 + ly as i32) else {
                continue;
            };
            let src = rank[i];
            rank[i] = 0.0;
            if src <= 0.0 {
                continue;
            }
            acc[i] = src + acc[i] * (1.0 - src);
        }
    }
}

fn union_rect(a: &TileRect, b: &TileRect) -> TileRect {
    let x0 = a.x0.min(b.x0);
    let y0 = a.y0.min(b.y0);
    let x1 = (a.x0 + a.width as i32).max(b.x0 + b.width as i32);
    let y1 = (a.y0 + a.height as i32).max(b.y0 + b.height as i32);
    TileRect {
        x0,
        y0,
        width: (x1 - x0) as usize,
        height: (y1 - y0) as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::{CanvasElement, CanvasVertex};
    use oxiedraw_utils::geometry::Point;

    fn vertex(x: f32, y: f32, half_width: f32) -> CanvasVertex {
        CanvasVertex {
            pos: Point::new(x, y),
            half_width,
        }
    }

    fn ribbon(verts: Vec<CanvasVertex>, alpha: f32, depth: i16) -> CanvasElement {
        CanvasElement {
            verts,
            alpha,
            depth,
            fill: false,
            reverse: false,
        }
    }

    #[test]
    fn nothing_to_draw_yields_no_tile() {
        assert!(rasterize(&[], &RasterOptions::default()).is_none());
        let invisible = ribbon(vec![vertex(10.0, 10.0, 0.0), vertex(20.0, 10.0, 0.0)], 1.0, 0);
        assert!(rasterize(&[invisible], &RasterOptions::default()).is_none());
    }

    #[test]
    fn a_thick_stroke_is_solid_in_the_middle() {
        let element = ribbon(vec![vertex(20.0, 20.0, 5.0), vertex(60.0, 20.0, 5.0)], 1.0, 0);
        let tile = rasterize(&[element], &RasterOptions::default()).expect("tile");
        assert_eq!(tile.coverage_at(40, 20), 255, "centre must be solid");
        assert_eq!(tile.coverage_at(40, 18), 255);
        assert_eq!(tile.coverage_at(40, 40), 0, "far outside must be empty");
        assert_eq!(tile.coverage_at(40, 27), 0);
    }

    #[test]
    fn edges_are_anti_aliased() {
        let element = ribbon(vec![vertex(20.0, 20.5, 4.0), vertex(60.0, 20.5, 4.0)], 1.0, 0);
        let tile = rasterize(&[element], &RasterOptions::default()).expect("tile");
        let edge = tile.coverage_at(40, 24);
        assert!(
            (1..255).contains(&edge),
            "expected a partial edge pixel, got {edge}"
        );
    }

    // The edge control spans hard-and-aliased to a soft fade without moving
    // the silhouette itself.
    #[test]
    fn the_edge_control_sets_how_crisp_the_boundary_is() {
        let partials = |edge: f32| {
            let element = ribbon(vec![vertex(20.0, 20.0, 6.0), vertex(60.0, 20.0, 6.0)], 1.0, 0);
            let tile = rasterize(
                &[element],
                &RasterOptions {
                    edge,
                    ..RasterOptions::default()
                },
            )
            .expect("tile");
            (
                tile.data.iter().filter(|&&v| v > 0 && v < 255).count(),
                tile.coverage_at(40, 20),
            )
        };
        let (hard, hard_centre) = partials(0.0);
        let (crisp, crisp_centre) = partials(1.0);
        let (soft, soft_centre) = partials(6.0);
        assert_eq!(hard, 0, "edge 0 must be hard-edged");
        assert!(crisp > 0 && crisp < soft, "crisp {crisp}, soft {soft}");
        for centre in [hard_centre, crisp_centre, soft_centre] {
            assert_eq!(centre, 255, "the interior stays solid at every edge width");
        }
    }

    #[test]
    fn a_soft_edge_still_fits_inside_the_canvas() {
        let element = ribbon(vec![vertex(4.0, 4.0, 3.0), vertex(40.0, 4.0, 3.0)], 1.0, 0);
        let tile = rasterize(
            &[element],
            &RasterOptions {
                canvas: Some((64, 64)),
                edge: 12.0,
            },
        )
        .expect("tile");
        assert!(tile.x >= 0 && tile.y >= 0);
        assert!(i64::from(tile.x) + i64::from(tile.width) <= 64);
        assert!(i64::from(tile.y) + i64::from(tile.height) <= 64);
    }

    #[test]
    fn the_tile_hugs_the_geometry() {
        let element = ribbon(vec![vertex(100.0, 50.0, 3.0), vertex(140.0, 50.0, 3.0)], 1.0, 0);
        let tile = rasterize(&[element], &RasterOptions::default()).expect("tile");
        assert!(tile.x >= 94 && tile.x <= 97, "x {}", tile.x);
        assert!(tile.width <= 54, "width {}", tile.width);
        assert!(tile.height <= 12, "height {}", tile.height);
        assert_eq!(tile.data.len(), (tile.width * tile.height) as usize);
    }

    #[test]
    fn alpha_scales_coverage() {
        let element = ribbon(vec![vertex(20.0, 20.0, 5.0), vertex(60.0, 20.0, 5.0)], 0.5, 0);
        let tile = rasterize(&[element], &RasterOptions::default()).expect("tile");
        let value = tile.coverage_at(40, 20);
        assert!((126..=130).contains(&value), "coverage {value}");
    }

    // The whole stroke is a single flat color.
    #[test]
    fn overlapping_strokes_in_one_element_do_not_compound() {
        let zigzag = ribbon(
            vec![
                vertex(20.0, 20.0, 4.0),
                vertex(40.0, 20.0, 4.0),
                vertex(20.0, 20.0, 4.0),
            ],
            0.5,
            0,
        );
        let tile = rasterize(&[zigzag], &RasterOptions::default()).expect("tile");
        let value = tile.coverage_at(30, 20);
        assert!(
            (126..=130).contains(&value),
            "self-overlap compounded to {value}"
        );
    }

    // Two spikes of one tuft are the same surface, so no seam where they cross.
    #[test]
    fn elements_at_the_same_depth_union_instead_of_stacking() {
        let a = ribbon(vec![vertex(20.0, 20.0, 5.0), vertex(60.0, 20.0, 5.0)], 0.5, 0);
        let b = ribbon(vec![vertex(20.0, 20.0, 5.0), vertex(60.0, 20.0, 5.0)], 0.5, 0);
        let tile = rasterize(&[a, b], &RasterOptions::default()).expect("tile");
        let value = tile.coverage_at(40, 20);
        assert!(
            (126..=130).contains(&value),
            "same-depth overlap compounded to {value}"
        );
    }

    #[test]
    fn a_front_element_covers_a_dimmer_one_behind_it() {
        let back = ribbon(vec![vertex(20.0, 20.0, 5.0), vertex(60.0, 20.0, 5.0)], 0.5, 0);
        let front = ribbon(vec![vertex(20.0, 20.0, 5.0), vertex(60.0, 20.0, 5.0)], 1.0, 1);
        let tile = rasterize(&[back, front], &RasterOptions::default()).expect("tile");
        assert_eq!(tile.coverage_at(40, 20), 255);
    }

    #[test]
    fn depth_order_is_independent_of_emission_order() {
        let back = ribbon(vec![vertex(20.0, 20.0, 5.0), vertex(60.0, 20.0, 5.0)], 0.5, 0);
        let front = ribbon(vec![vertex(20.0, 20.0, 5.0), vertex(60.0, 20.0, 5.0)], 1.0, 1);
        let a = rasterize(&[back.clone(), front.clone()], &RasterOptions::default())
            .expect("tile");
        let b = rasterize(&[front, back], &RasterOptions::default()).expect("tile");
        assert_eq!(a.data, b.data);
    }

    #[test]
    fn a_filled_square_is_solid_inside_and_clean_outside() {
        let square = CanvasElement {
            verts: vec![
                vertex(20.0, 20.0, 0.0),
                vertex(60.0, 20.0, 0.0),
                vertex(60.0, 50.0, 0.0),
                vertex(20.0, 50.0, 0.0),
            ],
            alpha: 1.0,
            depth: 0,
            fill: true,
            reverse: false,
        };
        let tile = rasterize(&[square], &RasterOptions::default()).expect("tile");
        assert_eq!(tile.coverage_at(40, 35), 255, "inside");
        assert_eq!(tile.coverage_at(30, 25), 255, "inside");
        assert_eq!(tile.coverage_at(10, 35), 0, "left of the square");
        assert_eq!(tile.coverage_at(40, 60), 0, "below the square");
    }

    #[test]
    fn geometry_outside_the_canvas_is_clipped_away() {
        let element = ribbon(
            vec![vertex(-100.0, -100.0, 5.0), vertex(-60.0, -100.0, 5.0)],
            1.0,
            0,
        );
        let options = RasterOptions {
            canvas: Some((256, 256)),
            ..RasterOptions::default()
        };
        assert!(rasterize(&[element], &options).is_none());
    }

    #[test]
    fn geometry_crossing_the_canvas_edge_is_cropped_to_it() {
        let element = ribbon(vec![vertex(-20.0, 30.0, 6.0), vertex(40.0, 30.0, 6.0)], 1.0, 0);
        let options = RasterOptions {
            canvas: Some((256, 256)),
            ..RasterOptions::default()
        };
        let tile = rasterize(&[element], &options).expect("tile");
        assert!(tile.x >= 0 && tile.y >= 0, "tile starts off-canvas");
        assert!(i64::from(tile.x) + i64::from(tile.width) <= 256);
        assert_eq!(tile.coverage_at(0, 30), 255);
        assert_eq!(tile.coverage_at(30, 30), 255);
    }
}
