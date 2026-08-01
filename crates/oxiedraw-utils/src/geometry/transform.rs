//! Affine transform box and the sampling filter used when remapping pixels.

/// Pixel sampling filter for affine remap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransformFilter {
    #[default]
    Bilinear,
    NearestNeighbor,
}

impl TransformFilter {
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Bilinear => "Bilinear",
            Self::NearestNeighbor => "Nearest Neighbor",
        }
    }
}

/// A 2-D affine transform box, defined by center, width, height, and a
/// rotation angle (radians, positive = clockwise on screen).
#[derive(Clone, Copy, PartialEq)]
pub struct TransformRect {
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub angle: f32,
}

impl std::fmt::Debug for TransformRect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "TransformRect(c=({},{}) {}x{} a={:.1}deg)",
            self.cx,
            self.cy,
            self.w,
            self.h,
            self.angle.to_degrees()
        )
    }
}

impl TransformRect {
    #[inline]
    pub const fn new(cx: f32, cy: f32, w: f32, h: f32, angle: f32) -> Self {
        Self {
            cx,
            cy,
            w,
            h,
            angle,
        }
    }

    #[inline]
    pub fn half_w(self) -> f32 {
        self.w / 2.0
    }

    #[inline]
    pub fn half_h(self) -> f32 {
        self.h / 2.0
    }

    /// Map a local-space offset (origin at box centre) to outer coords.
    #[must_use]
    pub fn local_to_canvas(self, lx: f32, ly: f32) -> (f32, f32) {
        let (sa, ca) = self.angle.sin_cos();
        (
            ly.mul_add(-sa, lx.mul_add(ca, self.cx)),
            ly.mul_add(ca, lx.mul_add(sa, self.cy)),
        )
    }

    /// Inverse of [`Self::local_to_canvas`]: map outer coords to a local-space
    /// offset (origin at box centre).
    #[must_use]
    pub fn canvas_to_local(self, x: f32, y: f32) -> (f32, f32) {
        let (sa, ca) = self.angle.sin_cos();
        let dx = x - self.cx;
        let dy = y - self.cy;
        (dx.mul_add(ca, dy * sa), dy.mul_add(ca, -dx * sa))
    }

    /// Remap `self` (a rect in the same outer space as `from`) through the
    /// affine that carries `from` onto `to`. Used to move a layer's box or
    /// placement rigidly with a shared group transform: centre and orientation
    /// follow exactly; the size scales by the `from -> to` ratio, which is exact
    /// when `self` is axis-aligned with `from` (the common `from.angle == 0`
    /// union-bounds case) and a close approximation otherwise.
    #[must_use]
    pub fn remap(self, from: Self, to: Self) -> Self {
        let sx = if from.w.abs() > 1e-6 { to.w / from.w } else { 1.0 };
        let sy = if from.h.abs() > 1e-6 { to.h / from.h } else { 1.0 };
        let (lx, ly) = from.canvas_to_local(self.cx, self.cy);
        let (ncx, ncy) = to.local_to_canvas(lx * sx, ly * sy);
        Self::new(ncx, ncy, self.w * sx, self.h * sy, self.angle + (to.angle - from.angle))
    }
}

#[cfg(test)]
mod tests {
    use super::TransformRect;

    #[test]
    fn canvas_local_round_trip_rotated() {
        let rect = TransformRect::new(100.0, 50.0, 40.0, 20.0, 0.7);
        for (lx, ly) in [(0.0, 0.0), (12.0, -8.0), (-20.0, 9.0)] {
            let (cx, cy) = rect.local_to_canvas(lx, ly);
            let (blx, bly) = rect.canvas_to_local(cx, cy);
            assert!((blx - lx).abs() < 1e-3, "lx {lx} -> {blx}");
            assert!((bly - ly).abs() < 1e-3, "ly {ly} -> {bly}");
        }
        // Local origin maps to the box centre.
        let (ox, oy) = rect.local_to_canvas(0.0, 0.0);
        assert!((ox - 100.0).abs() < 1e-3 && (oy - 50.0).abs() < 1e-3);
    }

    #[test]
    fn remap_identity_returns_self() {
        let from = TransformRect::new(50.0, 40.0, 20.0, 10.0, 0.0);
        let g = TransformRect::new(60.0, 30.0, 8.0, 4.0, 0.3);
        let out = g.remap(from, from);
        assert!((out.cx - g.cx).abs() < 1e-3 && (out.cy - g.cy).abs() < 1e-3);
        assert!((out.w - g.w).abs() < 1e-3 && (out.h - g.h).abs() < 1e-3);
        assert!((out.angle - g.angle).abs() < 1e-3);
    }

    #[test]
    fn remap_translate_and_scale_group() {
        // Union box moves by (+100, 0) and doubles in width.
        let from = TransformRect::new(0.0, 0.0, 10.0, 10.0, 0.0);
        let to = TransformRect::new(100.0, 0.0, 20.0, 10.0, 0.0);
        // A member sitting at the union's right edge follows rigidly: its centre
        // x-offset (5) scales by 2 -> 10, plus the +100 translation.
        let member = TransformRect::new(5.0, 0.0, 4.0, 4.0, 0.0);
        let out = member.remap(from, to);
        assert!((out.cx - 110.0).abs() < 1e-3, "cx {}", out.cx);
        assert!((out.cy - 0.0).abs() < 1e-3, "cy {}", out.cy);
        assert!((out.w - 8.0).abs() < 1e-3, "w {}", out.w);
        assert!((out.h - 4.0).abs() < 1e-3, "h {}", out.h);
    }

    #[test]
    fn remap_rotation_adds_angle() {
        let from = TransformRect::new(0.0, 0.0, 10.0, 10.0, 0.0);
        let to = TransformRect::new(0.0, 0.0, 10.0, 10.0, 0.5);
        let member = TransformRect::new(3.0, 0.0, 2.0, 2.0, 0.1);
        let out = member.remap(from, to);
        assert!((out.angle - 0.6).abs() < 1e-3, "angle {}", out.angle);
        // Centre rotates about the shared origin by 0.5 rad.
        let (sa, ca) = 0.5_f32.sin_cos();
        assert!((out.cx - 3.0 * ca).abs() < 1e-3 && (out.cy - 3.0 * sa).abs() < 1e-3);
    }
}
