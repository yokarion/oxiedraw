//! Shape-drawing assistant.
//!
//! Turns a paused freehand stroke into a cleaner version of itself. Instead of
//! an all-or-nothing "snap to a perfect primitive or do nothing", every stroke
//! is corrected with a variable strength: the output is a per-point blend
//! between a jitter-smoothed copy of the stroke and its projection onto a
//! fitted primitive (straight line or ellipse). The blend weight comes from how
//! close the stroke already is to that primitive, so an almost-perfect circle
//! snaps hard while a deliberately wobbly one only gets tidied up.

use oxiedraw_utils::geometry::{Point, arc_length, bounding_box, morph_path, resample};

const MIN_POINTS: usize = 8;
const MIN_STROKE_LENGTH: f32 = 30.0;

/// Which primitive family a correction belongs to, used only to gate against
/// the per-type enable switches in the preferences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeKind {
    Line,
    Ellipse,
    Rectangle,
}

/// A ready-to-apply correction: `target[i]` is where sample `i` of the input
/// stroke should end up. Always the same length as the input, so per-sample pen
/// dynamics ride along unchanged.
#[derive(Debug, Clone)]
pub struct Correction {
    pub kind: ShapeKind,
    pub target: Vec<Point>,
}

/// Analyze a freehand stroke and return the corrected target positions, or
/// `None` when the stroke doesn't look like anything we can help with.
pub fn detect_correction(points: &[Point]) -> Option<Correction> {
    if points.len() < MIN_POINTS {
        return None;
    }
    let arc = arc_length(points);
    if arc < MIN_STROKE_LENGTH {
        return None;
    }

    let (min_x, min_y, max_x, max_y) = bounding_box(points);
    let bbox_diag = Point::new(min_x, min_y).distance(Point::new(max_x, max_y));

    if is_closed(points, bbox_diag) {
        // A closed loop is either a rectangle (a distinct intentional shape,
        // kept strict) or an ellipse.
        rectangle_correction(points, arc).or_else(|| ellipse_correction(points))
    } else {
        line_correction(points, arc)
    }
}

// -- smoothing --------------------------------------------------------------

/// Smooth a polyline in place (same length) with repeated [1,2,1] passes to
/// kill hand jitter while keeping the overall shape. Open strokes pin their
/// endpoints; closed strokes wrap around so the seam stays continuous.
fn smooth_polyline(points: &[Point], closed: bool) -> Vec<Point> {
    let n = points.len();
    if n < 3 {
        return points.to_vec();
    }
    // Denser strokes carry more high-frequency jitter, so smooth them harder.
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let passes = ((n as f32 / 15.0) as usize).clamp(3, 12);

    let mut cur = points.to_vec();
    let mut next = cur.clone();
    for _ in 0..passes {
        for i in 0..n {
            let (a, b) = if closed {
                (cur[(i + n - 1) % n], cur[(i + 1) % n])
            } else if i == 0 || i == n - 1 {
                // Keep endpoints fixed for open strokes.
                next[i] = cur[i];
                continue;
            } else {
                (cur[i - 1], cur[i + 1])
            };
            next[i] = Point::new(
                a.x.mul_add(0.25, cur[i].x * 0.5) + b.x * 0.25,
                a.y.mul_add(0.25, cur[i].y * 0.5) + b.y * 0.25,
            );
        }
        std::mem::swap(&mut cur, &mut next);
    }
    cur
}

/// Smoothstep that ramps 0 -> 1 across `[edge0, edge1]`.
fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge1 <= edge0 {
        return if x < edge0 { 0.0 } else { 1.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

// -- line / open curve ------------------------------------------------------

// Line/curve tuning: residual below LO snaps fully straight, above HI keeps the
// smoothed curve untouched.
const LINE_RES_LO: f32 = 0.015;
const LINE_RES_HI: f32 = 0.06;

fn line_correction(points: &[Point], arc: f32) -> Option<Correction> {
    let start = points[0];
    let end = *points.last().expect("non-empty slice");
    let chord = start.distance(end);
    if chord < f32::EPSILON {
        return None;
    }
    // A very low chord/arc ratio means a coil or scribble, not a line the user
    // wants straightened; leave those alone.
    if chord < arc * 0.45 {
        return None;
    }

    let smoothed = smooth_polyline(points, false);
    let (center, dir) = fit_line(&smoothed);

    // Project each smoothed point perpendicularly onto the fitted line; that is
    // the fully-straightened target.
    let straight: Vec<Point> = smoothed
        .iter()
        .map(|p| {
            let t = (p.x - center.x).mul_add(dir.x, (p.y - center.y) * dir.y);
            Point::new(center.x + dir.x * t, center.y + dir.y * t)
        })
        .collect();

    #[allow(clippy::cast_precision_loss)]
    let n = smoothed.len() as f32;
    let rms = (smoothed
        .iter()
        .zip(straight.iter())
        .map(|(p, q)| p.distance(*q).powi(2))
        .sum::<f32>()
        / n)
        .sqrt();
    let residual = rms / chord;
    let w = 1.0 - smoothstep(LINE_RES_LO, LINE_RES_HI, residual);

    let target = smoothed
        .iter()
        .zip(straight.iter())
        .map(|(s, q)| s.lerp(*q, w))
        .collect();

    Some(Correction {
        kind: ShapeKind::Line,
        target,
    })
}

/// Total-least-squares line fit: returns a point on the line (the centroid) and
/// a unit direction along it.
fn fit_line(points: &[Point]) -> (Point, Point) {
    #[allow(clippy::cast_precision_loss)]
    let n = points.len() as f32;
    let cx = points.iter().map(|p| p.x).sum::<f32>() / n;
    let cy = points.iter().map(|p| p.y).sum::<f32>() / n;
    let (mut sxx, mut syy, mut sxy) = (0.0_f32, 0.0_f32, 0.0_f32);
    for p in points {
        let dx = p.x - cx;
        let dy = p.y - cy;
        sxx += dx * dx;
        syy += dy * dy;
        sxy += dx * dy;
    }
    let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    (Point::new(cx, cy), Point::new(theta.cos(), theta.sin()))
}

// -- ellipse ----------------------------------------------------------------

// Ellipse tuning: residual below LO snaps to a perfect ellipse, above HI keeps
// the smoothed loop; beyond MAX the loop isn't ellipse-like enough to touch.
const ELLIPSE_RES_LO: f32 = 0.03;
const ELLIPSE_RES_HI: f32 = 0.16;
const ELLIPSE_RES_MAX: f32 = 0.45;
const ELLIPSE_MAX_ASPECT: f32 = 5.0;

struct Ellipse {
    center: Point,
    major: Point, // unit direction of the a-axis
    a: f32,
    b: f32,
}

impl Ellipse {
    fn minor(&self) -> Point {
        Point::new(-self.major.y, self.major.x)
    }

    /// Radially project a point onto the ellipse boundary (same ray from the
    /// center), so a nearby point only moves in/out, never sideways.
    fn project(&self, p: Point) -> Point {
        let minor = self.minor();
        let dx = p.x - self.center.x;
        let dy = p.y - self.center.y;
        let lx = dx.mul_add(self.major.x, dy * self.major.y);
        let ly = dx.mul_add(minor.x, dy * minor.y);
        let denom = (lx / self.a).powi(2) + (ly / self.b).powi(2);
        if denom < f32::EPSILON {
            return p;
        }
        let scale = 1.0 / denom.sqrt();
        let bx = lx * scale;
        let by = ly * scale;
        Point::new(
            self.center.x + bx * self.major.x + by * minor.x,
            self.center.y + bx * self.major.y + by * minor.y,
        )
    }
}

fn ellipse_correction(points: &[Point]) -> Option<Correction> {
    let ellipse = fit_ellipse(points)?;
    if ellipse.a > ellipse.b * ELLIPSE_MAX_ASPECT {
        return None; // too elongated to read as an oval
    }

    let smoothed = smooth_polyline(points, true);
    let ideal: Vec<Point> = smoothed.iter().map(|p| ellipse.project(*p)).collect();

    #[allow(clippy::cast_precision_loss)]
    let n = smoothed.len() as f32;
    let rms = (smoothed
        .iter()
        .zip(ideal.iter())
        .map(|(p, q)| p.distance(*q).powi(2))
        .sum::<f32>()
        / n)
        .sqrt();
    let r_mean = (ellipse.a + ellipse.b) * 0.5;
    if r_mean < 10.0 {
        return None;
    }
    let residual = rms / r_mean;
    if residual > ELLIPSE_RES_MAX {
        return None; // a closed blob, not an ellipse
    }
    let w = 1.0 - smoothstep(ELLIPSE_RES_LO, ELLIPSE_RES_HI, residual);

    let target = smoothed
        .iter()
        .zip(ideal.iter())
        .map(|(s, q)| s.lerp(*q, w))
        .collect();

    Some(Correction {
        kind: ShapeKind::Ellipse,
        target,
    })
}

/// Least-squares ellipse fit: centroid for the center, covariance eigenvector
/// for the orientation, then a linear fit of the two axis lengths in the
/// rotated frame. Robust enough for a hand-drawn loop and cheap to compute.
fn fit_ellipse(points: &[Point]) -> Option<Ellipse> {
    // Resample by arc length first so uneven sampling speed doesn't bias the
    // centroid or the covariance toward the slow parts of the stroke.
    let res = resample(points, 128);
    #[allow(clippy::cast_precision_loss)]
    let n = res.len() as f32;
    if n < 3.0 {
        return None;
    }
    let cx = res.iter().map(|p| p.x).sum::<f32>() / n;
    let cy = res.iter().map(|p| p.y).sum::<f32>() / n;

    let (mut sxx, mut syy, mut sxy) = (0.0_f32, 0.0_f32, 0.0_f32);
    for p in &res {
        let dx = p.x - cx;
        let dy = p.y - cy;
        sxx += dx * dx;
        syy += dy * dy;
        sxy += dx * dy;
    }
    let theta = 0.5 * (2.0 * sxy).atan2(sxx - syy);
    let mut major = Point::new(theta.cos(), theta.sin());
    let mut minor = Point::new(-major.y, major.x);

    // Fit A*lx^2 + B*ly^2 = 1 by least squares -> a = 1/sqrt(A), b = 1/sqrt(B).
    let (mut m00, mut m01, mut m11, mut r0, mut r1) = (0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32, 0.0_f32);
    for p in &res {
        let dx = p.x - cx;
        let dy = p.y - cy;
        let lx = dx.mul_add(major.x, dy * major.y);
        let ly = dx.mul_add(minor.x, dy * minor.y);
        let x2 = lx * lx;
        let y2 = ly * ly;
        m00 += x2 * x2;
        m01 += x2 * y2;
        m11 += y2 * y2;
        r0 += x2;
        r1 += y2;
    }
    let det = m00.mul_add(m11, -(m01 * m01));
    if det.abs() < f32::EPSILON {
        return None;
    }
    let coef_a = r0.mul_add(m11, -(r1 * m01)) / det;
    let coef_b = m00.mul_add(r1, -(m01 * r0)) / det;
    if coef_a <= 0.0 || coef_b <= 0.0 {
        return None;
    }
    let mut a = 1.0 / coef_a.sqrt();
    let mut b = 1.0 / coef_b.sqrt();

    // Keep `major` as the longer axis.
    if a < b {
        std::mem::swap(&mut a, &mut b);
        std::mem::swap(&mut major, &mut minor);
    }

    Some(Ellipse {
        center: Point::new(cx, cy),
        major,
        a,
        b,
    })
}

// -- rectangle --------------------------------------------------------------

fn rectangle_correction(points: &[Point], arc: f32) -> Option<Correction> {
    let corners = fit_rectangle(detect_rectangle_corners(points, arc)?);
    let geo = rectangle_polyline(&corners);
    let target = morph_path(points, &geo);
    if target.is_empty() {
        return None;
    }
    Some(Correction {
        kind: ShapeKind::Rectangle,
        target,
    })
}

/// Dense perimeter polyline of a rectangle (closed loop), used as the morph
/// target for the freehand samples.
fn rectangle_polyline(corners: &[Point; 4]) -> Vec<Point> {
    const STEPS: usize = 24;
    let mut pts = Vec::with_capacity(STEPS * 4 + 1);
    for i in 0..4 {
        let from = corners[i];
        let to = corners[(i + 1) % 4];
        for j in 0..STEPS {
            #[allow(clippy::cast_precision_loss)]
            let t = j as f32 / STEPS as f32;
            pts.push(Point::new(
                (to.x - from.x).mul_add(t, from.x),
                (to.y - from.y).mul_add(t, from.y),
            ));
        }
    }
    pts.push(corners[0]);
    pts
}

// Returns true when the stroke appears to close back on itself.
fn is_closed(points: &[Point], bbox_diag: f32) -> bool {
    let Some(last) = points.last() else {
        return false;
    };
    points[0].distance(*last) < bbox_diag * 0.25
}

// Ramer-Douglas-Peucker simplification.
fn rdp(points: &[Point], epsilon: f32) -> Vec<Point> {
    if points.len() < 3 {
        return points.to_vec();
    }
    let start = points[0];
    let end = *points.last().expect("non-empty slice");

    let mut max_dist = 0.0_f32;
    let mut max_idx = 0_usize;
    for (i, p) in points[1..points.len() - 1].iter().enumerate() {
        let d = p.perp_distance_to_line(start, end);
        if d > max_dist {
            max_dist = d;
            max_idx = i + 1;
        }
    }

    if max_dist > epsilon {
        let mut left = rdp(&points[..=max_idx], epsilon);
        let right = rdp(&points[max_idx..], epsilon);
        left.pop();
        left.extend(right);
        left
    } else {
        vec![start, end]
    }
}

fn detect_rectangle_corners(points: &[Point], arc: f32) -> Option<[Point; 4]> {
    let simplified = rdp(points, arc * 0.05);

    let corners: [Point; 4] = if simplified.len() == 5 {
        let last = *simplified.last().expect("len == 5");
        if simplified[0].distance(last) > arc * 0.15 {
            return None; // Path is not closed enough.
        }
        [simplified[0], simplified[1], simplified[2], simplified[3]]
    } else if simplified.len() == 4 {
        [simplified[0], simplified[1], simplified[2], simplified[3]]
    } else {
        return None;
    };

    // each corner should be almost 90*
    for i in 0..4 {
        let prev = corners[(i + 3) % 4];
        let curr = corners[i];
        let next = corners[(i + 1) % 4];
        let v1 = Point::new(prev.x - curr.x, prev.y - curr.y).normalize();
        let v2 = Point::new(next.x - curr.x, next.y - curr.y).normalize();
        let dot = v1.x.mul_add(v2.x, v1.y * v2.y);
        // |dot| < 0.5 -> corner angle between 60deg and 120deg
        if dot.abs() > 0.5 {
            return None;
        }
    }

    // The four corners alone can't tell a rectangle from a circle - a circle's
    // 90deg arcs also simplify to four ~right-angle corners. The difference is
    // the edges: a rectangle's sides run straight between corners, a circle's
    // arcs bulge away from the chord. Reject if any edge is too curved.
    if !edges_are_straight(points, &corners) {
        return None;
    }

    Some(corners)
}

/// True when every original point sits close to the rectangle edge it belongs
/// to (max perpendicular deviation under 10% of that edge's length). This is
/// what separates a real rectangle from a circle whose arcs happen to simplify
/// to four corners.
fn edges_are_straight(points: &[Point], corners: &[Point; 4]) -> bool {
    // Assign each corner to its nearest original point index.
    let corner_idx: Vec<usize> = corners
        .iter()
        .map(|c| {
            (0..points.len())
                .min_by(|&i, &j| {
                    points[i]
                        .distance(*c)
                        .partial_cmp(&points[j].distance(*c))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .unwrap_or(0)
        })
        .collect();

    let n = points.len();
    for e in 0..4 {
        let a = corners[e];
        let b = corners[(e + 1) % 4];
        let edge_len = a.distance(b);
        if edge_len < f32::EPSILON {
            continue;
        }
        // Walk original points from this corner to the next, wrapping around.
        let mut i = corner_idx[e];
        let end = corner_idx[(e + 1) % 4];
        let mut max_dev = 0.0_f32;
        let mut steps = 0;
        while i != end && steps <= n {
            max_dev = max_dev.max(points[i].perp_distance_to_line(a, b));
            i = (i + 1) % n;
            steps += 1;
        }
        if max_dev > edge_len * 0.10 {
            return false;
        }
    }
    true
}

/// Snap four detected corners into a geometrically perfect rectangle.
fn fit_rectangle(corners: [Point; 4]) -> [Point; 4] {
    let cx = corners.iter().map(|p| p.x).sum::<f32>() / 4.0;
    let cy = corners.iter().map(|p| p.y).sum::<f32>() / 4.0;
    let center = Point::new(cx, cy);

    let d0 = Point::new(corners[1].x - corners[0].x, corners[1].y - corners[0].y).normalize();
    let d2_raw = Point::new(corners[3].x - corners[2].x, corners[3].y - corners[2].y).normalize();
    let dot = d0.x.mul_add(d2_raw.x, d0.y * d2_raw.y);
    let d2 = if dot < 0.0 {
        Point::new(-d2_raw.x, -d2_raw.y)
    } else {
        d2_raw
    };
    let avg = Point::new(d0.x + d2.x, d0.y + d2.y).normalize();

    if avg.x.abs() < f32::EPSILON && avg.y.abs() < f32::EPSILON {
        return corners;
    }

    let perp = Point::new(-avg.y, avg.x);

    let half_w = corners
        .iter()
        .map(|p| {
            (p.x - center.x)
                .mul_add(avg.x, (p.y - center.y) * avg.y)
                .abs()
        })
        .fold(0.0_f32, f32::max);
    let half_h = corners
        .iter()
        .map(|p| {
            (p.x - center.x)
                .mul_add(perp.x, (p.y - center.y) * perp.y)
                .abs()
        })
        .fold(0.0_f32, f32::max);

    let raw = [
        Point::new(
            center.x - half_w * avg.x - half_h * perp.x,
            center.y - half_w * avg.y - half_h * perp.y,
        ),
        Point::new(
            center.x + half_w * avg.x - half_h * perp.x,
            center.y + half_w * avg.y - half_h * perp.y,
        ),
        Point::new(
            center.x + half_w * avg.x + half_h * perp.x,
            center.y + half_w * avg.y + half_h * perp.y,
        ),
        Point::new(
            center.x - half_w * avg.x + half_h * perp.x,
            center.y - half_w * avg.y + half_h * perp.y,
        ),
    ];

    // Rotate output so corner[0] sits closest to the input corner[0].
    let start = (0..4)
        .min_by(|&i, &j| {
            raw[i]
                .distance(corners[0])
                .partial_cmp(&raw[j].distance(corners[0]))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(0);
    let rotated = [
        raw[start],
        raw[(start + 1) % 4],
        raw[(start + 2) % 4],
        raw[(start + 3) % 4],
    ];

    // Preserve the input winding direction
    let in_cross = (corners[1].x - corners[0].x).mul_add(
        corners[2].y - corners[0].y,
        -((corners[1].y - corners[0].y) * (corners[2].x - corners[0].x)),
    );
    let out_cross = (rotated[1].x - rotated[0].x).mul_add(
        rotated[2].y - rotated[0].y,
        -((rotated[1].y - rotated[0].y) * (rotated[2].x - rotated[0].x)),
    );
    if (in_cross > 0.0) == (out_cross > 0.0) {
        rotated
    } else {
        // Flip
        [rotated[0], rotated[3], rotated[2], rotated[1]]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::f32::consts::TAU;

    use super::*;

    /// Cross product z-component of vectors AB and AC.
    /// Positive = clockwise in screen coords (y-down).
    fn cross_z(a: Point, b: Point, c: Point) -> f32 {
        (b.x - a.x).mul_add(c.y - a.y, -((b.y - a.y) * (c.x - a.x)))
    }

    fn all_right_angles(c: [Point; 4]) -> bool {
        (0..4).all(|i| {
            let prev = c[(i + 3) % 4];
            let curr = c[i];
            let next = c[(i + 1) % 4];
            let v1 = Point::new(prev.x - curr.x, prev.y - curr.y).normalize();
            let v2 = Point::new(next.x - curr.x, next.y - curr.y).normalize();
            v1.x.mul_add(v2.x, v1.y * v2.y).abs() < 0.01
        })
    }

    fn circle_points(cx: f32, cy: f32, r: f32, n: usize, jitter: f32) -> Vec<Point> {
        (0..=n)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let a = TAU * i as f32 / n as f32;
                // Deterministic pseudo-jitter so tests stay stable.
                let wob = jitter * (a * 5.0).sin();
                Point::new((r + wob).mul_add(a.cos(), cx), (r + wob).mul_add(a.sin(), cy))
            })
            .collect()
    }

    // -- line -----------------------------------------------------------------

    #[test]
    fn near_straight_line_snaps_straight() {
        // Gentle wobble around a horizontal line -> should collapse onto it.
        let pts: Vec<Point> = (0..40)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let x = i as f32 * 5.0;
                Point::new(x, 100.0 + (x * 0.2).sin() * 0.8)
            })
            .collect();
        let c = detect_correction(&pts).expect("should correct");
        assert_eq!(c.kind, ShapeKind::Line);
        let max_dev = c
            .target
            .iter()
            .map(|p| (p.y - 100.0).abs())
            .fold(0.0_f32, f32::max);
        assert!(max_dev < 0.5, "line should be flat, max dev {max_dev}");
    }

    #[test]
    fn curvy_line_keeps_its_bend() {
        // A clear arc: chord/arc still high enough to be a "line", but far from
        // straight, so the bend must survive (only smoothed).
        let pts: Vec<Point> = (0..40)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let t = i as f32 / 39.0;
                let x = t * 200.0;
                let y = 100.0 + (t * std::f32::consts::PI).sin() * 40.0;
                Point::new(x, y)
            })
            .collect();
        let c = detect_correction(&pts).expect("should correct");
        assert_eq!(c.kind, ShapeKind::Line);
        // Midpoint bulge should stay well away from the straight chord.
        let mid = c.target[c.target.len() / 2];
        assert!(
            mid.y > 120.0,
            "curve should be preserved, mid y = {}",
            mid.y
        );
    }

    // -- ellipse --------------------------------------------------------------

    #[test]
    fn clean_circle_snaps_round() {
        let pts = circle_points(100.0, 100.0, 60.0, 64, 0.0);
        let c = detect_correction(&pts).expect("should correct");
        assert_eq!(c.kind, ShapeKind::Ellipse);
        let radii: Vec<f32> = c
            .target
            .iter()
            .map(|p| p.distance(Point::new(100.0, 100.0)))
            .collect();
        let max_r = radii.iter().copied().fold(0.0_f32, f32::max);
        let min_r = radii.iter().copied().fold(f32::MAX, f32::min);
        assert!(
            max_r - min_r < 1.0,
            "clean circle should be near-perfect, spread {}",
            max_r - min_r
        );
    }

    #[test]
    fn oval_stays_oval() {
        // Axis-aligned ellipse, rx = 100, ry = 40.
        let pts: Vec<Point> = (0..=64)
            .map(|i| {
                #[allow(clippy::cast_precision_loss)]
                let a = TAU * i as f32 / 64.0;
                Point::new(100.0f32.mul_add(a.cos(), 200.0), 40.0f32.mul_add(a.sin(), 200.0))
            })
            .collect();
        let c = detect_correction(&pts).expect("should correct");
        assert_eq!(c.kind, ShapeKind::Ellipse);
        let (min_x, min_y, max_x, max_y) = bounding_box(&c.target);
        let w = max_x - min_x;
        let h = max_y - min_y;
        assert!(
            (w / h - 2.5).abs() < 0.4,
            "oval aspect should be preserved, got {}",
            w / h
        );
    }

    // -- fit_rectangle (unchanged) -------------------------------------------

    #[test]
    fn rect_cw_axis_aligned_start_and_winding() {
        let input = [
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
            Point::new(0.0, 10.0),
            Point::new(0.0, 0.0),
        ];
        let fitted = fit_rectangle(input);
        assert!(fitted[0].distance(input[0]) < 0.5);
        assert!(cross_z(fitted[0], fitted[1], fitted[2]) > 0.0);
        assert!(all_right_angles(fitted));
    }

    #[test]
    fn rect_rotated_45_cw_start_and_right_angles() {
        let s = 7.071_f32;
        let input = [
            Point::new(0.0, -s),
            Point::new(s, 0.0),
            Point::new(0.0, s),
            Point::new(-s, 0.0),
        ];
        let fitted = fit_rectangle(input);
        assert!(fitted[0].distance(input[0]) < 0.5);
        assert!(cross_z(fitted[0], fitted[1], fitted[2]) > 0.0);
        assert!(all_right_angles(fitted));
    }
}
