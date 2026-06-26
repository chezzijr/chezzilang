//! Deterministic, seedable PRNG for the differential generator.
//!
//! Hand-rolled `splitmix64` (seeding) → `xoshiro256**` (stream) so the engine stays
//! dependency-free (the crate has no `rand`). Same seed => same program, which is the
//! unit of reproduction for a failing case.

pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    /// Seed the xoshiro state by running splitmix64 four times.
    pub fn seed(seed: u64) -> Self {
        let mut z = seed;
        let mut next = || {
            z = z.wrapping_add(0x9E3779B97F4A7C15);
            let mut x = z;
            x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
            x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB);
            x ^ (x >> 31)
        };
        Rng {
            s: [next(), next(), next(), next()],
        }
    }

    fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in `[0, n)`. `below(0)` returns 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        // Lemire-style rejection-free-ish: good enough for fuzzing, not crypto.
        self.next_u64() % n
    }

    /// Uniform in `[lo, hi]` inclusive. Requires `lo <= hi`.
    pub fn range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(lo <= hi);
        let span = (hi as i128 - lo as i128 + 1) as u128;
        lo + (self.next_u64() as u128 % span) as i64
    }

    /// True with probability `p` (clamped to [0,1]).
    pub fn chance(&mut self, p: f64) -> bool {
        let p = p.clamp(0.0, 1.0);
        (self.next_u64() as f64 / u64::MAX as f64) < p
    }

    /// Pick an index in `[0, len)`. Panics if `len == 0` — callers must guard.
    pub fn pick(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    /// Borrow one of the choices uniformly.
    pub fn choice<'a, T>(&mut self, choices: &'a [T]) -> &'a T {
        &choices[self.pick(choices.len())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let mut a = Rng::seed(42);
        let mut b = Rng::seed(42);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn different_seed_different_stream() {
        let mut a = Rng::seed(1);
        let mut b = Rng::seed(2);
        // Vanishingly unlikely to collide on the first draw.
        assert_ne!(a.next_u64(), b.next_u64());
    }

    #[test]
    fn range_within_bounds() {
        let mut r = Rng::seed(7);
        for _ in 0..10000 {
            let v = r.range_i64(-1000, 1000);
            assert!((-1000..=1000).contains(&v));
        }
    }

    #[test]
    fn below_within_bounds() {
        let mut r = Rng::seed(7);
        for _ in 0..10000 {
            assert!(r.below(13) < 13);
        }
        assert_eq!(r.below(0), 0);
    }
}
