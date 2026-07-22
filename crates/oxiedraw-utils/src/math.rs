//! Small scalar helpers reused across pixel and color math.

/// Clamp a float to the unit interval `[0, 1]`.
#[inline]
#[must_use]
pub fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}

/// Linear interpolation: `t = 0` returns `a`, `t = 1` returns `b`.
#[inline]
#[must_use]
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    (b - a).mul_add(t, a)
}

/// Wrap an angle (radians) into `(-pi, pi]`, so accumulated rotation takes the
/// short way around and crosses the atan2 branch cut smoothly.
#[inline]
#[must_use]
pub fn wrap_pi(a: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let mut a = a % TAU;
    if a <= -PI {
        a += TAU;
    } else if a > PI {
        a -= TAU;
    }
    a
}
