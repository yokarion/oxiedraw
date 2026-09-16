//! Crop tool geometry: hit-testing, drag-to-rect, aspect lock, snapping, cursor.
//!
//! A ratio-locked drag is driven by a single size value, and every moving edge
//! is a linear function of it. Snapping solves that value for the edge closest
//! to a canvas border, so the edge lands exactly and the ratio still holds.

use oxiedraw_core::tools::{CropAspectRatio, CropHandle, CropRect};
use oxiedraw_utils::geometry::{Point, Size};

const HANDLE_R: f32 = 10.0;
const SNAP_WIDGET_PX: f32 = 8.0;


#[derive(Clone, Copy)]
enum Axis {
    X,
    Y,
}

impl Axis {
    const fn cross(self) -> Self {
        match self {
            Self::X => Self::Y,
            Self::Y => Self::X,
        }
    }
}

/// Canvas borders that pull nearby crop edges onto them. The rect may still
/// extend past the canvas; this is snapping, not clamping.
#[derive(Clone, Copy)]
pub(super) struct CanvasSnap {
    width: f32,
    height: f32,
    threshold: f32,
}

impl CanvasSnap {
    pub(super) fn new(canvas_size: Size, zoom: f32) -> Self {
        #[allow(clippy::cast_precision_loss)]
        let (width, height) = (canvas_size.width as f32, canvas_size.height as f32);
        Self {
            width,
            height,
            threshold: SNAP_WIDGET_PX / zoom.max(f32::EPSILON),
        }
    }

    // Signed offset from `value` to the nearest border in reach.
    fn pull(self, axis: Axis, value: f32) -> Option<f32> {
        let far = match axis {
            Axis::X => self.width,
            Axis::Y => self.height,
        };
        nearest([-value, far - value].into_iter().filter(|offset| offset.abs() < self.threshold))
    }
}

/// A moving edge of a ratio-locked rect, sitting at `base + slope * size`.
struct LockedEdge {
    axis: Axis,
    base: f32,
    slope: f32,
}


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

/// The ratio a drag is locked to: the preset, or under Free with Shift held,
/// the shape the rect had when the drag started (a square for a new rect).
pub(super) fn drag_ratio(
    preset: CropAspectRatio,
    shift: bool,
    handle: CropHandle,
    start_rect: Option<CropRect>,
) -> Option<f32> {
    let locked = start_rect.map_or_else(|| preset.ratio(), |rect| preset.oriented_to(rect));
    if locked.is_some() || !shift {
        return locked;
    }
    match (handle, start_rect.map(CropRect::normalized)) {
        (CropHandle::NewRect, _) => Some(1.0),
        (_, Some(start)) if start.w > 0.0 && start.h > 0.0 => Some(start.w / start.h),
        _ => None,
    }
}

/// The rect produced by dragging `handle` from `start` to `cur`, where
/// `start_rect` is the crop rect at drag start.
pub(super) fn drag_rect(
    handle: CropHandle,
    start_rect: Option<CropRect>,
    start: Point,
    cur: Point,
    ratio: Option<f32>,
    snap: Option<CanvasSnap>,
) -> Option<CropRect> {
    if handle == CropHandle::NewRect {
        return Some(drag_corner(start, cur, ratio, snap));
    }
    let n = start_rect?.normalized();
    // Handles move by the pointer delta so they keep their grab offset.
    let (dx, dy) = (cur.x - start.x, cur.y - start.y);
    let (left, top, right, bottom) = (n.x, n.y, n.right(), n.bottom());
    let corner = |anchor_x: f32, anchor_y: f32, x: f32, y: f32| {
        drag_corner(Point::new(anchor_x, anchor_y), Point::new(x + dx, y + dy), ratio, snap)
    };

    let rect = match handle {
        CropHandle::None | CropHandle::NewRect => return start_rect,
        CropHandle::Move => move_rect(n, dx, dy, snap),
        CropHandle::TopLeft => corner(right, bottom, left, top),
        CropHandle::TopRight => corner(left, bottom, right, top),
        CropHandle::BottomLeft => corner(right, top, left, bottom),
        CropHandle::BottomRight => corner(left, top, right, bottom),
        CropHandle::TopMid => drag_edge(n, Axis::Y, bottom, top + dy, ratio, snap),
        CropHandle::BottomMid => drag_edge(n, Axis::Y, top, bottom + dy, ratio, snap),
        CropHandle::MidLeft => drag_edge(n, Axis::X, right, left + dx, ratio, snap),
        CropHandle::MidRight => drag_edge(n, Axis::X, left, right + dx, ratio, snap),
    };
    Some(rect)
}

fn drag_corner(
    anchor: Point,
    corner: Point,
    ratio: Option<f32>,
    snap: Option<CanvasSnap>,
) -> CropRect {
    let Some(ratio) = ratio else {
        let x = snapped(snap, Axis::X, corner.x);
        let y = snapped(snap, Axis::Y, corner.y);
        return CropRect::new(anchor.x, anchor.y, x - anchor.x, y - anchor.y);
    };

    let (dx, dy) = (corner.x - anchor.x, corner.y - anchor.y);
    let (sign_x, sign_y) = (dx.signum(), dy.signum());
    // Project onto the ratio diagonal so movement on either axis resizes.
    let height = (dx.abs() * ratio + dy.abs()) / (ratio * ratio + 1.0);
    let edges = [
        LockedEdge { axis: Axis::X, base: anchor.x, slope: sign_x * ratio },
        LockedEdge { axis: Axis::Y, base: anchor.y, slope: sign_y },
    ];
    let height = snap_locked(snap, &edges, height);
    CropRect::new(anchor.x, anchor.y, sign_x * ratio * height, sign_y * height)
}

// Free keeps the cross axis as is; a lock resizes it around its centre.
fn drag_edge(
    n: CropRect,
    axis: Axis,
    anchor: f32,
    edge: f32,
    ratio: Option<f32>,
    snap: Option<CanvasSnap>,
) -> CropRect {
    let (cross_start, cross_len) = match axis {
        Axis::X => (n.y, n.h),
        Axis::Y => (n.x, n.w),
    };

    let (along, cross_start, cross_len) = ratio.map_or_else(
        || (snapped(snap, axis, edge) - anchor, cross_start, cross_len),
        |ratio| {
            let cross_per_len = match axis {
                Axis::X => ratio.recip(),
                Axis::Y => ratio,
            };
            let sign = (edge - anchor).signum();
            let centre = cross_start + cross_len / 2.0;
            let half = cross_per_len / 2.0;
            let edges = [
                LockedEdge { axis, base: anchor, slope: sign },
                LockedEdge { axis: axis.cross(), base: centre, slope: -half },
                LockedEdge { axis: axis.cross(), base: centre, slope: half },
            ];
            let len = snap_locked(snap, &edges, (edge - anchor).abs());
            (sign * len, centre - half * len, cross_per_len * len)
        },
    );

    match axis {
        Axis::X => CropRect::new(anchor, cross_start, along, cross_len),
        Axis::Y => CropRect::new(cross_start, anchor, cross_len, along),
    }
}

// Snapping shifts the whole rect, so its size never changes.
fn move_rect(n: CropRect, dx: f32, dy: f32, snap: Option<CanvasSnap>) -> CropRect {
    let (x, y) = (n.x + dx, n.y + dy);
    let Some(snap) = snap else {
        return CropRect::new(x, y, n.w, n.h);
    };
    let offset = |axis: Axis, near: f32, far: f32| {
        nearest([snap.pull(axis, near), snap.pull(axis, far)].into_iter().flatten()).unwrap_or(0.0)
    };
    CropRect::new(
        x + offset(Axis::X, x, x + n.w),
        y + offset(Axis::Y, y, y + n.h),
        n.w,
        n.h,
    )
}

fn snapped(snap: Option<CanvasSnap>, axis: Axis, value: f32) -> f32 {
    value + snap.and_then(|snap| snap.pull(axis, value)).unwrap_or(0.0)
}

// Resize so the edge closest to a border lands on it; the others follow the lock.
fn snap_locked(snap: Option<CanvasSnap>, edges: &[LockedEdge], size: f32) -> f32 {
    let Some(snap) = snap else {
        return size;
    };
    edges
        .iter()
        .filter_map(|edge| {
            let offset = snap.pull(edge.axis, edge.base + edge.slope * size)?;
            let resized = size + offset / edge.slope;
            (resized > 0.0).then_some((offset.abs(), resized))
        })
        .min_by(|a, b| a.0.total_cmp(&b.0))
        .map_or(size, |(_, resized)| resized)
}

fn nearest(offsets: impl Iterator<Item = f32>) -> Option<f32> {
    offsets.min_by(|a, b| a.abs().total_cmp(&b.abs()))
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


#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f32 = 1e-3;

    // Drags on a 1000 x 800 canvas at zoom 1, snapping on.
    fn drag(
        handle: CropHandle,
        start_rect: Option<CropRect>,
        from: (f32, f32),
        to: (f32, f32),
        ratio: Option<f32>,
    ) -> CropRect {
        let snap = CanvasSnap::new(Size { width: 1000, height: 800 }, 1.0);
        let from = Point::new(from.0, from.1);
        let to = Point::new(to.0, to.1);
        drag_rect(handle, start_rect, from, to, ratio, Some(snap))
            .expect("drag with a start rect yields a rect")
            .normalized()
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < EPS
    }

    fn has_ratio(rect: CropRect, ratio: f32) -> bool {
        near(rect.w / rect.h, ratio)
    }

    fn ratio_is(ratio: Option<f32>, want: f32) -> bool {
        ratio.is_some_and(|ratio| near(ratio, want))
    }

    #[test]
    fn locked_corner_snap_keeps_ratio() {
        let start = CropRect::new(100.0, 100.0, 400.0, 225.0);
        // Dragged along the diagonal to 4px short of the right border.
        let rect = drag(
            CropHandle::BottomRight,
            Some(start),
            (500.0, 325.0),
            (996.0, 604.0),
            Some(16.0 / 9.0),
        );
        assert!(near(rect.right(), 1000.0), "{rect:?}");
        assert!(has_ratio(rect, 16.0 / 9.0), "{rect:?}");
        assert!(near(rect.x, 100.0) && near(rect.y, 100.0), "{rect:?}");
    }

    #[test]
    fn locked_corner_keeps_opposite_corner() {
        let start = CropRect::new(200.0, 200.0, 400.0, 300.0);
        let rect = drag(
            CropHandle::TopLeft,
            Some(start),
            (200.0, 200.0),
            (150.0, 180.0),
            Some(4.0 / 3.0),
        );
        assert!(near(rect.right(), 600.0) && near(rect.bottom(), 500.0), "{rect:?}");
        assert!(has_ratio(rect, 4.0 / 3.0), "{rect:?}");
    }

    #[test]
    fn free_corner_snaps_axes_independently() {
        let rect = drag(CropHandle::NewRect, None, (300.0, 300.0), (996.0, 795.0), None);
        assert!(near(rect.right(), 1000.0) && near(rect.bottom(), 800.0), "{rect:?}");
    }

    #[test]
    fn locked_edge_grows_around_centre_and_snaps() {
        let start = CropRect::new(300.0, 100.0, 400.0, 400.0);
        let rect =
            drag(CropHandle::BottomMid, Some(start), (500.0, 500.0), (500.0, 796.0), Some(1.0));
        assert!(near(rect.bottom(), 800.0), "{rect:?}");
        assert!(near(rect.x + rect.w / 2.0, 500.0), "{rect:?}");
        assert!(has_ratio(rect, 1.0), "{rect:?}");
    }

    #[test]
    fn locked_drags_still_match_the_preset() {
        let preset = CropAspectRatio::SixteenNine;
        let start = CropRect::new(100.0, 100.0, 320.0, 180.0);
        let ratio = drag_ratio(preset, false, CropHandle::MidRight, Some(start));
        for (handle, from, to) in [
            (CropHandle::MidRight, (420.0, 190.0), (637.3, 211.9)),
            (CropHandle::TopMid, (260.0, 100.0), (251.0, 3.7)),
            (CropHandle::BottomLeft, (100.0, 280.0), (997.1, 603.3)),
        ] {
            let rect = drag(handle, Some(start), from, to, ratio);
            assert!(preset.matches(rect), "{handle:?} -> {rect:?}");
        }
    }

    #[test]
    fn move_snap_keeps_size() {
        let start = CropRect::new(100.0, 100.0, 300.0, 200.0);
        let rect = drag(CropHandle::Move, Some(start), (200.0, 200.0), (105.0, 200.0), None);
        assert!(near(rect.x, 0.0) && near(rect.w, 300.0) && near(rect.h, 200.0), "{rect:?}");
    }

    #[test]
    fn shift_locks_free_to_start_shape() {
        let start = Some(CropRect::new(0.0, 0.0, 300.0, 100.0));
        let free = CropAspectRatio::Free;
        assert!(drag_ratio(free, false, CropHandle::BottomRight, start).is_none());
        assert!(ratio_is(drag_ratio(free, true, CropHandle::BottomRight, start), 3.0));
        assert!(ratio_is(drag_ratio(free, true, CropHandle::NewRect, start), 1.0));
    }

    #[test]
    fn preset_follows_start_orientation() {
        let portrait = Some(CropRect::new(0.0, 0.0, 90.0, 160.0));
        let ratio = drag_ratio(CropAspectRatio::SixteenNine, false, CropHandle::TopMid, portrait);
        assert!(ratio_is(ratio, 9.0 / 16.0));
    }
}
