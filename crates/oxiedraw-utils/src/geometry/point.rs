#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    #[inline]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    #[inline]
    pub fn distance(self, other: Self) -> f32 {
        (self.x - other.x).hypot(self.y - other.y)
    }

    #[inline]
    #[must_use]
    pub fn normalize(self) -> Self {
        let len = self.x.hypot(self.y);
        if len < f32::EPSILON {
            return Self::ZERO;
        }
        Self::new(self.x / len, self.y / len)
    }

    /// Linearly interpolate toward `other`. `t = 0` returns `self`, `t = 1` returns `other`.
    #[inline]
    #[must_use]
    pub fn lerp(self, other: Self, t: f32) -> Self {
        Self::new(
            (other.x - self.x).mul_add(t, self.x),
            (other.y - self.y).mul_add(t, self.y),
        )
    }

    pub fn perp_distance_to_line(self, a: Self, b: Self) -> f32 {
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let len = dx.hypot(dy);
        if len < f32::EPSILON {
            return self.distance(a);
        }
        (self.x - a.x).mul_add(dy, -((self.y - a.y) * dx)).abs() / len
    }
}
