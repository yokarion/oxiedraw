//! Transform tool geometry: hit-testing, rect computation, cursor names.

use std::cell::Cell;
use std::rc::Rc;

use oxiedraw_core::tools::TransformHandle;
use oxiedraw_utils::geometry::{Point, TransformRect};

const HANDLE_RADIUS: f32 = 10.0;
const ROTATE_RADIUS: f32 = 12.0;
const ROTATE_DISTANCE: f32 = 28.0;

/// Hit-test widget-space coordinates against transform handles.
pub(super) fn hit_test(
    rect: TransformRect,
    widget_x: f32,
    widget_y: f32,
    pan: &Rc<Cell<Point>>,
    zoom: &Rc<Cell<f32>>,
    rotation: &Rc<Cell<f32>>,
) -> TransformHandle {
    let pan_offset = pan.get();
    let zoom = zoom.get();
    let view = rotation.get();

    // Canvas -> widget for the rect centre: pan + zoom * R(view) * centre.
    let (view_sin, view_cos) = view.sin_cos();
    let scaled_cx = rect.cx * zoom;
    let scaled_cy = rect.cy * zoom;
    let center_x = pan_offset.x + view_cos.mul_add(scaled_cx, -view_sin * scaled_cy);
    let center_y = pan_offset.y + view_sin.mul_add(scaled_cx, view_cos * scaled_cy);
    let half_width = rect.half_w() * zoom;
    let half_height = rect.half_h() * zoom;
    // The view rotation stacks on top of the rect's own angle.
    let (sin_angle, cos_angle) = (rect.angle + view).sin_cos();

    let top_mid_x = center_x + half_height * sin_angle;
    let top_mid_y = center_y - half_height * cos_angle;
    let rotate_x = top_mid_x + sin_angle * ROTATE_DISTANCE;
    let rotate_y = top_mid_y - cos_angle * ROTATE_DISTANCE;

    let near = |anchor_x: f32, anchor_y: f32, radius: f32| {
        (widget_x - anchor_x).hypot(widget_y - anchor_y) < radius
    };

    if near(rotate_x, rotate_y, ROTATE_RADIUS) {
        return TransformHandle::Rotate;
    }

    let local_to_widget = |local_x: f32, local_y: f32| -> (f32, f32) {
        (
            center_x + local_x * cos_angle - local_y * sin_angle,
            center_y + local_x * sin_angle + local_y * cos_angle,
        )
    };

    let (top_left_x, top_left_y) = local_to_widget(-half_width, -half_height);
    let (top_right_x, top_right_y) = local_to_widget(half_width, -half_height);
    let (bottom_left_x, bottom_left_y) = local_to_widget(-half_width, half_height);
    let (bottom_right_x, bottom_right_y) = local_to_widget(half_width, half_height);
    let (top_mid_x, top_mid_y) = local_to_widget(0.0, -half_height);
    let (bottom_mid_x, bottom_mid_y) = local_to_widget(0.0, half_height);
    let (mid_left_x, mid_left_y) = local_to_widget(-half_width, 0.0);
    let (mid_right_x, mid_right_y) = local_to_widget(half_width, 0.0);

    if near(top_left_x, top_left_y, HANDLE_RADIUS) {
        return TransformHandle::TopLeft;
    }
    if near(top_right_x, top_right_y, HANDLE_RADIUS) {
        return TransformHandle::TopRight;
    }
    if near(bottom_left_x, bottom_left_y, HANDLE_RADIUS) {
        return TransformHandle::BottomLeft;
    }
    if near(bottom_right_x, bottom_right_y, HANDLE_RADIUS) {
        return TransformHandle::BottomRight;
    }
    if near(top_mid_x, top_mid_y, HANDLE_RADIUS) {
        return TransformHandle::TopMid;
    }
    if near(bottom_mid_x, bottom_mid_y, HANDLE_RADIUS) {
        return TransformHandle::BottomMid;
    }
    if near(mid_left_x, mid_left_y, HANDLE_RADIUS) {
        return TransformHandle::MidLeft;
    }
    if near(mid_right_x, mid_right_y, HANDLE_RADIUS) {
        return TransformHandle::MidRight;
    }

    let delta_x = widget_x - center_x;
    let delta_y = widget_y - center_y;
    let local_x = delta_x * cos_angle + delta_y * sin_angle;
    let local_y = -delta_x * sin_angle + delta_y * cos_angle;
    if local_x.abs() <= half_width && local_y.abs() <= half_height {
        return TransformHandle::Move;
    }

    TransformHandle::None
}

pub(super) const fn cursor_name(handle: TransformHandle) -> &'static str {
    match handle {
        TransformHandle::None => "default",
        TransformHandle::Move => "move",
        TransformHandle::Rotate => "crosshair",
        TransformHandle::TopLeft | TransformHandle::BottomRight => "nwse-resize",
        TransformHandle::TopRight | TransformHandle::BottomLeft => "nesw-resize",
        TransformHandle::TopMid | TransformHandle::BottomMid => "ns-resize",
        TransformHandle::MidLeft | TransformHandle::MidRight => "ew-resize",
    }
}

/// Recompute the `TransformRect` based on which handle is being dragged.
///
/// `shift` constrains scaling to the box's original aspect ratio; `alt`
/// scales symmetrically about the rect centre instead of pinning the
/// opposite corner/edge. Both mirror the shape-tool modifier behaviour.
pub(super) fn compute_rect(
    handle: TransformHandle,
    start: TransformRect,
    start_canvas: Point,
    start_rotation_angle: f32,
    cur: Point,
    shift: bool,
    alt: bool,
) -> TransformRect {
    let delta = Point::new(cur.x - start_canvas.x, cur.y - start_canvas.y);

    let out = match handle {
        TransformHandle::None => start,

        TransformHandle::Move => TransformRect::new(
            start.cx + delta.x,
            start.cy + delta.y,
            start.w,
            start.h,
            start.angle,
        ),

        TransformHandle::Rotate => {
            let cur_angle = (cur.y - start.cy).atan2(cur.x - start.cx);
            return TransformRect::new(
                start.cx,
                start.cy,
                start.w,
                start.h,
                start.angle + (cur_angle - start_rotation_angle),
            );
        }

        TransformHandle::TopLeft => scale_handle(start, delta, -1.0, -1.0, shift, alt),
        TransformHandle::TopRight => scale_handle(start, delta, 1.0, -1.0, shift, alt),
        TransformHandle::BottomLeft => scale_handle(start, delta, -1.0, 1.0, shift, alt),
        TransformHandle::BottomRight => scale_handle(start, delta, 1.0, 1.0, shift, alt),

        TransformHandle::TopMid => scale_handle(start, delta, 0.0, -1.0, shift, alt),
        TransformHandle::BottomMid => scale_handle(start, delta, 0.0, 1.0, shift, alt),
        TransformHandle::MidLeft => scale_handle(start, delta, -1.0, 0.0, shift, alt),
        TransformHandle::MidRight => scale_handle(start, delta, 1.0, 0.0, shift, alt),
    };

    snap_to_pixel_grid(out)
}

/// Snap the rect's unrotated extents so its top-left, width, and height
/// are integer canvas pixels. Rotation is preserved verbatim; with angle = 0
/// this puts all four corners on the pixel grid.
fn snap_to_pixel_grid(rect: TransformRect) -> TransformRect {
    let half_width = rect.w / 2.0;
    let half_height = rect.h / 2.0;
    let left = (rect.cx - half_width).round();
    let top = (rect.cy - half_height).round();
    let right = (rect.cx + half_width).round();
    let bottom = (rect.cy + half_height).round();
    let width = (right - left).max(1.0);
    let height = (bottom - top).max(1.0);
    TransformRect::new(left + width / 2.0, top + height / 2.0, width, height, rect.angle)
}

/// Scale by dragging a corner or edge handle. `(sign_x, sign_y)` are the
/// handle's local-space signs: `-1`/`+1` select the moving edge along that
/// axis, `0` means that axis is not controlled by this handle (edge handles).
///
/// Default (no modifiers) pins the opposite corner/edge. `alt` pins the
/// rect centre so it grows symmetrically. `shift` preserves the original
/// aspect ratio.
fn scale_handle(
    start: TransformRect,
    delta: Point,
    sign_x: f32,
    sign_y: f32,
    shift: bool,
    alt: bool,
) -> TransformRect {
    let angle = start.angle;
    let (sin_angle, cos_angle) = angle.sin_cos();
    // Drag delta projected onto the rect's local axes.
    let drag_local_x = delta.x * cos_angle + delta.y * sin_angle;
    let drag_local_y = -delta.x * sin_angle + delta.y * cos_angle;
    let start_half_width = start.half_w();
    let start_half_height = start.half_h();

    let (mut half_width, mut center_offset_x) =
        axis_resize(start_half_width, sign_x, drag_local_x, alt);
    let (mut half_height, mut center_offset_y) =
        axis_resize(start_half_height, sign_y, drag_local_y, alt);

    if shift {
        // Keep the box's original aspect ratio: pick one uniform scale factor
        // (driven by the controlled axes) and apply it to both half-extents.
        let scale = match (sign_x != 0.0, sign_y != 0.0) {
            (true, true) => (half_width / start_half_width).max(half_height / start_half_height),
            (true, false) => half_width / start_half_width,
            (false, true) => half_height / start_half_height,
            (false, false) => return start,
        };
        let target_half_width = start_half_width * scale;
        let target_half_height = start_half_height * scale;
        let (scaled_half_width, scaled_center_offset_x) =
            axis_to_target(start_half_width, sign_x, drag_local_x, alt, target_half_width);
        let (scaled_half_height, scaled_center_offset_y) =
            axis_to_target(start_half_height, sign_y, drag_local_y, alt, target_half_height);
        half_width = scaled_half_width;
        center_offset_x = scaled_center_offset_x;
        half_height = scaled_half_height;
        center_offset_y = scaled_center_offset_y;
    }

    let new_width = (half_width * 2.0).max(1.0);
    let new_height = (half_height * 2.0).max(1.0);
    // Rotate the local centre offset back into canvas space.
    let new_center_x = center_offset_x.mul_add(cos_angle, start.cx) - center_offset_y * sin_angle;
    let new_center_y = center_offset_x.mul_add(sin_angle, start.cy) + center_offset_y * cos_angle;
    TransformRect::new(new_center_x, new_center_y, new_width, new_height, angle)
}

/// New half-extent + local centre offset for one axis. `sign == 0` leaves the
/// axis unchanged; otherwise `alt` pins the centre and the default pins the
/// opposite edge (local `-sign * start_half`).
fn axis_resize(start_half: f32, sign: f32, drag: f32, alt: bool) -> (f32, f32) {
    if sign == 0.0 {
        return (start_half, 0.0);
    }
    if alt {
        ((sign * start_half + drag).abs(), 0.0)
    } else {
        let moving = sign * start_half + drag;
        let anchor = -sign * start_half;
        ((moving - anchor).abs() / 2.0, f32::midpoint(moving, anchor))
    }
}

/// Like [`axis_resize`] but forces the half-extent to `target` (constrained
/// scaling), keeping the same anchor semantics.
fn axis_to_target(start_half: f32, sign: f32, drag: f32, alt: bool, target: f32) -> (f32, f32) {
    if sign == 0.0 || alt {
        return (target, 0.0);
    }
    let moving = sign * start_half + drag;
    let anchor = -sign * start_half;
    let direction = (moving - anchor).signum();
    (target, direction.mul_add(target, anchor))
}
