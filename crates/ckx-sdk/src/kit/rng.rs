//! A seeded random number generator (xorshift64*), plain data so it lives in a hub's state
//! and carries on after a snapshot. Seed it from something the hub already has: its key, the
//! clock at spawn, a match's seed.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rng {
    state: u64,
}

impl Default for Rng {
    fn default() -> Self {
        Self::seeded(1)
    }
}

impl Rng {
    /// A generator seeded with `seed`; 0 counts as 1, which xorshift needs.
    pub fn seeded(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    /// A seed from text: a key, a name.
    pub fn seed_from(text: &str) -> u64 {
        // FNV-1a.
        text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut s = self.state;
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        self.state = s;
        s.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    /// Uniform in `[0, 1)`.
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `[0, n)`; 0 when `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// Uniform in `[lo, hi]`, both included; `lo` when `hi` is not above it.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        if hi <= lo {
            return lo;
        }
        let span = hi.abs_diff(lo).saturating_add(1);
        lo.wrapping_add(self.below(span) as i64)
    }

    /// True with probability `p` (clamped to `[0, 1]`).
    pub fn chance(&mut self, p: f64) -> bool {
        self.f64() < p.clamp(0.0, 1.0)
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> Option<&'a T> {
        if items.is_empty() { None } else { items.get(self.below(items.len() as u64) as usize) }
    }

    /// The index of one weight, chosen in proportion to the weights; `None` when they sum to
    /// nothing.
    pub fn weighted(&mut self, weights: &[u64]) -> Option<usize> {
        let total: u64 = weights.iter().fold(0u64, |a, w| a.saturating_add(*w));
        if total == 0 {
            return None;
        }
        let mut roll = self.below(total);
        for (i, w) in weights.iter().enumerate() {
            if roll < *w {
                return Some(i);
            }
            roll -= w;
        }
        None
    }

    /// Shuffles in place (Fisher-Yates).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_bounded() {
        let (mut a, mut b) = (Rng::seeded(42), Rng::seeded(42));
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        for _ in 0..1_000 {
            assert!((0.0..1.0).contains(&a.f64()));
            assert!((-5..=5).contains(&a.range(-5, 5)));
            assert!(a.below(3) < 3);
        }
        // The widest span does not overflow.
        let _ = a.range(i64::MIN, i64::MAX);
        assert_eq!(a.range(4, 4), 4);
        assert!(a.pick::<u8>(&[]).is_none());
        assert_eq!(a.weighted(&[0, 0]), None);
        assert_eq!(a.weighted(&[0, 5, 0]), Some(1));
    }

    #[test]
    fn a_shuffle_keeps_every_item_and_seeds_from_text_are_stable() {
        let mut items: Vec<u32> = (0..52).collect();
        Rng::seeded(Rng::seed_from("table-1")).shuffle(&mut items);
        let mut sorted = items.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..52).collect::<Vec<_>>());
        assert_ne!(items, sorted);
        assert_eq!(Rng::seed_from("a"), Rng::seed_from("a"));
        assert_ne!(Rng::seed_from("a"), Rng::seed_from("b"));
    }
}
