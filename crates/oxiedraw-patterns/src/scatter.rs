//! Where elements sit along the curve.
//!
//! Roots go in fixed buckets of ribbon space, jittered inside them, and a
//! density below 1.0 rejects buckets rather than renumbering them - so turning
//! density down removes elements and leaves every survivor where it was.

use crate::rng::Rng;
use crate::spine::{FrameSample, SpineField};

/// Keeps scatter randomness clear of the channels a generator uses.
const SCATTER_SALT: u64 = 0x5CA7_7E12_D00D_5EED;

const CH_JITTER: u8 = 0;
const CH_DENSITY: u8 = 1;

/// Upper bound on roots from one pass, so a 1 px spacing on a long curve
/// degrades instead of allocating without limit.
pub const MAX_ROOTS: u32 = 20_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Root {
    /// The bucket this root came from. Randomness keyed on this survives edits
    /// to the curve and to unrelated parameters.
    pub id: u32,
    /// Position along the curve in ribbon space.
    pub s: f32,
    pub frame: FrameSample,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScatterSpec {
    /// Nominal gap between roots, in ribbon-space pixels.
    pub spacing: f32,
    /// `0.0` scatters roots freely inside their bucket, `1.0` places them on
    /// exact centres.
    pub regularity: f32,
    /// Added to every bucket index. Give each depth rank and each side its own
    /// offset so they do not draw identical numbers.
    pub id_offset: u32,
}

impl Default for ScatterSpec {
    fn default() -> Self {
        Self {
            spacing: 12.0,
            regularity: 0.35,
            id_offset: 0,
        }
    }
}

/// `density` returns `0..=1` per candidate frame and gates buckets
/// probabilistically.
#[must_use]
pub fn along(
    field: &SpineField,
    spec: &ScatterSpec,
    rng: Rng,
    density: impl Fn(&FrameSample) -> f32,
) -> Vec<Root> {
    let spacing = spec.spacing.max(0.5);
    let rest = field.rest_length();
    if rest <= 0.0 {
        return Vec::new();
    }
    let buckets = (((rest / spacing).floor() as u32) + 1).min(MAX_ROOTS);
    let rng = rng.salted(SCATTER_SALT);
    let regularity = spec.regularity.clamp(0.0, 1.0);
    let mut out = Vec::with_capacity(buckets as usize);
    for bucket in 0..buckets {
        let id = spec.id_offset.wrapping_add(bucket);
        let jitter = rng.signed(id, CH_JITTER) * (1.0 - regularity) * 0.5 * spacing;
        let s = (bucket as f32 + 0.5).mul_add(spacing, jitter);
        if s < 0.0 || s > rest {
            continue;
        }
        let frame = field.sample(s);
        let want = density(&frame);
        if want <= 0.0 || (want < 1.0 && rng.unit(id, CH_DENSITY) >= want) {
            continue;
        }
        out.push(Root { id, s, frame });
    }
    out
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::spine::SpineNode;
    use oxiedraw_utils::geometry::Point;

    fn field(len: f32) -> SpineField {
        let nodes = vec![
            SpineNode::new(Point::new(0.0, 0.0), 0.0),
            SpineNode::new(Point::new(len, 0.0), 1.0),
        ];
        SpineField::new(&nodes, 1.0, None).expect("field")
    }

    #[test]
    fn spacing_sets_the_root_count() {
        let f = field(200.0);
        let spec = ScatterSpec {
            spacing: 10.0,
            ..ScatterSpec::default()
        };
        let roots = along(&f, &spec, Rng::new(1), |_| 1.0);
        assert!(
            (18..=21).contains(&roots.len()),
            "expected about 20 roots, got {}",
            roots.len()
        );
    }

    #[test]
    fn full_regularity_places_roots_on_exact_centres() {
        let f = field(100.0);
        let spec = ScatterSpec {
            spacing: 10.0,
            regularity: 1.0,
            id_offset: 0,
        };
        let roots = along(&f, &spec, Rng::new(1), |_| 1.0);
        for (i, root) in roots.iter().enumerate() {
            assert!(
                (root.s - (i as f32 + 0.5) * 10.0).abs() < 1e-4,
                "root {i} at {}",
                root.s
            );
        }
    }

    #[test]
    fn jitter_moves_roots_but_keeps_them_in_their_bucket() {
        let f = field(200.0);
        let spec = ScatterSpec {
            spacing: 10.0,
            regularity: 0.0,
            id_offset: 0,
        };
        let roots = along(&f, &spec, Rng::new(5), |_| 1.0);
        let mut moved = 0;
        for root in &roots {
            let centre = (root.id as f32 + 0.5) * 10.0;
            assert!((root.s - centre).abs() <= 5.0 + 1e-4, "left its bucket");
            if (root.s - centre).abs() > 0.1 {
                moved += 1;
            }
        }
        assert!(moved > roots.len() / 2, "jitter barely did anything");
    }

    #[test]
    fn density_removes_roots_without_moving_the_rest() {
        let f = field(400.0);
        let spec = ScatterSpec {
            spacing: 8.0,
            ..ScatterSpec::default()
        };
        let dense = along(&f, &spec, Rng::new(77), |_| 1.0);
        let sparse = along(&f, &spec, Rng::new(77), |_| 0.4);
        assert!(sparse.len() < dense.len(), "density did not thin anything");
        assert!(!sparse.is_empty());
        for root in &sparse {
            let same = dense
                .iter()
                .find(|d| d.id == root.id)
                .expect("survivor must exist in the dense pass");
            assert_eq!(same.s, root.s, "root {} moved when density dropped", root.id);
        }
    }

    #[test]
    fn density_zero_places_nothing() {
        let f = field(200.0);
        let roots = along(&f, &ScatterSpec::default(), Rng::new(2), |_| 0.0);
        assert!(roots.is_empty());
    }

    // Pressure ramps 0 -> 1 along this curve.
    #[test]
    fn density_can_follow_the_frame() {
        let f = field(400.0);
        let spec = ScatterSpec {
            spacing: 6.0,
            ..ScatterSpec::default()
        };
        let roots = along(&f, &spec, Rng::new(3), |frame| frame.pressure);
        let first_half = roots.iter().filter(|r| r.s < 200.0).count();
        let second_half = roots.len() - first_half;
        assert!(
            second_half > first_half * 2,
            "pressure gating did not bias placement: {first_half} vs {second_half}"
        );
    }

    #[test]
    fn absurd_spacing_is_clamped_not_unbounded() {
        let f = field(5000.0);
        let spec = ScatterSpec {
            spacing: 0.0,
            ..ScatterSpec::default()
        };
        let roots = along(&f, &spec, Rng::new(1), |_| 1.0);
        assert!(roots.len() <= MAX_ROOTS as usize);
        assert!(!roots.is_empty());
    }

    #[test]
    fn id_offset_separates_passes() {
        let f = field(100.0);
        let a = along(&f, &ScatterSpec::default(), Rng::new(9), |_| 1.0);
        let b = along(
            &f,
            &ScatterSpec {
                id_offset: 10_000,
                ..ScatterSpec::default()
            },
            Rng::new(9),
            |_| 1.0,
        );
        let identical = a
            .iter()
            .zip(&b)
            .filter(|(x, y)| (x.s - y.s).abs() < 1e-6)
            .count();
        assert!(identical < a.len() / 2, "ranks landed on the same jitter");
    }
}
