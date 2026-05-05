//! Crop tool geometry: hit-testing, rect computation, constraints, cursor.

use oxiedraw_core::tools::{CropAspectRatio, CropHandle, CropRect};
use oxiedraw_utils::geometry::{Point, Size};

const HANDLE_R: f32 = 10.0;
const SNAP_WIDGET_PX: f32 = 8.0;

/// Hit-test widget-space coordinates against the crop handles.
pub(super) fn hit_test_widget(rect: Option<(f32, f32, f32, f32)>, wx: f32, wy: f32) -> CropHandle {
    let Some((x1, y1, x2, y2)) = rect else {
        return CropHandle::NewRect;
    };

    let mx = f32::midpoint(x1, x2);
    let my = f32::midpoint(y1, y2);
    let near = |ax: f32, ay: f32| (wx - ax).abs() < HANDLE_R && (wy - ay).abs() < HANDLE_R;

    if near(x1, y1) {
        return CropHandle::TopLeft;
    }
    if near(x2, y1) {
        return CropHandle::TopRight;
    }
    if near(x1, y2) {
        return CropHandle::BottomLeft;
    }
    if near(x2, y2) {
        return CropHandle::BottomRight;
    }
    if near(mx, y1) {
        return CropHandle::TopMid;
    }
    if near(mx, y2) {
        return CropHandle::BottomMid;
    }
    if near(x1, my) {
        return CropHandle::MidLeft;
    }
    if near(x2, my) {
        return CropHandle::MidRight;
    }

    let min_x = x1.min(x2);
    let max_x = x1.max(x2);
    let min_y = y1.min(y2);
    let max_y = y1.max(y2);
    if wx >= min_x && wx <= max_x && wy >= min_y && wy <= max_y {
        return CropHandle::Move;
    }

    CropHandle::NewRect
}

pub(super) fn compute_new_rect(
    handle: CropHandle,
    old: Option<CropRect>,
    start: Point,
    cx: f32,
    cy: f32,
) -> Option<CropRect> {
    match handle {
        CropHandle::NewRect => Some(CropRect::new(start.x, start.y, cx - start.x, cy - start.y)),
        CropHandle::Move => {
            let r = old?.normalized();
            Some(CropRect::new(
                r.x + (cx - start.x),
                r.y + (cy - start.y),
                r.w,
                r.h,
            ))
        }
        CropHandle::TopLeft => {
            let n = old?.normalized();
            let (br_x, br_y) = (n.right(), n.bottom());
            Some(CropRect::new(cx, cy, br_x - cx, br_y - cy))
        }
        CropHandle::TopRight => {
            let n = old?.normalized();
            let (bl_x, br_y) = (n.x, n.bottom());
            Some(CropRect::new(bl_x, cy, cx - bl_x, br_y - cy))
        }
        CropHandle::BottomLeft => {
            let n = old?.normalized();
            let (br_x, tl_y) = (n.right(), n.y);
            Some(CropRect::new(cx, tl_y, br_x - cx, cy - tl_y))
        }
        CropHandle::BottomRight => {
            let n = old?.normalized();
            let (tl_x, tl_y) = (n.x, n.y);
            Some(CropRect::new(tl_x, tl_y, cx - tl_x, cy - tl_y))
        }
        CropHandle::TopMid => {
            let n = old?.normalized();
            let bot_y = n.bottom();
            Some(CropRect::new(n.x, cy, n.w, bot_y - cy))
        }
        CropHandle::BottomMid => {
            let n = old?.normalized();
            Some(CropRect::new(n.x, n.y, n.w, cy - n.y))
        }
        CropHandle::MidLeft => {
            let n = old?.normalized();
            let right_x = n.right();
            Some(CropRect::new(cx, n.y, right_x - cx, n.h))
        }
        CropHandle::MidRight => {
            let n = old?.normalized();
            Some(CropRect::new(n.x, n.y, cx - n.x, n.h))
        }
        CropHandle::None => old,
    }
}

pub(super) fn constrain_rect(
    rect: Option<CropRect>,
    ratio: CropAspectRatio,
    handle: CropHandle,
) -> Option<CropRect> {
    let Some(r) = ratio.ratio() else {
        return rect;
    };
    let rect = rect?;
    let n = rect.normalized();

    let (new_w, new_h) = match handle {
        CropHandle::Move | CropHandle::None => (n.w, n.h),
        CropHandle::TopMid | CropHandle::BottomMid => (n.h * r, n.h),
        CropHandle::MidLeft | CropHandle::MidRight => (n.w, n.w / r),
        _ => {
            let h_from_w = n.w / r;
            if h_from_w <= n.h {
                (n.w, h_from_w)
            } else {
                (n.h * r, n.h)
            }
        }
    };

    Some(CropRect::new(n.x, n.y, new_w, new_h))
}

/// Snap each edge of `rect` to the canvas boundary when within
/// `SNAP_WIDGET_PX` (in widget pixels, converted via `zoom`). Unlike
/// clamping, this allows the rect to extend beyond the canvas.
pub(super) fn snap_rect_to_canvas(rect: CropRect, canvas_size: Size, zoom: f32) -> CropRect {
    let threshold = SNAP_WIDGET_PX / zoom.max(f32::EPSILON);
    let n = rect.normalized();
    #[allow(clippy::cast_precision_loss)]
    let (cw, ch) = (canvas_size.width as f32, canvas_size.height as f32);

    let mut x = n.x;
    let mut y = n.y;
    let mut r = n.right();
    let mut b = n.bottom();

    if x.abs() < threshold {
        x = 0.0;
    }
    if y.abs() < threshold {
        y = 0.0;
    }
    if (r - cw).abs() < threshold {
        r = cw;
    }
    if (b - ch).abs() < threshold {
        b = ch;
    }

    CropRect::new(x, y, r - x, b - y)
}

pub(super) const fn cursor_name(handle: CropHandle) -> &'static str {
    match handle {
        CropHandle::None | CropHandle::NewRect => "crosshair",
        CropHandle::Move => "move",
        CropHandle::TopLeft | CropHandle::BottomRight => "nwse-resize",
        CropHandle::TopRight | CropHandle::BottomLeft => "nesw-resize",
        CropHandle::TopMid | CropHandle::BottomMid => "ns-resize",
        CropHandle::MidLeft | CropHandle::MidRight => "ew-resize",
    }
}
