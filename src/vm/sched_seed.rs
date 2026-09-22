//! TICKET-167 — the seeded scheduler mode (`docs/future.md` §2b). `CHEZZI_SCHED_SEED=<u64>` makes
//! every free scheduling choice (queue pick, steal victim, preemption budget, handoff slot) draw from
//! a seeded PRNG instead of timing, so a schedule/netpoller race can be found and replayed
//! mechanically. Unset, `on()` is one relaxed atomic load — see `## Decisions` "Unset costs one
//! relaxed load per decision point" in TICKET-167.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static ON: AtomicBool = AtomicBool::new(false);
static SEED: AtomicU64 = AtomicU64::new(0);
/// One stream counter shared by every `MnSched` created this process, so each sched's `SeedRng` is
/// keyed by CREATION ORDER (see `## Decisions` "One RNG stream per `MnSched`, keyed by creation
/// order").
static STREAMS: AtomicU64 = AtomicU64::new(0);

/// Called once, before any worker is spawned (mirrors `runnable`'s documented exception).
pub fn init(seed: u64) {
    SEED.store(seed, Ordering::Relaxed);
    ON.store(true, Ordering::Release);
}

#[inline(always)]
pub fn on() -> bool {
    ON.load(Ordering::Relaxed)
}

pub fn seed() -> Option<u64> {
    if on() {
        Some(SEED.load(Ordering::Relaxed))
    } else {
        None
    }
}

/// Parse `CHEZZI_SCHED_SEED`. Unset or blank is `Ok(None)`; a trimmed `u64` is `Ok(Some(_))`;
/// anything else is `Err(trimmed)`, mirroring `main.rs::resolve_threads_env`.
pub fn parse(raw: Option<&str>) -> Result<Option<u64>, String> {
    let raw = match raw {
        None => return Ok(None),
        Some(r) => r.trim(),
    };
    if raw.is_empty() {
        return Ok(None);
    }
    match raw.parse::<u64>() {
        Ok(n) => Ok(Some(n)),
        Err(_) => Err(raw.to_string()),
    }
}

const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// splitmix64 finalizer.
fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A counter-based splitmix64 stream: a draw is one `fetch_add`, no lock. One instance per
/// `MnSched`.
pub(crate) struct SeedRng(AtomicU64);

impl SeedRng {
    pub(crate) fn new() -> Self {
        let stream = STREAMS.fetch_add(1, Ordering::Relaxed);
        let state = mix(SEED.load(Ordering::Relaxed) ^ mix(stream));
        SeedRng(AtomicU64::new(state))
    }

    pub(crate) fn next(&self) -> u64 {
        let s = self
            .0
            .fetch_add(GAMMA, Ordering::Relaxed)
            .wrapping_add(GAMMA);
        mix(s)
    }

    /// A draw uniform in `[0, n)`. `0` for `n <= 1` (nothing to choose between).
    pub(crate) fn below(&self, n: u64) -> u64 {
        if n <= 1 { 0 } else { self.next() % n }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_accepts_u64_and_rejects_junk() {
        assert_eq!(parse(Some(" 7 ")), Ok(Some(7)));
        assert_eq!(parse(Some("")), Ok(None));
        assert_eq!(parse(None), Ok(None));
        assert_eq!(parse(Some("abc")), Err("abc".to_string()));
        assert_eq!(parse(Some("-1")), Err("-1".to_string()));
    }

    #[test]
    fn seed_rng_below_stays_in_range() {
        let rng = SeedRng::new();
        for _ in 0..1000 {
            assert!(rng.below(5) < 5);
        }
        assert_eq!(rng.below(1), 0);
    }
}
