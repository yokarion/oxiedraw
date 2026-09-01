//! Value noise, seeded from the same hash the [`crate::rng`] uses.
//!
//! Patterns use this for variation along the curve rather than per element:
//! per-element jitter alone reads as noise, noise along the curve as intent.

use crate::rng::{hash_unit, mix64};

#[inline]
fn lattice(seed: u64, i: i64) -> f32 {
    hash_unit(mix64(seed ^ (i as u64).wrapping_mul(0x2545_F491_4F6C_DD1D)))
}

/// Cubic smoothstep, `3t^2 - 2t^3`.
#[inline]
pub fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[inline]
pub fn smoothstep_range(edge0: f32, edge1: f32, x: f32) -> f32 {
    if (edge1 - edge0).abs() < f32::EPSILON {
        return f32::from(x >= edge1);
    }
    smoothstep((x - edge0) / (edge1 - edge0))
}

/// 1D value noise in `0.0..1.0`, one unit per lattice cell.
#[must_use]
pub fn value_1d(seed: u64, x: f32) -> f32 {
    let cell = x.floor();
    let t = x - cell;
    let i = cell as i64;
    let a = lattice(seed, i);
    let b = lattice(seed, i + 1);
    a + (b - a) * smoothstep(t)
}

/// Fractal value noise in `0.0..1.0`: `octaves` of [`value_1d`], each double
/// the frequency and half the amplitude of the last.
#[must_use]
pub fn fbm_1d(seed: u64, x: f32, octaves: u32) -> f32 {
    let mut sum = 0.0;
    let mut amplitude = 1.0;
    let mut total = 0.0;
    let mut frequency = 1.0;
    for octave in 0..octaves.max(1) {
        sum += value_1d(seed ^ (u64::from(octave) << 32), x * frequency) * amplitude;
        total += amplitude;
        amplitude *= 0.5;
        frequency *= 2.0;
    }
    if total <= 0.0 { 0.0 } else { sum / total }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_noise_stays_in_range() {
        for i in 0..2000 {
            let x = (i as f32) * 0.37 - 300.0;
            let v = value_1d(42, x);
            assert!((0.0..=1.0).contains(&v), "value_1d({x}) = {v}");
            let f = fbm_1d(42, x, 3);
            assert!((0.0..=1.0).contains(&f), "fbm_1d({x}) = {f}");
        }
    }

    #[test]
    fn value_noise_is_continuous() {
        let mut prev = value_1d(9, -5.0);
        let mut x = -5.0;
        while x < 5.0 {
            x += 0.01;
            let v = value_1d(9, x);
            assert!((v - prev).abs() < 0.05, "jump of {} at x = {x}", v - prev);
            prev = v;
        }
    }

    #[test]
    fn lattice_points_are_hit_exactly() {
        // At integer x the interpolation weight is 0.
        for i in -20..20 {
            let x = i as f32;
            assert!((value_1d(3, x) - lattice(3, i64::from(i))).abs() < 1e-6);
        }
    }

    #[test]
    fn different_seeds_give_different_fields() {
        let same = (0..500)
            .filter(|&i| {
                let x = i as f32 * 0.5;
                (value_1d(1, x) - value_1d(2, x)).abs() < 1e-6
            })
            .count();
        assert!(same < 5, "seeds 1 and 2 agree on {same} of 500 samples");
    }

    #[test]
    fn smoothstep_range_handles_degenerate_edges() {
        assert!((smoothstep_range(1.0, 1.0, 2.0) - 1.0).abs() < f32::EPSILON);
        assert!(smoothstep_range(1.0, 1.0, 0.0).abs() < f32::EPSILON);
    }
}
