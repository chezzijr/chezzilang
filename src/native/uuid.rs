//! `std.uuid` — RFC 4122 version-4 (random) UUID generation.
//!
//! `v4()` returns a random UUID as the canonical 36-char `8-4-4-4-12` lowercase-hex `str`, with the
//! version nibble forced to `4` and the variant bits to `10` (so position 19 is one of `8/9/a/b`).
//! `uuid_seed(n)` reseeds the generator deterministically (for golden/reproducible runs).
//!
//! State is this module's OWN process-global `OnceLock<Mutex<u64>>` (separate from `std.rand`'s
//! stream so a `v4()` draw never perturbs a program's `rand.float()` sequence), auto-seeded from OS
//! entropy on first use. The 128 random bits come from two [`super::rand::next_u64`] draws (the
//! shared SplitMix64 step is reused — the RNG algorithm is not duplicated).
//!
//! `v4`/`uuid_seed` are pure CPU transforms (no I/O) → not in [`super::is_blocking`]; they run inline
//! on every engine. LIMIT (same as `std.rand`): under `--parallel`, CONCURRENT draws from multiple
//! tasks interleave nondeterministically on the shared global, so an EXACT seeded value is only
//! deterministic for strictly-sequential draws — the goldens draw sequentially.

use super::rand::next_u64;
use super::{Host, HostError, NativeFn, NativeRet, expect_args};
use std::sync::{Mutex, OnceLock};

/// A process-wide lock serializing every test that draws from the shared global UUID RNG (the unit
/// tests here AND the run-file golden). Same concurrent-draw limit as `std.rand`'s `TEST_RNG_LOCK`.
#[cfg(test)]
pub(crate) static TEST_UUID_LOCK: Mutex<()> = Mutex::new(());

/// This module's own process-global PRNG state, lazily auto-seeded from OS entropy.
static UUID_RNG: OnceLock<Mutex<u64>> = OnceLock::new();

/// Auto-seed from OS entropy (reuses `std.rand`'s strategy), mixing the address so two modules don't
/// share a seed when the OS-entropy path is unavailable.
fn auto_seed() -> u64 {
    #[cfg(target_os = "linux")]
    {
        let mut buf = [0u8; 8];
        // SAFETY: writing `buf.len()` bytes into our own stack buffer; flags=0.
        let n = unsafe { libc::getrandom(buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n == buf.len() as isize {
            return u64::from_ne_bytes(buf);
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let addr = &UUID_RNG as *const _ as u64;
    let mut s = nanos ^ addr ^ 0x9E37_79B9_7F4A_7C15;
    next_u64(&mut s)
}

fn with_state<R>(f: impl FnOnce(&mut u64) -> R) -> R {
    let m = UUID_RNG.get_or_init(|| Mutex::new(auto_seed()));
    let mut guard = m.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
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
    let (hi, lo) = with_state(|s| (next_u64(s), next_u64(s)));
    let mut b = [0u8; 16];
    b[0..8].copy_from_slice(&hi.to_be_bytes());
    b[8..16].copy_from_slice(&lo.to_be_bytes());
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
    Ok(NativeRet::Nil)
}

/// Callable members. `(name, fn)`. NOTE: the reseed is `uuid_seed` (not `seed`) to keep the bare
/// member name unique across modules — `std.rand` already owns `seed` (the `is_blocking` classifier
/// dispatches by bare name, guarded by `native_member_names_are_unique_across_modules`).
pub const MEMBERS: &[(&str, NativeFn)] = &[("v4", v4), ("uuid_seed", uuid_seed)];

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
        reseed(42);
        let a = draw();
        let b = draw();

        // Shape: 36 chars, dashes at 8/13/18/23, version '4' at 14, variant in {8,9,a,b} at 19.
        for u in [&a, &b] {
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

        // Seeded determinism: frozen pair (captured from a real run; regen if the format changes).
        assert_eq!(a, "bdd73226-2feb-4e95-a8ef-e333b266f103");
        assert_eq!(b, "47526757-130f-4f52-981c-e1ff0e4ae394");
    }
}
