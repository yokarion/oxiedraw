//! Drag-to-bounding-box resolution for the shape tools.
//!
//! The shape tools rasterise entirely on the GPU (see the `shape_overlay`
//! renderer module + `shaders/shape.frag`); the only CPU-side geometry left
//! is turning a pointer drag into the bounding box / endpoints the shader
//! consumes, including the shift / alt modifier behaviour.

use crate::tools::ShapeTool;
use oxiedraw_utils::geometry::Point;

/// Stroke width (canvas pixels) used by the Line shape until per-shape
/// options exist.
pub const DEFAULT_LINE_WIDTH: f32 = 4.0;

/// Resolve a shape-tool drag into a bounding box `(x, y, w, h)` in canvas
/// pixels (`w`/`h` may be negative). `alt` makes `start` the centre so the
/// box grows symmetrically out from the initial cursor position.
///
/// `shift` constrains the drag: for a box shape it forces a 1:1 aspect ratio
/// (square); for [`ShapeTool::Line`] it snaps the segment angle to the
/// nearest 45deg increment (horizontal / vertical / both diagonals) while
/// preserving the drag length.
#[must_use]
pub fn shape_rect_from_drag(
    start: Point,
    cur: Point,
    kind: ShapeTool,
    shift: bool,
    alt: bool,
) -> (f32, f32, f32, f32) {
    let mut dx = cur.x - start.x;
    let mut dy = cur.y - start.y;
    if shift {
        if matches!(kind, ShapeTool::Line) {
            let len = dx.hypot(dy);
            let step = std::f32::consts::FRAC_PI_4;
            let snapped = (dy.atan2(dx) / step).round() * step;
            dx = len * snapped.cos();
            dy = len * snapped.sin();
        } else {
            let m = dx.abs().max(dy.abs());
            dx = m.copysign(dx);
            dy = m.copysign(dy);
        }
    }
    if alt {
        (start.x - dx, start.y - dy, dx * 2.0, dy * 2.0)
    } else {
        (start.x, start.y, dx, dy)
    }
}

#[cfg(test)]
// Exact compares are deliberate: fract/signum and round-tripped literals are
// exact by construction. Approximate checks nearby use an epsilon.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use std::f32::consts::FRAC_PI_4;

    #[test]
    fn drag_without_modifiers_is_identity() {
        let s = Point::new(10.0, 20.0);
        let c = Point::new(40.0, 60.0);
        let r = shape_rect_from_drag(s, c, ShapeTool::Rectangle, false, false);
        assert_eq!(r, (10.0, 20.0, 30.0, 40.0));
    }

    #[test]
    fn shift_on_box_forces_square_preserving_signs() {
        let s = Point::new(0.0, 0.0);
        let c = Point::new(100.0, -20.0);
        let (_x, _y, w, h) = shape_rect_from_drag(s, c, ShapeTool::Rectangle, true, false);
        assert!((w.abs() - h.abs()).abs() < 1e-3, "expected square, got {w}x{h}");
        assert_eq!(w.signum(), 1.0);
        assert_eq!(h.signum(), -1.0);
        // larger axis wins.
        assert!((w.abs() - 100.0).abs() < 1e-3);
    }

    #[test]
    fn alt_makes_start_the_center() {
        let s = Point::new(50.0, 50.0);
        let c = Point::new(60.0, 80.0);
        let (x, y, w, h) = shape_rect_from_drag(s, c, ShapeTool::Rectangle, false, true);
        // (x, y) shifts by -delta so the rect spans symmetrically about start.
        assert_eq!((x, y, w, h), (40.0, 20.0, 20.0, 60.0));
        // start is the centre of the resulting rect.
        assert!((x + w * 0.5 - s.x).abs() < 1e-3);
        assert!((y + h * 0.5 - s.y).abs() < 1e-3);
    }

    #[test]
    fn shift_on_line_snaps_nearly_horizontal_to_horizontal() {
        let s = Point::new(0.0, 0.0);
        // dx=100, dy=10 -> angle ~5.7deg -> snaps to 0deg (horizontal).
        let c = Point::new(100.0, 10.0);
        let (_, _, w, h) = shape_rect_from_drag(s, c, ShapeTool::Line, true, false);
        assert!(h.abs() < 1e-3, "expected horizontal line, h={h}");
        // length preserved.
        let original_len = (100.0_f32).hypot(10.0);
        assert!((w.abs() - original_len).abs() < 1e-3);
    }

    #[test]
    fn shift_on_line_snaps_nearly_vertical_to_vertical() {
        let s = Point::new(0.0, 0.0);
        let c = Point::new(8.0, -100.0);
        let (_, _, w, h) = shape_rect_from_drag(s, c, ShapeTool::Line, true, false);
        assert!(w.abs() < 1e-3, "expected vertical line, w={w}");
        assert!(h < 0.0);
    }

    #[test]
    fn shift_on_line_keeps_diagonal_diagonal() {
        let s = Point::new(0.0, 0.0);
        // Already at 45deg -> stays at 45deg, length preserved.
        let c = Point::new(70.0, 70.0);
        let (_, _, w, h) = shape_rect_from_drag(s, c, ShapeTool::Line, true, false);
        let snapped_dir = h.atan2(w);
        assert!((snapped_dir - FRAC_PI_4).abs() < 1e-3);
        let len = w.hypot(h);
        assert!((len - 70.0_f32.hypot(70.0)).abs() < 1e-3);
    }
}
