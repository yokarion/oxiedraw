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
}
