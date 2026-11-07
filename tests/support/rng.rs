//! A small deterministic generator for property tests, std only: splitmix64, seeded by the test.
//!
//! The convention for a property test: loop over the seeds `1..=N`, build one [`Rng`] per seed with
//! [`Rng::new`], and put the seed in every assertion message, so a failure names the one seed that
//! reproduces it. Nothing is read from the environment or the clock, so a run is the same every time.
//!
//! ```ignore
//! for seed in 1..=CASES {
//!     let mut rng = Rng::new(seed);
//!     let n = rng.below(16);
//!     assert!(n < 16, "seed {seed}: {n} out of range");
//! }
//! ```

/// A splitmix64 generator; the same seed always gives the same sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rng(u64);

/// The splitmix64 state increment, also the state seed 0 starts from.
const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

impl Rng {
    /// A generator for `seed`; seed 0 is mapped to a fixed non-zero state, so the state is never zero.
    pub const fn new(seed: u64) -> Self {
        Self(if seed == 0 { GAMMA } else { seed })
    }

    /// The next 64 bits of the sequence.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(GAMMA);
        let mut z = self.0;
        z = (z ^ z.wrapping_shr(30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ z.wrapping_shr(27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ z.wrapping_shr(31)
    }

    /// A value in `0..n` (with a modulo bias too small to matter for tests), or 0 when `n` is 0.
    /// Every call draws one value, so what follows does not depend on `n`.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64().checked_rem(n).unwrap_or(0)
    }

    /// True with probability `num / den`: always when `num >= den`, never when `num` or `den` is 0.
    pub fn chance(&mut self, num: u32, den: u32) -> bool {
        let drawn = self.below(u64::from(den));
        den != 0 && drawn < u64::from(num)
    }

    /// One element of `items`, or `None` when it is empty; draws one value either way.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        let len = u64::try_from(items.len()).ok()?;
        items.get(usize::try_from(self.below(len)).ok()?)
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn same_seed_same_sequence() {
        for seed in 0..=64 {
            let (mut a, mut b) = (Rng::new(seed), Rng::new(seed));
            for draw in 0..256 {
                assert_eq!(
                    a.next_u64(),
                    b.next_u64(),
                    "seed {seed}: draw {draw} differs"
                );
            }
        }
    }

    // pinned so that a change to the algorithm, which changes every property test's cases, shows here
    #[test]
    fn seed_one_is_pinned() {
        let mut rng = Rng::new(1);
        let drawn = [rng.next_u64(), rng.next_u64(), rng.next_u64()];
        assert_eq!(
            drawn,
            [
                0x910A_2DEC_8902_5CC1,
                0xBEEB_8DA1_658E_EC67,
                0xF893_A2EE_FB32_555E
            ]
        );
    }

    #[test]
    fn helpers_stay_in_range() {
        let items = ["a", "b", "c"];
        for seed in 1..=64 {
            let mut rng = Rng::new(seed);
            assert_eq!(rng.below(0), 0, "seed {seed}: below(0)");
            assert_eq!(rng.below(1), 0, "seed {seed}: below(1)");
            assert!(rng.below(7) < 7, "seed {seed}: below(7)");
            assert!(!rng.chance(0, 5), "seed {seed}: chance(0, 5)");
            assert!(!rng.chance(5, 0), "seed {seed}: chance(5, 0)");
            assert!(rng.chance(5, 5), "seed {seed}: chance(5, 5)");
            assert!(rng.pick(&items).is_some(), "seed {seed}: pick of 3");
            assert!(rng.pick::<u8>(&[]).is_none(), "seed {seed}: pick of none");
        }
    }
}
