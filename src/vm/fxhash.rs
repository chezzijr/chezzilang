//! M19 — a tiny FxHash `BuildHasher` (the rustc-hash algorithm) for our integer-keyed maps.
//!
//! The map/set `index` (`heap::MapData`/`SetData`) is keyed by an *already-computed* content hash
//! (`u64`), and `str_intern` is keyed by a raw pointer (`usize`). The stdlib default (SipHash) is a
//! keyed, DoS-resistant hash — overkill when the key is itself a good hash or a pointer. FxHash mixes
//! with a single rotate + multiply, far cheaper, and a user `hash() -> int` that returns clustered
//! ints still gets spread by the multiply (pure identity would bucket-collide them). The hasher only
//! routes the probe; `values_equal` confirms every hit, so swapping it can never change a result.
//!
//! Not used for any cryptographic / untrusted-input boundary — purely an in-process speed lever.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// 64-bit FxHash seed (the rustc-hash constant).
const SEED64: u64 = 0x51_7c_c1_b7_27_22_0a_95;
/// Per-word rotate before XOR-mix.
const ROTATE: u32 = 5;

/// A non-cryptographic hasher: `hash = (hash <<< 5 ^ word) * SEED` per 64-bit word.
#[derive(Default)]
pub struct FxHasher {
    hash: u64,
}

impl FxHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(ROTATE) ^ word).wrapping_mul(SEED64);
    }
}

impl Hasher for FxHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut buf = [0u8; 8];
            buf[..chunk.len()].copy_from_slice(chunk);
            self.add(u64::from_le_bytes(buf));
        }
    }
    // The hot paths key on `u64` (cached hashes) and `usize` (pointers / slot ids) — feed them whole.
    #[inline]
    fn write_u64(&mut self, i: u64) {
        self.add(i);
    }
    #[inline]
    fn write_usize(&mut self, i: usize) {
        self.add(i as u64);
    }
    #[inline]
    fn write_u32(&mut self, i: u32) {
        self.add(i as u64);
    }
    #[inline]
    fn finish(&self) -> u64 {
        // Finalizer (splitmix64-style avalanche). FxHash's multiply mixes entropy only UPward, so an
        // input whose entropy lives in high bits — e.g. `f64::to_bits` of an integer key, whose low
        // mantissa bits are zero — leaves hashbrown's low-bit bucket index degenerate (→ O(n) probe
        // chains; measured 100× on a 200k-int map). One xorshift-multiply-xorshift avalanches the
        // high bits down. A handful of ops — still far under SipHash.
        let mut z = self.hash;
        z = (z ^ (z >> 32)).wrapping_mul(0xd6e8_feb8_6659_fd93);
        z ^ (z >> 32)
    }
}

/// `Default`-constructible build hasher — so `FxHashMap` keeps deriving `Default` like a plain map.
pub type FxBuildHasher = BuildHasherDefault<FxHasher>;
/// A `HashMap` that hashes with [`FxHasher`]. Drop-in for `std::collections::HashMap`.
pub type FxHashMap<K, V> = HashMap<K, V, FxBuildHasher>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::Hash;

    fn fx<T: Hash>(v: T) -> u64 {
        let mut h = FxHasher::default();
        v.hash(&mut h);
        h.finish()
    }

    #[test]
    fn deterministic_and_distinguishing() {
        // Same input → same hash (no random seed); distinct small ints → distinct hashes (the mix
        // spreads sequential keys, the property identity hashing would lose).
        assert_eq!(fx(42u64), fx(42u64));
        assert_ne!(fx(1u64), fx(2u64));
        assert_ne!(fx(0u64), fx(1u64));
        assert_ne!(fx(100usize), fx(101usize));
    }

    #[test]
    fn map_round_trips() {
        let mut m: FxHashMap<u64, &str> = FxHashMap::default();
        m.insert(7, "a");
        m.insert(7, "b"); // overwrite — same key
        m.insert(8, "c");
        assert_eq!(m.get(&7), Some(&"b"));
        assert_eq!(m.get(&8), Some(&"c"));
        assert_eq!(m.get(&9), None);
        assert_eq!(m.len(), 2);
    }
}
