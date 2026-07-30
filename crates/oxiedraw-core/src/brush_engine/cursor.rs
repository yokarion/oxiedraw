//! Brush footprint cursor.
//!
//! Computes the outline of the area the active brush would touch at the current
//! input state, for the UI's custom cursor. It reuses the dab construction from
//! `stamp::PresetStrokeRenderer` so the cursor mirrors what gets painted:
//! ellipse for `SoftRound`, circle for `Pixel`, and a marching-squares contour
//! of the alpha mask (threshold 0.5) for `Textured`. With a `scatter` mapping
//! active the outline is dilated by the maximum scatter distance so it shows
//! the envelope of possible dab placements, not a single dot.

use std::f32::consts::TAU;

use oxiedraw_utils::geometry::Point;

use super::dynamics::{SpawnInput, evaluate};
use super::pattern::PatternData;
use super::{BrushFamily, BrushPreset, Dab, StrokeContext, TipShape};

/// Lower bound on dab radius. Mirrors `stamp::MIN_DAB_RADIUS` - kept in
/// sync by convention; both are 0.5 px because below that the renderer
/// can't produce a visible mark.
const MIN_DAB_RADIUS: f32 = 0.5;

/// Number of segments used to sample an ellipse outline. 64 is the
/// trade-off between smoothness and per-frame cost - the cursor is
/// re-emitted on every pointer move.
const ELLIPSE_SEGMENTS: usize = 64;

/// Alpha threshold used to extract a textured brush's footprint.
const MASK_THRESHOLD: f32 = 0.5;

/// Outline geometry of a single brush dab, ready for the UI to stroke
/// in widget space. Coordinates are canvas pixels relative to the
/// caller-supplied cursor anchor - the UI adds the world position
/// during draw. `strokes` may contain multiple disjoint polylines:
/// `Textured` patterns with internal holes produce one polyline per
/// contour component.
#[derive(Debug, Clone, PartialEq)]
pub struct BrushCursor {
    pub strokes: Vec<Vec<Point>>,
}

impl BrushCursor {
    pub fn is_empty(&self) -> bool {
        self.strokes.iter().all(|s| s.len() < 2)
    }
}

/// Build the cursor footprint for `preset` at `ctx`, evaluating
/// dynamics against `input`. The output is centred at `(0, 0)`.
///
/// `base_size_for_dynamics` is the value passed to the dynamics
/// evaluator as the base diameter - almost always `ctx.size`, but
/// kept explicit so tests can pin it independent of the stroke
/// context.
pub fn compute_brush_cursor(
    preset: &BrushPreset,
    ctx: StrokeContext,
    input: SpawnInput,
    base_size_for_dynamics: f32,
) -> BrushCursor {
    let scatter_max = preset
        .dynamics
        .scatter
        .as_ref()
        .map_or(0.0, |m| m.range.0.abs().max(m.range.1.abs()));

    let dab = build_preview_dab(preset, ctx, input, base_size_for_dynamics);

    let strokes = match &preset.family {
        BrushFamily::SoftRound => {
            let outline = ellipse_outline(&dab);
            vec![expand_outward(&outline, dab.center, scatter_max)]
        }
        BrushFamily::Pixel => {
            // The pixel shader is `step(d, radius)` on a pixel-snapped
            // centre - render the same boundary, snapped to the integer
            // pixel grid so it reads as "pixel art" even at large sizes.
            let outline = ellipse_outline(&dab);
            let expanded = expand_outward(&outline, dab.center, scatter_max);
            vec![snap_to_pixel_grid(&expanded)]
        }
        BrushFamily::Textured(pattern) => {
            // Global-grain brushes (texture_scale > 0) paint a procedural
            // tip modulated by a canvas-anchored pattern, so the footprint
            // is the tip - not the pattern's alpha contour. Tracing the
            // (512px, noisy) grain per pointer-move would peg the CPU, and
            // the grain isn't the outline anyway. Legacy stamped-pattern
            // brushes (scale 0) still trace the mask.
            if preset.texture_scale > 0.0 {
                let outline = match preset.tip {
                    TipShape::Round => ellipse_outline(&dab),
                    TipShape::Square => rect_outline(&dab),
                };
                vec![expand_outward(&outline, dab.center, scatter_max)]
            } else {
                textured_outline(pattern, &dab, scatter_max)
            }
        }
        BrushFamily::ImageTip { .. } => {
            // The stamped tip fills the dab quad; approximate its footprint
            // by the dab's bounding ellipse rather than tracing the tip mask.
            let outline = ellipse_outline(&dab);
            vec![expand_outward(&outline, dab.center, scatter_max)]
        }
        BrushFamily::Smudge => {
            // Round tip shaped by hardness - same footprint as soft round.
            let outline = ellipse_outline(&dab);
            vec![expand_outward(&outline, dab.center, scatter_max)]
        }
    };

    BrushCursor { strokes }
}

/// Construct the dab the engine would emit for a single sample,
/// *excluding* the scatter offset - the cursor draws an envelope, so
/// the scatter contribution is folded into the outline radius later,
/// not into the centre position.
fn build_preview_dab(
    preset: &BrushPreset,
    ctx: StrokeContext,
    input: SpawnInput,
    base_size: f32,
) -> Dab {
    let mut dab = Dab::round(Point::ZERO, ctx.size * 0.5, ctx.color);
    if matches!(preset.family, BrushFamily::Pixel) || !preset.dynamics.any_active() {
        return dab;
    }
    // Run dynamics with scatter stripped out so `dab.center` stays at
    // the anchor; envelope dilation below uses `scatter_max` directly.
    let mut dynamics = preset.dynamics.clone();
    dynamics.scatter = None;
    let scatter_seed = (input.random, (input.random + 0.5).fract());
    evaluate(&dynamics, &input, base_size, scatter_seed, &mut dab);
    dab.radius = dab.radius.max(MIN_DAB_RADIUS);
    dab
}

fn ellipse_outline(dab: &Dab) -> Vec<Point> {
    let rx = dab.radius.max(MIN_DAB_RADIUS);
    let ry = (dab.radius * dab.aspect).max(MIN_DAB_RADIUS);
    let (sin_r, cos_r) = dab.rotation.sin_cos();
    let mut points = Vec::with_capacity(ELLIPSE_SEGMENTS + 1);
    for i in 0..=ELLIPSE_SEGMENTS {
        #[allow(clippy::cast_precision_loss)]
        let t = (i as f32) / (ELLIPSE_SEGMENTS as f32) * TAU;
        let (sin_t, cos_t) = t.sin_cos();
        let lx = rx * cos_t;
        let ly = ry * sin_t;
        let x = lx.mul_add(cos_r, -(ly * sin_r)) + dab.center.x;
        let y = lx.mul_add(sin_r, ly * cos_r) + dab.center.y;
        points.push(Point::new(x, y));
    }
    points
}

/// Outline of a square tip: a rotated rectangle with half-extents
/// `radius` x `radius * aspect`, matching the chebyshev footprint the
/// textured shader paints for `TipShape::Square`.
fn rect_outline(dab: &Dab) -> Vec<Point> {
    let rx = dab.radius.max(MIN_DAB_RADIUS);
    let ry = (dab.radius * dab.aspect).max(MIN_DAB_RADIUS);
    let (sin_r, cos_r) = dab.rotation.sin_cos();
    let corners = [(-rx, -ry), (rx, -ry), (rx, ry), (-rx, ry), (-rx, -ry)];
    corners
        .iter()
        .map(|&(lx, ly)| {
            let x = lx.mul_add(cos_r, -(ly * sin_r)) + dab.center.x;
            let y = lx.mul_add(sin_r, ly * cos_r) + dab.center.y;
            Point::new(x, y)
        })
        .collect()
}

/// Dilate `outline` outward along the centroid-radial direction by
/// `expand` canvas pixels. A no-op when `expand <= 0`. Used to render
/// the scatter envelope - the true envelope of `radius + scatter` is a
/// rounded rectangle (scatter is uniform per axis), but radial dilation
/// produces a visually clean ellipse that overapproximates the
/// rendered area by at most ~20% on the diagonals.
fn expand_outward(outline: &[Point], center: Point, expand: f32) -> Vec<Point> {
    if expand <= 0.0 {
        return outline.to_vec();
    }
    outline.iter().map(|p| dilate_point(*p, center, expand)).collect()
}

/// Round each outline vertex to the integer pixel grid. Used by the
/// Pixel family so the cursor reads at the same resolution as the
/// rasterised dab - consecutive duplicates after snapping are dropped
/// so cairo doesn't emit zero-length sub-strokes.
fn snap_to_pixel_grid(outline: &[Point]) -> Vec<Point> {
    let mut out = Vec::with_capacity(outline.len());
    for p in outline {
        let snapped = Point::new(p.x.round(), p.y.round());
        if out.last() != Some(&snapped) {
            out.push(snapped);
        }
    }
    if out.len() >= 2 && out.first() != out.last()
        && let Some(&first) = out.first() {
            out.push(first);
        }
    out
}

fn dilate_point(p: Point, center: Point, expand: f32) -> Point {
    let dx = p.x - center.x;
    let dy = p.y - center.y;
    let d = dx.hypot(dy);
    if d < 1e-6 {
        return p;
    }
    let nx = dx / d;
    let ny = dy / d;
    Point::new(nx.mul_add(expand, p.x), ny.mul_add(expand, p.y))
}

/// Trace the alpha-0.5 contour of `pattern` via marching squares,
/// scale it to the dab's diameter, rotate by `dab.rotation`, translate
/// to `dab.center`, then dilate by `scatter_max`. Returns a list of
/// short polylines - cairo strokes them as separate segments; visually
/// they form a continuous contour.
fn textured_outline(
    pattern: &PatternData,
    dab: &Dab,
    scatter_max: f32,
) -> Vec<Vec<Point>> {
    let segments = marching_squares_pattern(pattern, MASK_THRESHOLD);
    if segments.is_empty() {
        return Vec::new();
    }

    // Pattern is `w x h` premultiplied RGBA. Scale so the pattern's
    // full extent maps to the dab's bounding box.
    #[allow(clippy::cast_precision_loss)]
    let pw = pattern.width as f32;
    #[allow(clippy::cast_precision_loss)]
    let ph = pattern.height as f32;
    let diameter = (dab.radius * 2.0).max(MIN_DAB_RADIUS * 2.0);
    let scale_x = diameter / pw;
    let scale_y = (diameter * dab.aspect) / ph;
    let cx_p = pw * 0.5;
    let cy_p = ph * 0.5;
    let (sin_r, cos_r) = dab.rotation.sin_cos();

    let transform = |pp: Point| -> Point {
        let lx = (pp.x - cx_p) * scale_x;
        let ly = (pp.y - cy_p) * scale_y;
        let x = lx.mul_add(cos_r, -(ly * sin_r)) + dab.center.x;
        let y = lx.mul_add(sin_r, ly * cos_r) + dab.center.y;
        Point::new(x, y)
    };

    segments
        .into_iter()
        .map(|(a, b)| {
            let mut ta = transform(a);
            let mut tb = transform(b);
            if scatter_max > 0.0 {
                ta = dilate_point(ta, dab.center, scatter_max);
                tb = dilate_point(tb, dab.center, scatter_max);
            }
            vec![ta, tb]
        })
        .collect()
}

/// Marching squares on a premultiplied-RGBA pattern, returning a list
/// of (start, end) line segments in pattern-pixel coordinates. The
/// contour threshold is `MASK_THRESHOLD`. Segments are not stitched -
/// callers either stroke them independently or stitch via endpoint
/// matching as needed.
fn marching_squares_pattern(pattern: &PatternData, threshold: f32) -> Vec<(Point, Point)> {
    let w = pattern.width as usize;
    let h = pattern.height as usize;
    if w < 2 || h < 2 {
        return Vec::new();
    }
    #[allow(clippy::cast_precision_loss)]
    let alpha_at = |x: usize, y: usize| -> f32 {
        let i = (y * w + x) * 4 + 3;
        f32::from(pattern.rgba[i]) / 255.0
    };
    let mut segments: Vec<(Point, Point)> = Vec::new();
    let interp = |a: f32, b: f32| -> f32 {
        // Guard against the rare a == b case; the contour passes
        // between them, snap to midpoint.
        let denom = b - a;
        if denom.abs() < 1e-6 {
            0.5
        } else {
            ((threshold - a) / denom).clamp(0.0, 1.0)
        }
    };
    for y in 0..h - 1 {
        for x in 0..w - 1 {
            let a00 = alpha_at(x, y);
            let a10 = alpha_at(x + 1, y);
            let a11 = alpha_at(x + 1, y + 1);
            let a01 = alpha_at(x, y + 1);
            let mut code = 0u8;
            if a00 >= threshold {
                code |= 1;
            }
            if a10 >= threshold {
                code |= 2;
            }
            if a11 >= threshold {
                code |= 4;
            }
            if a01 >= threshold {
                code |= 8;
            }
            if code == 0 || code == 15 {
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            let fx = x as f32;
            #[allow(clippy::cast_precision_loss)]
            let fy = y as f32;
            let top = Point::new(fx + interp(a00, a10), fy);
            let bot = Point::new(fx + interp(a01, a11), fy + 1.0);
            let left = Point::new(fx, fy + interp(a00, a01));
            let right = Point::new(fx + 1.0, fy + interp(a10, a11));
            match code {
                1 | 14 => segments.push((left, top)),
                2 | 13 => segments.push((top, right)),
                4 | 11 => segments.push((right, bot)),
                8 | 7 => segments.push((bot, left)),
                3 | 12 => segments.push((left, right)),
                6 | 9 => segments.push((top, bot)),
                5 => {
                    segments.push((left, top));
                    segments.push((right, bot));
                }
                10 => {
                    segments.push((top, right));
                    segments.push((bot, left));
                }
                _ => {}
            }
        }
    }
    segments
}

#[cfg(test)]
// Exact compares are deliberate: fract/signum and round-tripped literals are
// exact by construction. Approximate checks nearby use an epsilon.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::brush_engine::dynamics::{Curve, DynSource, Mapping};
    use crate::brush_engine::{BrushPresetId, Dynamics};
    use crate::color::Color;
    use std::rc::Rc;

    fn baseline_input() -> SpawnInput {
        SpawnInput {
            pressure: 1.0,
            speed: 0.0,
            direction: 0.0,
            distance: 0.0,
            random: 0.5,
            pen_rotation: 0.0,
            angle: 0.0,
        }
    }

    fn baseline_ctx() -> StrokeContext {
        StrokeContext {
            preset: BrushPresetId(0),
            color: Color::BLACK,
            size: 20.0,
            opacity: 1.0,
        }
    }

    fn default_round_preset() -> BrushPreset {
        BrushPreset::default_round(BrushPresetId(0))
    }

    /// 16x4 mask: solid filled horizontal stripe two rows thick. After
    /// stretching to fit the dab's square quad (the renderer's UV
    /// mapping), the footprint comes out ~15x10 - distinctly
    /// non-circular and visibly rotation-sensitive.
    fn rect_pattern() -> Rc<PatternData> {
        let w = 16u32;
        let h = 4u32;
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 1..h - 1 {
            for x in 2..w - 2 {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = 255;
                rgba[i + 1] = 255;
                rgba[i + 2] = 255;
                rgba[i + 3] = 255;
            }
        }
        Rc::new(PatternData::new(rgba, w, h))
    }

    fn textured_preset() -> BrushPreset {
        BrushPreset {
            id: BrushPresetId(0),
            name: "Test Texture".into(),
            family: BrushFamily::Textured(rect_pattern()),
            default_size: 20.0,
            default_opacity: 1.0,
            spacing_ratio: 0.1,
            stabilizer: 0.0,
            speed_smoothing: 0.0,
            buildup: false,
            hardness: 1.0,
            tip: crate::brush_engine::TipShape::Round,
            texture_scale: 0.0,
            texture_strength: 0.0,
            texturing_mode: crate::brush_engine::TexturingMode::Multiply,
            dynamics: Dynamics::default(),
            icon: None,
            preview: None,
            source_path: None,
        }
    }

    fn signature(c: &BrushCursor) -> Vec<(i32, i32)> {
        c.strokes
            .iter()
            .flat_map(|s| s.iter())
            .map(|p| ((p.x * 1000.0) as i32, (p.y * 1000.0) as i32))
            .collect()
    }

    fn bounds(c: &BrushCursor) -> (f32, f32, f32, f32) {
        let mut min_x = f32::INFINITY;
        let mut max_x = f32::NEG_INFINITY;
        let mut min_y = f32::INFINITY;
        let mut max_y = f32::NEG_INFINITY;
        for s in &c.strokes {
            for p in s {
                min_x = min_x.min(p.x);
                max_x = max_x.max(p.x);
                min_y = min_y.min(p.y);
                max_y = max_y.max(p.y);
            }
        }
        (min_x, max_x, min_y, max_y)
    }

    #[test]
    fn default_round_outline_is_a_circle() {
        let preset = default_round_preset();
        let c = compute_brush_cursor(&preset, baseline_ctx(), baseline_input(), 20.0);
        let (min_x, max_x, min_y, max_y) = bounds(&c);
        let width = max_x - min_x;
        let height = max_y - min_y;
        assert!((width - height).abs() < 0.01, "round outline must be square in bbox");
        assert!((width - 20.0).abs() < 0.05, "diameter ~= size");
    }

    #[test]
    fn size_changes_outline_radius() {
        let preset = default_round_preset();
        let small = compute_brush_cursor(
            &preset,
            StrokeContext { size: 10.0, ..baseline_ctx() },
            baseline_input(),
            10.0,
        );
        let large = compute_brush_cursor(
            &preset,
            StrokeContext { size: 40.0, ..baseline_ctx() },
            baseline_input(),
            40.0,
        );
        let (smin, smax, _, _) = bounds(&small);
        let (lmin, lmax, _, _) = bounds(&large);
        assert!((lmax - lmin) > (smax - smin) * 3.5, "4x size -> >= 3.5x diameter");
    }

    #[test]
    fn pressure_input_drives_size_dynamics() {
        let preset = default_round_preset();
        let light = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { pressure: 0.1, ..baseline_input() },
            20.0,
        );
        let heavy = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { pressure: 1.0, ..baseline_input() },
            20.0,
        );
        let (lmin, lmax, _, _) = bounds(&light);
        let (hmin, hmax, _, _) = bounds(&heavy);
        assert!(hmax - hmin > lmax - lmin, "heavier pressure -> larger outline");
    }

    #[test]
    fn speed_input_drives_size_dynamics_on_speed_brush() {
        let preset = BrushPreset::speed_brush(BrushPresetId(0));
        let slow = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { speed: 0.0, ..baseline_input() },
            20.0,
        );
        let fast = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { speed: 1.0, ..baseline_input() },
            20.0,
        );
        let (smin, smax, _, _) = bounds(&slow);
        let (fmin, fmax, _, _) = bounds(&fast);
        // speed_brush range: slow=1.0xbase, fast=0.15xbase.
        assert!(fmax - fmin < (smax - smin) * 0.5, "faster speed -> smaller outline");
    }

    #[test]
    fn rotation_dynamics_rotates_textured_mask() {
        let mut preset = textured_preset();
        preset.dynamics.rotation = Some(Mapping {
            source: DynSource::FakePenRotation,
            curve: Curve::linear(),
            range: (0.0, std::f32::consts::FRAC_PI_2),
            invert: false,
        });
        let upright = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { direction: 0.0, ..baseline_input() },
            20.0,
        );
        let rotated = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { direction: 1.0, ..baseline_input() },
            20.0,
        );
        // Non-symmetric mask: rotating 90deg must swap bbox width/height.
        let (u_xmin, u_xmax, u_ymin, u_ymax) = bounds(&upright);
        let (r_xmin, r_xmax, r_ymin, r_ymax) = bounds(&rotated);
        let u_w = u_xmax - u_xmin;
        let u_h = u_ymax - u_ymin;
        let r_w = r_xmax - r_xmin;
        let r_h = r_ymax - r_ymin;
        assert!(u_w > u_h * 1.3, "upright mask is wider than tall (got {u_w}x{u_h})");
        assert!(r_h > r_w * 1.3, "90deg rotated mask is taller than wide (got {r_w}x{r_h})");
        assert_ne!(signature(&upright), signature(&rotated));
    }

    /// Direction-driven rotation must visibly rotate the preview. This
    /// guards the "preview doesn't rotate" regression: the UI passes
    /// motion direction into `make_spawn_input`, so a `Direction`-
    /// mapped rotation dynamic must alter the outline accordingly.
    #[test]
    fn direction_input_rotates_textured_preview() {
        let mut preset = textured_preset();
        preset.dynamics.rotation = Some(Mapping {
            source: DynSource::Direction,
            curve: Curve::linear(),
            range: (0.0, std::f32::consts::FRAC_PI_2),
            invert: false,
        });
        let no_motion = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { direction: 0.0, ..baseline_input() },
            20.0,
        );
        let quarter = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { direction: 1.0, ..baseline_input() },
            20.0,
        );
        let (u_xmin, u_xmax, u_ymin, u_ymax) = bounds(&no_motion);
        let (q_xmin, q_xmax, q_ymin, q_ymax) = bounds(&quarter);
        assert!(u_xmax - u_xmin > u_ymax - u_ymin, "no-motion mask wider than tall");
        assert!(q_ymax - q_ymin > q_xmax - q_xmin, "quarter-turn mask taller than wide");
    }

    /// Same as above but driven by `PenRotation`, the tablet barrel-
    /// twist axis. The UI threads `pen_rotation_rad` through; this
    /// test fails if the cursor stops consuming `input.pen_rotation`.
    #[test]
    fn pen_rotation_input_rotates_textured_preview() {
        let mut preset = textured_preset();
        preset.dynamics.rotation = Some(Mapping {
            source: DynSource::PenRotation,
            curve: Curve::linear(),
            range: (0.0, std::f32::consts::FRAC_PI_2),
            invert: false,
        });
        let upright = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { pen_rotation: 0.0, ..baseline_input() },
            20.0,
        );
        let twisted = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { pen_rotation: 1.0, ..baseline_input() },
            20.0,
        );
        let (u_xmin, u_xmax, u_ymin, u_ymax) = bounds(&upright);
        let (t_xmin, t_xmax, t_ymin, t_ymax) = bounds(&twisted);
        assert!(u_xmax - u_xmin > u_ymax - u_ymin);
        assert!(t_ymax - t_ymin > t_xmax - t_xmin);
    }

    /// Same as above but driven by tilt-azimuth (`Angle`). Verifies the
    /// UI's tilt-axis wiring is consumed by the cursor.
    #[test]
    fn angle_input_rotates_textured_preview() {
        let mut preset = textured_preset();
        preset.dynamics.rotation = Some(Mapping {
            source: DynSource::Angle,
            curve: Curve::linear(),
            range: (0.0, std::f32::consts::FRAC_PI_2),
            invert: false,
        });
        let upright = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { angle: 0.0, ..baseline_input() },
            20.0,
        );
        let tilted = compute_brush_cursor(
            &preset,
            baseline_ctx(),
            SpawnInput { angle: 1.0, ..baseline_input() },
            20.0,
        );
        let (u_xmin, u_xmax, u_ymin, u_ymax) = bounds(&upright);
        let (t_xmin, t_xmax, t_ymin, t_ymax) = bounds(&tilted);
        assert!(u_xmax - u_xmin > u_ymax - u_ymin);
        assert!(t_ymax - t_ymin > t_xmax - t_xmin);
    }

    #[test]
    fn scatter_dilates_outline_envelope_on_soft_round() {
        let preset_no_scatter = default_round_preset();
        let mut preset_scatter = default_round_preset();
        preset_scatter.dynamics.scatter = Some(Mapping {
            source: DynSource::Random,
            curve: Curve::linear(),
            range: (0.0, 12.0),
            invert: false,
        });
        let no = compute_brush_cursor(
            &preset_no_scatter,
            baseline_ctx(),
            baseline_input(),
            20.0,
        );
        let yes = compute_brush_cursor(
            &preset_scatter,
            baseline_ctx(),
            baseline_input(),
            20.0,
        );
        let (n_min, n_max, _, _) = bounds(&no);
        let (y_min, y_max, _, _) = bounds(&yes);
        let n_diam = n_max - n_min;
        let y_diam = y_max - y_min;
        // base diameter 20 -> envelope ~= 20 + 2*12 = 44.
        assert!(y_diam > n_diam + 20.0, "scatter must enlarge the envelope (no={n_diam}, yes={y_diam})");
        // Centre must stay put - scatter is shown as area, not offset.
        let n_cx = (n_min + n_max) * 0.5;
        let y_cx = (y_min + y_max) * 0.5;
        assert!((n_cx - y_cx).abs() < 0.1, "scatter must not displace the cursor centre");
    }

    #[test]
    fn scatter_dilates_textured_outline_envelope() {
        let mut preset = textured_preset();
        let baseline = compute_brush_cursor(&preset, baseline_ctx(), baseline_input(), 20.0);
        preset.dynamics.scatter = Some(Mapping {
            source: DynSource::Random,
            curve: Curve::linear(),
            range: (0.0, 10.0),
            invert: false,
        });
        let with_scatter = compute_brush_cursor(&preset, baseline_ctx(), baseline_input(), 20.0);
        let (b_xmin, b_xmax, _, _) = bounds(&baseline);
        let (s_xmin, s_xmax, _, _) = bounds(&with_scatter);
        assert!(
            s_xmax - s_xmin > (b_xmax - b_xmin) + 15.0,
            "scatter must enlarge textured envelope"
        );
    }

    #[test]
    fn aspect_squishes_outline() {
        let dab = Dab {
            center: Point::ZERO,
            radius: 10.0,
            rotation: 0.0,
            aspect: 0.5,
            flow: 1.0,
            color: Color::BLACK,
            texture_uv: [0.0, 0.0, 1.0, 1.0],
            hardness: 1.0,
            tip: 0.0,
            texture_scale: 0.0,
            texture_strength: 0.0,
            texturing_mode: 0.0,
            smudge_rate: 1.0,
            color_rate: 0.0,
        };
        let outline = ellipse_outline(&dab);
        let (mut xmin, mut xmax, mut ymin, mut ymax) =
            (f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY, f32::NEG_INFINITY);
        for p in &outline {
            xmin = xmin.min(p.x);
            xmax = xmax.max(p.x);
            ymin = ymin.min(p.y);
            ymax = ymax.max(p.y);
        }
        assert!((xmax - xmin) > (ymax - ymin) * 1.5, "aspect 0.5 squishes Y");
    }

    #[test]
    fn pixel_family_produces_circle_outline_not_square() {
        let preset = BrushPreset::pixel(BrushPresetId(0));
        let c = compute_brush_cursor(
            &preset,
            StrokeContext { size: 16.0, ..baseline_ctx() },
            baseline_input(),
            16.0,
        );
        let outline = &c.strokes[0];
        // Snapped circle must still have many more samples than a
        // 4-corner square. Catches regressions back to square output.
        assert!(outline.len() > 10, "pixel cursor must trace a circle, not a 4-corner square");
        // Verify roundness within one pixel of the snap grid.
        let cx = outline.iter().map(|p| p.x).sum::<f32>() / outline.len() as f32;
        let cy = outline.iter().map(|p| p.y).sum::<f32>() / outline.len() as f32;
        let mut min_r = f32::INFINITY;
        let mut max_r = f32::NEG_INFINITY;
        for p in outline {
            let r = (p.x - cx).hypot(p.y - cy);
            min_r = min_r.min(r);
            max_r = max_r.max(r);
        }
        assert!(
            (max_r - min_r) < 1.5,
            "pixel cursor outline must be circular within pixel-snap tolerance (got {min_r}..{max_r})"
        );
        // All vertices must lie on the integer pixel grid.
        for p in outline {
            assert_eq!(p.x.fract(), 0.0, "pixel outline must snap to integers: {p:?}");
            assert_eq!(p.y.fract(), 0.0, "pixel outline must snap to integers: {p:?}");
        }
    }

    #[test]
    fn pixel_family_skips_dynamics() {
        let mut preset = BrushPreset::pixel(BrushPresetId(0));
        preset.dynamics = Dynamics {
            size: Some(Mapping {
                source: DynSource::Pressure,
                curve: Curve::flat(0.1),
                range: (0.0, 1.0),
                invert: false,
            }),
            ..Dynamics::default()
        };
        let c = compute_brush_cursor(
            &preset,
            StrokeContext { size: 8.0, ..baseline_ctx() },
            SpawnInput { pressure: 0.1, ..baseline_input() },
            8.0,
        );
        let (xmin, xmax, _, _) = bounds(&c);
        assert!((xmax - xmin - 8.0).abs() < 0.1, "Pixel ignores size dynamics");
    }

    #[test]
    fn textured_outline_traces_mask_not_an_ellipse() {
        let preset = textured_preset();
        let textured = compute_brush_cursor(&preset, baseline_ctx(), baseline_input(), 20.0);
        // The 16x4 stripe pattern stretches to ~15x10 over a 20-px dab
        // (the dab's quad is square, so pattern UV is stretched
        // non-uniformly). The outline must reflect that aspect, not
        // come out as a 20x20 circle.
        let (xmin, xmax, ymin, ymax) = bounds(&textured);
        let w = xmax - xmin;
        let h = ymax - ymin;
        assert!(w > h * 1.3 && w < h * 1.8, "textured mask must reflect non-circular aspect, got {w}x{h}");
        // A round dab on the same preset (no texture) would be 20x20.
        let round = compute_brush_cursor(
            &default_round_preset(),
            baseline_ctx(),
            baseline_input(),
            20.0,
        );
        assert_ne!(signature(&textured), signature(&round));
    }

    /// Catch-all guard: every brush property currently routed into the
    /// dab must produce an observable outline difference relative to a
    /// baseline. If a property is added but never read by
    /// `compute_brush_cursor`, add a case here - and if an existing
    /// property is dropped, this test fails loudly.
    #[test]
    fn all_tracked_brush_properties_affect_cursor() {
        // Clean baseline preset: SoftRound, no dynamics. Each case
        // below mutates exactly one input axis or preset field.
        let baseline_preset = {
            let mut p = default_round_preset();
            p.dynamics = Dynamics::default();
            p
        };
        let base = compute_brush_cursor(
            &baseline_preset,
            baseline_ctx(),
            baseline_input(),
            20.0,
        );

        // Textured preset is used for direction / pen_rotation / angle
        // cases because rotation only has observable effect on
        // non-circularly-symmetric outlines.
        let textured_baseline = textured_preset();
        let textured_base = compute_brush_cursor(
            &textured_baseline,
            baseline_ctx(),
            baseline_input(),
            20.0,
        );

        let cases: Vec<(&str, BrushCursor, BrushCursor)> = vec![
            (
                "ctx.size",
                base.clone(),
                compute_brush_cursor(
                    &baseline_preset,
                    StrokeContext { size: 40.0, ..baseline_ctx() },
                    baseline_input(),
                    40.0,
                ),
            ),
            (
                "dynamics.size + pressure input",
                base.clone(),
                compute_brush_cursor(
                    &{
                        let mut p = baseline_preset.clone();
                        p.dynamics.size = Some(Mapping::pressure_linear());
                        p
                    },
                    baseline_ctx(),
                    SpawnInput { pressure: 0.25, ..baseline_input() },
                    20.0,
                ),
            ),
            (
                "dynamics.size + speed input",
                base.clone(),
                compute_brush_cursor(
                    &{
                        let mut p = baseline_preset.clone();
                        p.dynamics.size = Some(Mapping {
                            source: DynSource::Speed,
                            curve: Curve::linear(),
                            range: (0.2, 1.0),
                            invert: false,
                        });
                        p
                    },
                    baseline_ctx(),
                    SpawnInput { speed: 0.8, ..baseline_input() },
                    20.0,
                ),
            ),
            (
                "dynamics.size + distance input",
                base.clone(),
                compute_brush_cursor(
                    &{
                        let mut p = baseline_preset.clone();
                        p.dynamics.size = Some(Mapping {
                            source: DynSource::Distance,
                            curve: Curve::linear(),
                            range: (0.2, 1.0),
                            invert: false,
                        });
                        p
                    },
                    baseline_ctx(),
                    SpawnInput { distance: 0.75, ..baseline_input() },
                    20.0,
                ),
            ),
            (
                "dynamics.rotation + direction input (textured)",
                textured_base.clone(),
                compute_brush_cursor(
                    &{
                        let mut p = textured_preset();
                        p.dynamics.rotation = Some(Mapping {
                            source: DynSource::Direction,
                            curve: Curve::linear(),
                            range: (0.0, std::f32::consts::FRAC_PI_2),
                            invert: false,
                        });
                        p
                    },
                    baseline_ctx(),
                    SpawnInput { direction: 0.5, ..baseline_input() },
                    20.0,
                ),
            ),
            (
                "dynamics.rotation + pen_rotation input (textured)",
                textured_base.clone(),
                compute_brush_cursor(
                    &{
                        let mut p = textured_preset();
                        p.dynamics.rotation = Some(Mapping {
                            source: DynSource::PenRotation,
                            curve: Curve::linear(),
                            range: (0.0, std::f32::consts::FRAC_PI_2),
                            invert: false,
                        });
                        p
                    },
                    baseline_ctx(),
                    SpawnInput { pen_rotation: 0.5, ..baseline_input() },
                    20.0,
                ),
            ),
            (
                "dynamics.rotation + angle (tilt) input (textured)",
                textured_base.clone(),
                compute_brush_cursor(
                    &{
                        let mut p = textured_preset();
                        p.dynamics.rotation = Some(Mapping {
                            source: DynSource::Angle,
                            curve: Curve::linear(),
                            range: (0.0, std::f32::consts::FRAC_PI_2),
                            invert: false,
                        });
                        p
                    },
                    baseline_ctx(),
                    SpawnInput { angle: 0.5, ..baseline_input() },
                    20.0,
                ),
            ),
            (
                "dynamics.scatter (envelope dilation)",
                base.clone(),
                compute_brush_cursor(
                    &{
                        let mut p = baseline_preset.clone();
                        p.dynamics.scatter = Some(Mapping {
                            source: DynSource::Random,
                            curve: Curve::linear(),
                            range: (0.0, 8.0),
                            invert: false,
                        });
                        p
                    },
                    baseline_ctx(),
                    baseline_input(),
                    20.0,
                ),
            ),
            (
                "family Pixel",
                base.clone(),
                compute_brush_cursor(
                    &BrushPreset::pixel(BrushPresetId(0)),
                    baseline_ctx(),
                    baseline_input(),
                    20.0,
                ),
            ),
            (
                "family Textured",
                base.clone(),
                textured_base.clone(),
            ),
        ];

        for (name, baseline_cursor, mutated) in &cases {
            assert_ne!(
                signature(baseline_cursor),
                signature(mutated),
                "property `{name}` must affect the brush cursor outline"
            );
        }
    }
}
