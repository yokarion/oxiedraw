//! Stateless, hash-based randomness.
//!
//! Every value is a pure function of `(seed, element, channel)` rather than of
//! a stream position, so dropping one element leaves every other element's
//! numbers untouched.

/// Mix a 64-bit value (splitmix64 finalizer). Good avalanche, no state.
#[inline]
pub const fn mix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Map a hash to `0.0..1.0` using its top 24 bits (exact in `f32`).
#[inline]
pub fn hash_unit(h: u64) -> f32 {
    ((h >> 40) as f32) / 16_777_216.0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rng {
    seed: u64,
}

impl Rng {
    pub const fn new(seed: u64) -> Self {
        Self { seed }
    }

    /// A derived source for a pass that must not draw the same numbers as this
    /// one: a second depth rank, the mirrored side.
    #[must_use]
    pub const fn salted(self, salt: u64) -> Self {
        Self {
            seed: mix64(self.seed ^ salt.wrapping_mul(0xD6E8_FEB8_6659_FD93)),
        }
    }

    pub const fn seed(self) -> u64 {
        self.seed
    }

    #[inline]
    pub const fn hash(self, element: u32, channel: u8) -> u64 {
        mix64(self.seed ^ (((element as u64) << 8) | (channel as u64)))
    }

    /// Uniform in `0.0..1.0`.
    #[inline]
    pub fn unit(self, element: u32, channel: u8) -> f32 {
        hash_unit(self.hash(element, channel))
    }

    /// Uniform in `-1.0..1.0`.
    #[inline]
    pub fn signed(self, element: u32, channel: u8) -> f32 {
        self.unit(element, channel).mul_add(2.0, -1.0)
    }

    #[inline]
    pub fn range(self, element: u32, channel: u8, min: f32, max: f32) -> f32 {
        min + (max - min) * self.unit(element, channel)
    }

    /// `true` with probability `p`.
    #[inline]
    pub fn chance(self, element: u32, channel: u8, p: f32) -> bool {
        self.unit(element, channel) < p
    }

    /// Uniform integer in `min..=max`. Returns `min` if the range is inverted.
    #[inline]
    pub fn count(self, element: u32, channel: u8, min: u32, max: u32) -> u32 {
        if max <= min {
            return min;
        }
        let span = max - min + 1;
        min + (self.hash(element, channel) % u64::from(span)) as u32
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn unit_is_in_range_and_reproducible() {
        let rng = Rng::new(0xFEED);
        for element in 0..1000 {
            let v = rng.unit(element, 3);
            assert!((0.0..1.0).contains(&v), "out of range: {v}");
            assert_eq!(v, rng.unit(element, 3), "not reproducible");
        }
    }

    // The property the design leans on: a scatter that drops half its elements
    // leaves the survivors bit-identical.
    #[test]
    fn elements_are_independent_of_their_neighbours() {
        let rng = Rng::new(7);
        let dense: Vec<(u32, f32)> = (0..128).map(|i| (i, rng.unit(i, 1))).collect();
        let sparse: Vec<(u32, f32)> = (0..128)
            .filter(|i| i % 3 != 0)
            .map(|i| (i, rng.unit(i, 1)))
            .collect();
        for (id, value) in sparse {
            let dense_value = dense
                .iter()
                .find(|(d, _)| *d == id)
                .map(|(_, v)| *v)
                .expect("id present in the dense set");
            assert_eq!(value, dense_value, "element {id} moved when its neighbours went");
        }
    }

    #[test]
    fn channels_do_not_correlate() {
        let rng = Rng::new(12345);
        let same = (0..2000).filter(|&i| rng.unit(i, 0) == rng.unit(i, 1)).count();
        assert_eq!(same, 0, "channels 0 and 1 collided");
    }

    #[test]
    fn distribution_is_roughly_flat() {
        let rng = Rng::new(99);
        let mut buckets = [0_u32; 10];
        for i in 0..10_000 {
            let b = (rng.unit(i, 5) * 10.0) as usize;
            buckets[b.min(9)] += 1;
        }
        for (i, count) in buckets.iter().enumerate() {
            assert!(
                (800..1200).contains(count),
                "bucket {i} has {count}, expected near 1000"
            );
        }
    }

    #[test]
    fn salted_sources_diverge() {
        let a = Rng::new(4);
        let b = a.salted(1);
        assert_ne!(a.seed(), b.seed());
        let collisions = (0..500).filter(|&i| a.unit(i, 0) == b.unit(i, 0)).count();
        assert_eq!(collisions, 0);
    }

    #[test]
    fn count_stays_within_bounds() {
        let rng = Rng::new(31);
        for i in 0..500 {
            let n = rng.count(i, 2, 1, 4);
            assert!((1..=4).contains(&n), "count out of range: {n}");
        }
        assert_eq!(rng.count(0, 0, 3, 3), 3);
        assert_eq!(rng.count(0, 0, 5, 2), 5);
    }
}
