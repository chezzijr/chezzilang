//! `std.uuid` — RFC 4122 version-4 (random) UUID generation.
//!
//! `v4()` returns a random UUID as the canonical 36-char `8-4-4-4-12` lowercase-hex `str`, with the
//! version nibble forced to `4` and the variant bits to `10` (so position 19 is one of `8/9/a/b`).
//! `uuid_seed(n)` reseeds the generator deterministically (for golden/reproducible runs).
//!
//! TWO streams, switched by `uuid_seed`:
//!   * DEFAULT (no `uuid_seed` call) — 16 bytes straight from the OS CSPRNG per draw
//!     ([`super::crypto::os_entropy`]), no PRNG state at all. Same shape as CPython's `uuid.uuid4()`
//!     (`int.from_bytes(os.urandom(16))`), so an id is unpredictable and one leak reveals nothing.
//!   * SEEDED (after `uuid_seed(n)`) — the reproducible 64-bit SplitMix64 stream, for golden runs.
//!     Two [`super::rand::next_u64`] draws over this module's OWN process-global `OnceLock<Mutex<u64>>`
//!     (separate from `std.rand`'s stream so a `v4()` draw never perturbs `rand.float()`). This stream
//!     is PREDICTABLE from one observed UUID — never use it for secrets.
//!
//! The switch is process-global and STICKY: once seeded, every later `v4()` in the process is seeded.
//!
//! `v4`/`uuid_seed` are a fast entropy syscall / a pure CPU transform (no blocking I/O) →
//! [`super::Kind::Inline`] on their registry entry, exactly like `crypto.secure_bytes`; they run
//! inline on every engine. LIMIT (same as `std.rand`, SEEDED path only — the default path holds no
//! state): under `--parallel`, CONCURRENT draws from multiple tasks interleave nondeterministically on
//! the shared global, so an EXACT seeded value is only deterministic for strictly-sequential draws —
//! the goldens draw sequentially.

use super::rand::next_u64;
use super::{Host, HostError, Kind, NativeFn, NativeRet, expect_args};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// A process-wide lock serializing every test that draws from the shared global UUID RNG (the unit
/// tests here AND the run-file golden). Same concurrent-draw limit as `std.rand`'s `TEST_RNG_LOCK`.
#[cfg(test)]
pub(crate) static TEST_UUID_LOCK: Mutex<()> = Mutex::new(());

/// This module's own process-global PRNG state. Only ever READ on the seeded path, and `uuid_seed`
/// overwrites it before flipping the switch — so the lazy init value is irrelevant (no auto-seed).
static UUID_RNG: OnceLock<Mutex<u64>> = OnceLock::new();

/// Has `uuid_seed(n)` been called? Unset → `v4()` draws from the OS CSPRNG; set → from `UUID_RNG`.
/// Process-global and sticky, exactly like the seeded stream it selects.
static UUID_SEEDED: AtomicBool = AtomicBool::new(false);

fn with_state<R>(f: impl FnOnce(&mut u64) -> R) -> R {
    let m = UUID_RNG.get_or_init(|| Mutex::new(0));
    let mut guard = m.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// Back to the default OS-entropy path. Tests only — the switch is sticky by design, so an unseeded
/// test must be able to undo an earlier test's `uuid_seed` (hold `TEST_UUID_LOCK` across it).
#[cfg(test)]
pub(crate) fn clear_seed() {
    UUID_SEEDED.store(false, Ordering::Relaxed);
}

/// Format 16 bytes as the canonical `8-4-4-4-12` lowercase-hex UUID string.
fn format_uuid(b: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let hex = |out: &mut String, byte: u8| {
        let _ = write!(out, "{byte:02x}");
    };
    let mut s = String::with_capacity(36);
    for (i, &byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            s.push('-');
        }
        hex(&mut s, byte);
    }
    s
}

fn v4(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "v4", 0)?;
    let mut b = [0u8; 16];
    if UUID_SEEDED.load(Ordering::Relaxed) {
        // Reproducible stream (explicitly asked for): two SplitMix64 draws, byte-identical to before.
        let (hi, lo) = with_state(|s| (next_u64(s), next_u64(s)));
        b[0..8].copy_from_slice(&hi.to_be_bytes());
        b[8..16].copy_from_slice(&lo.to_be_bytes());
    } else {
        // Default: 16 fresh bytes from the OS CSPRNG, failing closed (never a weak id).
        super::crypto::os_entropy(&mut b).map_err(|tail| HostError {
            message: format!("v4: {tail}"),
        })?;
    }
    // Version 4: high nibble of byte 6.
    b[6] = (b[6] & 0x0F) | 0x40;
    // Variant 10xx: top two bits of byte 8.
    b[8] = (b[8] & 0x3F) | 0x80;
    Ok(NativeRet::Str(format_uuid(&b)))
}

fn uuid_seed(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "uuid_seed", 1)?;
    let n = h.arg_int(0)?;
    with_state(|s| *s = n as u64);
    // Flip AFTER the state is in place, so a concurrent `v4()` never reads a stale seed. Sticky: every
    // later `v4()` in this process is reproducible (and predictable) until the process exits.
    UUID_SEEDED.store(true, Ordering::Relaxed);
    Ok(NativeRet::Nil)
}

/// Callable members. `(name, fn, kind)`. NOTE: the reseed is `uuid_seed` (not `seed`) because
/// `std.rand` already owns `seed`. That was originally forced — the old `is_blocking` classifier
/// dispatched by BARE NAME — and is now only a naming courtesy: [`Kind`] rides the entry, so two
/// modules may share a member name safely. The distinct name stays because renaming it is a surface
/// change, not because the engine needs it.
pub const MEMBERS: &[(&str, NativeFn, Kind)] = &[
    ("v4", v4, Kind::Inline),
    ("uuid_seed", uuid_seed, Kind::Inline),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct IntHost {
        ints: Vec<i64>,
    }
    impl Host for IntHost {
        fn arg_count(&self) -> usize {
            self.ints.len()
        }
        fn arg_int(&mut self, i: usize) -> Result<i64, HostError> {
            self.ints.get(i).copied().ok_or(HostError {
                message: "missing arg".into(),
            })
        }
        fn arg_float(&mut self, i: usize) -> Result<f64, HostError> {
            Ok(self.arg_int(i)? as f64)
        }
        fn arg_is_int(&self, i: usize) -> bool {
            i < self.ints.len()
        }
        fn arg_str(&mut self, _i: usize) -> Result<String, HostError> {
            Err(HostError {
                message: "no str args".into(),
            })
        }
        fn arg_str_map(&mut self, _i: usize) -> Result<Vec<(String, String)>, HostError> {
            Err(HostError {
                message: "no map args".into(),
            })
        }
        fn write_stdout(&mut self, _s: &str) {}
        fn write_stderr(&mut self, _s: &str) {}
        fn read_line(&mut self) -> Result<Option<String>, HostError> {
            Ok(None)
        }
        fn os_args(&self) -> Vec<String> {
            vec![]
        }
        fn os_env(&self, _key: &str) -> Option<String> {
            None
        }
        fn os_getcwd(&self) -> Result<Vec<u8>, HostError> {
            Ok(b"/".to_vec())
        }
    }

    fn reseed(n: i64) {
        uuid_seed(&mut IntHost { ints: vec![n] }).unwrap();
    }

    fn draw() -> String {
        match v4(&mut IntHost::default()).unwrap() {
            NativeRet::Str(s) => s,
            other => panic!("expected Str, got {other:?}"),
        }
    }

    #[test]
    fn v4_shape_and_seeded_determinism() {
        let _g = TEST_UUID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Clear FIRST so this verifies that `uuid_seed` flips the switch, not that some earlier test
        // left it flipped: the switch is process-global and sticky, so on a leftover `true` a
        // `uuid_seed` that forgot to set it would still take the seeded branch and these frozen
        // vectors would pass silently — the test would only be loud in the order libtest happens to
        // pick today.
        clear_seed();
        reseed(42);
        let a = draw();
        let b = draw();

        // Shape: 36 chars, dashes at 8/13/18/23, version '4' at 14, variant in {8,9,a,b} at 19.
        assert_v4_shape(&a);
        assert_v4_shape(&b);

        // Seeded determinism: frozen pair (captured from a real run; regen if the format changes).
        assert_eq!(a, "bdd73226-2feb-4e95-a8ef-e333b266f103");
        assert_eq!(b, "47526757-130f-4f52-981c-e1ff0e4ae394");
    }

    /// Check the shape invariants RFC 4122 v4 pins, on any draw.
    fn assert_v4_shape(u: &str) {
        assert_eq!(u.len(), 36, "uuid not 36 chars: {u}");
        let chars: Vec<char> = u.chars().collect();
        for idx in [8, 13, 18, 23] {
            assert_eq!(chars[idx], '-', "expected '-' at {idx} in {u}");
        }
        assert_eq!(chars[14], '4', "version nibble not 4 in {u}");
        assert!(
            matches!(chars[19], '8' | '9' | 'a' | 'b'),
            "variant char not in 8/9/a/b in {u}"
        );
        for (idx, c) in chars.iter().enumerate() {
            if matches!(idx, 8 | 13 | 18 | 23) {
                continue;
            }
            assert!(c.is_ascii_hexdigit(), "non-hex char at {idx} in {u}");
            assert!(!c.is_ascii_uppercase(), "uppercase hex in {u}");
        }
    }

    /// The security property: with NO `uuid_seed` call, `v4()` draws from the OS CSPRNG, not from the
    /// process-global SplitMix64 — so its first value is NOT the seed-42 vector (which is what a
    /// default-seeded PRNG would hand back after the state was pinned) and two draws differ.
    #[test]
    fn v4_unseeded_draws_from_os_entropy() {
        let _g = TEST_UUID_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // The seeded flag is process-global and sticky — an earlier test may have set it.
        reseed(42);
        clear_seed();

        let a = draw();
        let b = draw();
        assert_v4_shape(&a);
        assert_v4_shape(&b);
        assert_ne!(a, b, "two unseeded v4() draws must differ");
        assert_ne!(
            a, "bdd73226-2feb-4e95-a8ef-e333b266f103",
            "unseeded v4() must not replay the seeded PRNG stream"
        );
    }
}
