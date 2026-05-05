use std::f32::consts::{PI, TAU};

use oxiedraw_utils::geometry::{Point, arc_length, bounding_box};

const MIN_POINTS: usize = 8;
const MIN_STROKE_LENGTH: f32 = 30.0;

#[derive(Debug, Clone)]
pub enum CorrectedShape {
    Line {
        start: Point,
        end: Point,
    },
    Circle {
        center: Point,
        radius: f32,
        start_angle: f32,
        clockwise: bool,
    },
    Rectangle {
        corners: [Point; 4],
    },
}

// Returns true when the stroke appears to close back on itself.
fn is_closed(points: &[Point], bbox_diag: f32) -> bool {
    let Some(last) = points.last() else {
        return false;
    };
    points[0].distance(*last) < bbox_diag * 0.2
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

fn detect_line(points: &[Point], arc: f32) -> Option<CorrectedShape> {
    let start = points[0];
    let end = *points.last().expect("non-empty slice");
    let chord = start.distance(end);

    // At least 70%
    if chord < arc * 0.7 {
        return None;
    }

    // Max deviation 12%
    let max_dev = points
        .iter()
        .map(|p| p.perp_distance_to_line(start, end))
        .fold(0.0_f32, f32::max);

    if max_dev < chord * 0.12 {
        Some(CorrectedShape::Line { start, end })
    } else {
        None
    }
}

fn detect_circle(points: &[Point]) -> Option<CorrectedShape> {
    let (min_x, min_y, max_x, max_y) = bounding_box(points);
    let bbox_diag = Point::new(min_x, min_y).distance(Point::new(max_x, max_y));

    if !is_closed(points, bbox_diag) {
        return None;
    }

    // Center calc
    #[allow(clippy::cast_precision_loss)]
    let n = points.len() as f32;
    let cx = points.iter().map(|p| p.x).sum::<f32>() / n;
    let cy = points.iter().map(|p| p.y).sum::<f32>() / n;
    let center = Point::new(cx, cy);

    let radii: Vec<f32> = points.iter().map(|p| center.distance(*p)).collect();
    let avg_r = radii.iter().sum::<f32>() / n;

    if avg_r < 10.0 {
        return None;
    }

    // Max 20% of deviation
    let variance = radii.iter().map(|r| (r - avg_r).powi(2)).sum::<f32>() / n;
    let cv = variance.sqrt() / avg_r;
    if cv > 0.20 {
        return None;
    }

    // Circle almost 270* or more finished
    if angular_coverage(points, center) < PI * 1.5 {
        return None;
    }

    // Check if bounding box close to 1:1 ratio
    let width = max_x - min_x;
    let height = max_y - min_y;
    if width < f32::EPSILON || height < f32::EPSILON {
        return None;
    }
    let aspect = width.max(height) / width.min(height);
    if aspect > 1.5 {
        return None;
    }

    // Align the corrected circle start with where the user began drawing.
    let start_angle = (points[0].y - cy).atan2(points[0].x - cx);
    // Detect winding: positive shoelace area in screen coords (y-down) = clockwise.
    let signed_area: f32 = points
        .windows(2)
        .map(|w| w[0].x.mul_add(w[1].y, -(w[1].x * w[0].y)))
        .sum();
    let clockwise = signed_area > 0.0;

    Some(CorrectedShape::Circle {
        center,
        radius: avg_r,
        start_angle,
        clockwise,
    })
}

fn angular_coverage(points: &[Point], center: Point) -> f32 {
    let mut angles: Vec<f32> = points
        .iter()
        .map(|p| (p.y - center.y).atan2(p.x - center.x))
        .collect();
    angles.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let mut max_gap = 0.0_f32;
    for w in angles.windows(2) {
        max_gap = max_gap.max(w[1] - w[0]);
    }
    if let (Some(&first), Some(&last)) = (angles.first(), angles.last()) {
        max_gap = max_gap.max((first + TAU) - last);
    }
    TAU - max_gap
}

fn detect_rectangle(points: &[Point], arc: f32) -> Option<CorrectedShape> {
    let (min_x, min_y, max_x, max_y) = bounding_box(points);
    let bbox_diag = Point::new(min_x, min_y).distance(Point::new(max_x, max_y));

    if !is_closed(points, bbox_diag) {
        return None;
    }

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

    Some(CorrectedShape::Rectangle {
        corners: fit_rectangle(corners),
    })
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

// Analyze best fitting shape: rectangle -> circle -> line.
pub fn detect_shape(points: &[Point]) -> Option<CorrectedShape> {
    if points.len() < MIN_POINTS {
        return None;
    }
    let arc = arc_length(points);
    if arc < MIN_STROKE_LENGTH {
        return None;
    }

    detect_rectangle(points, arc)
        .or_else(|| detect_circle(points))
        .or_else(|| detect_line(points, arc))
}

pub fn corrected_samples(shape: &CorrectedShape) -> Vec<Point> {
    match shape {
        CorrectedShape::Line { start, end } => {
            const COUNT: usize = 32;
            (0..COUNT)
                .map(|i| {
                    #[allow(clippy::cast_precision_loss)]
                    let t = i as f32 / (COUNT - 1) as f32;
                    Point::new(
                        (end.x - start.x).mul_add(t, start.x),
                        (end.y - start.y).mul_add(t, start.y),
                    )
                })
                .collect()
        }

        CorrectedShape::Circle {
            center,
            radius,
            start_angle,
            clockwise,
        } => {
            const COUNT: usize = 64;
            // In screen coords (y-down) atan2 increases going CW, so CW = positive step.
            let dir = if *clockwise { 1.0_f32 } else { -1.0_f32 };
            (0..=COUNT)
                .map(|i| {
                    #[allow(clippy::cast_precision_loss)]
                    let angle = start_angle + dir * TAU * i as f32 / COUNT as f32;
                    Point::new(
                        center.x + radius * angle.cos(),
                        center.y + radius * angle.sin(),
                    )
                })
                .collect()
        }

        CorrectedShape::Rectangle { corners } => {
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
            // Close the loop.
            pts.push(corners[0]);
            pts
        }
    }
}

#[cfg(test)]
mod tests {
    use std::f32::consts::PI;

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

    // --- fit_rectangle ---

    #[test]
    fn rect_cw_axis_aligned_start_and_winding() {
        // CW in screen coords (y-down): TR -> BR -> BL -> TL
        let input = [
            Point::new(10.0, 0.0),
            Point::new(10.0, 10.0),
            Point::new(0.0, 10.0),
            Point::new(0.0, 0.0),
        ];
        let fitted = fit_rectangle(input);

        assert!(
            fitted[0].distance(input[0]) < 0.5,
            "corner[0] should stay near TR, got {:?}",
            fitted[0]
        );
        assert!(
            cross_z(fitted[0], fitted[1], fitted[2]) > 0.0,
            "output should be CW"
        );
        assert!(all_right_angles(fitted), "all corners should be 90deg");
    }

    #[test]
    fn rect_ccw_axis_aligned_start_and_winding() {
        // CCW: TR -> TL -> BL -> BR
        let input = [
            Point::new(10.0, 0.0),
            Point::new(0.0, 0.0),
            Point::new(0.0, 10.0),
            Point::new(10.0, 10.0),
        ];
        let fitted = fit_rectangle(input);

        assert!(
            fitted[0].distance(input[0]) < 0.5,
            "corner[0] should stay near TR, got {:?}",
            fitted[0]
        );
        assert!(
            cross_z(fitted[0], fitted[1], fitted[2]) < 0.0,
            "output should be CCW"
        );
        assert!(all_right_angles(fitted), "all corners should be 90deg");
    }

    #[test]
    fn rect_wobbly_cw_preserves_start_and_winding() {
        // Slightly imperfect CW rectangle
        let input = [
            Point::new(10.3, 0.2),
            Point::new(9.9, 10.1),
            Point::new(0.1, 9.8),
            Point::new(0.4, 0.3),
        ];
        let fitted = fit_rectangle(input);

        let d0 = fitted[0].distance(input[0]);
        assert!(
            (1..4).all(|i| fitted[i].distance(input[0]) >= d0),
            "fitted[0] should be the corner closest to input[0]"
        );
        assert!(
            (cross_z(input[0], input[1], input[2]) > 0.0)
                == (cross_z(fitted[0], fitted[1], fitted[2]) > 0.0),
            "winding direction should be preserved"
        );
        assert!(all_right_angles(fitted), "all corners should be 90deg");
    }

    #[test]
    fn rect_rotated_45_cw_start_and_right_angles() {
        // 45deg-rotated diamond, CW: top -> right -> bottom -> left
        let s = 7.071_f32;
        let input = [
            Point::new(0.0, -s),
            Point::new(s, 0.0),
            Point::new(0.0, s),
            Point::new(-s, 0.0),
        ];
        let fitted = fit_rectangle(input);

        assert!(
            fitted[0].distance(input[0]) < 0.5,
            "corner[0] should be near top, got {:?}",
            fitted[0]
        );
        assert!(
            cross_z(fitted[0], fitted[1], fitted[2]) > 0.0,
            "output should be CW"
        );
        assert!(all_right_angles(fitted), "all corners should be 90deg");
    }

    // --- corrected_samples: Circle ---

    #[test]
    fn circle_samples_start_at_stroke_angle() {
        // Start angle = top of circle (-pi/2 in screen coords: y-down, so y = center.y - r)
        let shape = CorrectedShape::Circle {
            center: Point::new(100.0, 100.0),
            radius: 50.0,
            start_angle: -PI / 2.0,
            clockwise: false,
        };
        let pts = corrected_samples(&shape);

        let first = pts[0];
        assert!(
            (first.x - 100.0).abs() < 1.0 && (first.y - 50.0).abs() < 1.0,
            "first point should be at top of circle (100, 50), got ({:.1}, {:.1})",
            first.x,
            first.y
        );
    }

    #[test]
    fn circle_samples_cw_moves_downward_from_right() {
        // CW from rightmost point: second sample should have positive y (screen down)
        let shape = CorrectedShape::Circle {
            center: Point::new(0.0, 0.0),
            radius: 100.0,
            start_angle: 0.0,
            clockwise: true,
        };
        let pts = corrected_samples(&shape);

        assert!(
            pts[1].y > 0.0,
            "CW from right should go downward next, got y = {:.2}",
            pts[1].y
        );
    }

    #[test]
    fn circle_samples_ccw_moves_upward_from_right() {
        // CCW from rightmost point: second sample should have negative y (screen up)
        let shape = CorrectedShape::Circle {
            center: Point::new(0.0, 0.0),
            radius: 100.0,
            start_angle: 0.0,
            clockwise: false,
        };
        let pts = corrected_samples(&shape);

        assert!(
            pts[1].y < 0.0,
            "CCW from right should go upward next, got y = {:.2}",
            pts[1].y
        );
    }

    #[test]
    fn circle_samples_first_and_last_coincide() {
        // The corrected circle is a closed loop regardless of start angle or direction
        let shape = CorrectedShape::Circle {
            center: Point::new(0.0, 0.0),
            radius: 50.0,
            start_angle: 1.23,
            clockwise: true,
        };
        let pts = corrected_samples(&shape);
        let first = *pts.first().unwrap();
        let last = *pts.last().unwrap();

        assert!(
            first.distance(last) < 0.01,
            "first and last circle points should coincide, distance = {:.4}",
            first.distance(last)
        );
    }
}
