//! `std.rand` — native pseudo-random number generator (SplitMix64).
//!
//! Exposes ONLY scalars: `seed(n) -> nil` (deterministic reseed), `float() -> float` in `[0, 1)`,
//! `int(lo, hi) -> int` (half-open `[lo, hi)`, faults if `hi <= lo`), `bool() -> bool`. Generic
//! collection helpers (`shuffle`/`choice`/`sample`) live in the pure-Chezzi `std.iter` module and
//! call `rand.int` — the native FFI seam carries only engine-neutral scalars, so it cannot return a
//! generic `list[T]` (forcing the split), and a native module name short-circuits a same-named
//! `std/<name>.chz` file in the resolver (so scalars + helpers cannot share the `rand` namespace).
//!
//! State is a single PROCESS-GLOBAL `OnceLock<Mutex<u64>>` (NOT thread-local, NOT Host-side): all
//! three engines (interp / cooperative VM / M:N) share one stream at the NativeFn seam, so any
//! SEQUENTIAL draw sequence is byte-identical across engines (3-engine parity by construction). The
//! global auto-seeds from OS entropy (`libc::getrandom`, with a time/address-mix fallback) on first
//! use; `seed(n)` overwrites it to make the stream deterministic.
//!
//! LIMIT (documented, not a bug): under `--parallel`, CONCURRENT draws from multiple tasks interleave
//! nondeterministically on the shared global — so engines may diverge ONLY for concurrent draws. The
//! goldens draw strictly sequentially to stay deterministic on all three engines.

use super::{Host, HostError, Kind, NativeFn, NativeRet, expect_args};
use std::sync::{Mutex, OnceLock};

/// A process-wide lock serializing every test that draws from the shared global RNG (the unit tests
/// here AND the run-file goldens in `vm`/`interp`). The harness runs tests on multiple threads, so
/// two tests that each `seed()`-then-draw would otherwise interleave on the shared global and
/// diverge — this is the same concurrent-draw limit documented for `--parallel`, surfacing in the
/// test runner. Hold it across a whole `seed()`-then-draw sequence to keep that sequence atomic.
#[cfg(test)]
pub(crate) static TEST_RNG_LOCK: Mutex<()> = Mutex::new(());

/// One SplitMix64 step: advance `state` and return the next mixed `u64`. Pure (no global) so it is
/// golden-testable in isolation, free of any test-order coupling with the process-global RNG.
pub(crate) fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The process-global PRNG state. Lazily auto-seeded from OS entropy on first use.
static RNG: OnceLock<Mutex<u64>> = OnceLock::new();

/// Auto-seed value when no `seed(n)` has been called: try `libc::getrandom`, falling back to a
/// time/address/counter mix run through one SplitMix64 step. Never returns zero (a zero seed is a
/// perfectly valid SplitMix64 state, but the fallback mixes to a non-trivial value regardless).
fn auto_seed() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    // 1) OS entropy (Linux glibc >= 2.25). `getrandom(buf, buflen, flags) -> ssize_t`: a full read
    //    fills our 8-byte buffer; a short read or -1 falls through to the mix below.
    #[cfg(target_os = "linux")]
    {
        let mut buf = [0u8; 8];
        // SAFETY: writing `buf.len()` bytes into our own stack buffer; flags=0 (blocking, but the
        // pool is seeded once at process start so there is effectively always entropy available).
        let n = unsafe { libc::getrandom(buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
        if n == buf.len() as isize {
            return u64::from_ne_bytes(buf);
        }
    }

    // 2) Fallback: mix wall-clock nanos, an address, and a process-unique counter through one step.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let addr = &COUNTER as *const _ as u64;
    let ctr = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut s = nanos ^ addr ^ ctr.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    next_u64(&mut s)
}

/// Run `f` with exclusive access to the global PRNG state, lazily auto-seeding it on first use.
fn with_state<R>(f: impl FnOnce(&mut u64) -> R) -> R {
    let m = RNG.get_or_init(|| Mutex::new(auto_seed()));
    let mut guard = m.lock().unwrap_or_else(|e| e.into_inner());
    f(&mut guard)
}

/// One draw from the global stream.
fn draw() -> u64 {
    with_state(next_u64)
}

fn seed(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "seed", 1)?;
    let n = h.arg_int(0)?;
    with_state(|s| *s = n as u64);
    Ok(NativeRet::Nil)
}

fn float(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "float", 0)?;
    // 53-bit mantissa: take the top 53 bits and scale by 2^-53 → uniform in [0, 1).
    let bits = draw() >> 11;
    Ok(NativeRet::Float(
        bits as f64 * (1.0 / 9_007_199_254_740_992.0),
    ))
}

fn int(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "int", 2)?;
    let lo = h.arg_int(0)?;
    let hi = h.arg_int(1)?;
    if hi <= lo {
        return Err(HostError {
            message: "rand.int(lo, hi): hi must be > lo".to_string(),
        });
    }
    // Range as u64 via i128 so it is correct for the full i64 span (e.g. lo=i64::MIN, hi=i64::MAX
    // would overflow an i64 subtraction). `hi > lo` ⇒ the difference is in `1..=2^64`, fitting u64.
    let range = (hi as i128 - lo as i128) as u64;
    // Unbiased rejection sampling: accept only draws below the largest multiple of `range`, so the
    // modulo is uniform. zone == u64::MAX when range == 1 (every draw accepts on the first try).
    let zone = u64::MAX - (u64::MAX % range);
    let r = loop {
        let r = draw();
        if r < zone {
            break r;
        }
    };
    // lo + (r % range): wrap the i64 arithmetic over the bit pattern so it is correct across the
    // full span (the result is provably in [lo, hi) since `r % range < range == hi - lo`).
    Ok(NativeRet::Int(lo.wrapping_add((r % range) as i64)))
}

fn bool(h: &mut dyn Host) -> Result<NativeRet, HostError> {
    expect_args(h, "bool", 0)?;
    Ok(NativeRet::Bool(draw() & 1 == 1))
}

/// Callable members. `(name, fn, kind)`.
pub const MEMBERS: &[(&str, NativeFn, Kind)] = &[
    ("seed", seed, Kind::Inline),
    ("float", float, Kind::Inline),
    ("int", int, Kind::Inline),
    ("bool", bool, Kind::Inline),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal int-only `Host` for exercising the rand natives in isolation.
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

    fn host(ints: Vec<i64>) -> IntHost {
        IntHost { ints }
    }

    /// Reseed the global stream deterministically. EVERY test that touches the global RNG must call
    /// this first (the global is process-shared, so order would otherwise couple the tests).
    fn reseed(n: i64) {
        let mut h = host(vec![n]);
        seed(&mut h).unwrap();
    }

    /// The pure SplitMix64 step, golden vector for seed=0 (frozen from a real run; the canonical
    /// SplitMix64 reference sequence). Tested in isolation — no global, no test-order coupling.
    #[test]
    fn splitmix64_golden_sequence() {
        let mut s = 0u64;
        assert_eq!(next_u64(&mut s), 0xE220_A839_7B1D_CDAF);
        assert_eq!(next_u64(&mut s), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(next_u64(&mut s), 0x06C4_5D18_8009_454F);
        assert_eq!(next_u64(&mut s), 0xF88B_B8A8_724C_81EC);
    }

    #[test]
    fn float_in_unit_range_and_deterministic_after_seed() {
        let _g = TEST_RNG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reseed(0);
        // First draw equals the frozen constant: (next_u64(seed=0) >> 11) scaled by 2^-53.
        let first = match float(&mut host(vec![])).unwrap() {
            NativeRet::Float(v) => v,
            other => panic!("expected Float, got {other:?}"),
        };
        let expect = (0xE220_A839_7B1D_CDAFu64 >> 11) as f64 * (1.0 / 9_007_199_254_740_992.0);
        assert_eq!(first, expect);
        // 100 draws all in [0, 1).
        for _ in 0..100 {
            let v = match float(&mut host(vec![])).unwrap() {
                NativeRet::Float(v) => v,
                other => panic!("expected Float, got {other:?}"),
            };
            assert!((0.0..1.0).contains(&v), "float() out of [0,1): {v}");
        }
    }

    #[test]
    fn int_is_half_open_and_faults_on_empty_range() {
        let _g = TEST_RNG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reseed(0);
        let mut saw_zero = false;
        for _ in 0..1000 {
            let v = match int(&mut host(vec![0, 10])).unwrap() {
                NativeRet::Int(v) => v,
                other => panic!("expected Int, got {other:?}"),
            };
            assert!((0..10).contains(&v), "int(0,10) out of range: {v}");
            if v == 0 {
                saw_zero = true;
            }
            assert_ne!(v, 10, "int(0,10) returned the exclusive upper bound");
        }
        assert!(
            saw_zero,
            "int(0,10) never produced the inclusive lower bound"
        );

        let err = int(&mut host(vec![5, 5])).unwrap_err();
        assert_eq!(err.message, "rand.int(lo, hi): hi must be > lo");
        let err = int(&mut host(vec![7, 3])).unwrap_err();
        assert_eq!(err.message, "rand.int(lo, hi): hi must be > lo");
    }

    #[test]
    fn bool_emits_both_values_after_seed() {
        let _g = TEST_RNG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        reseed(0);
        let mut saw_true = false;
        let mut saw_false = false;
        for _ in 0..100 {
            match bool(&mut host(vec![])).unwrap() {
                NativeRet::Bool(true) => saw_true = true,
                NativeRet::Bool(false) => saw_false = true,
                other => panic!("expected Bool, got {other:?}"),
            }
        }
        assert!(saw_true && saw_false, "bool() did not emit both values");
    }

    /// Without any `seed()` call the auto-seed path (OS entropy / fallback mix) must still yield
    /// in-range draws. Cannot assert a value (entropy), only shape.
    #[test]
    fn auto_seed_without_seed_call_yields_in_range() {
        let _g = TEST_RNG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // NB: relies on the process-global lazy init; if an earlier test already seeded, this is
        // still a valid range check (it never asserts a specific value).
        let f = match float(&mut host(vec![])).unwrap() {
            NativeRet::Float(v) => v,
            other => panic!("expected Float, got {other:?}"),
        };
        assert!((0.0..1.0).contains(&f));
        let i = match int(&mut host(vec![0, 5])).unwrap() {
            NativeRet::Int(v) => v,
            other => panic!("expected Int, got {other:?}"),
        };
        assert!((0..5).contains(&i));
    }

    /// Reproduces the missing-`TEST_RNG_LOCK` flake in the phase-4d golden
    /// (`vm::tests::golden_std_native_4d_chz_matches_expected_and_interp`): its
    /// `examples/std_native_4d.chz` does `rand.seed(1)` then `rand.int(0,100)` as TWO separate native
    /// calls on the shared process-global RNG, expecting `65`. The per-call state mutex makes each
    /// call atomic but NOT the seed→draw SEQUENCE, so a sibling rand test reseeding between them
    /// corrupts the draw. Holding `TEST_RNG_LOCK` across the whole sequence (as every other rand test
    /// does) is what serializes it. A background reseeder — modelling any concurrently-scheduled rand
    /// test, which also holds `TEST_RNG_LOCK` — hammers the global; while we hold the lock it is
    /// excluded and the draw stays the golden's deterministic `65`. Drop the guard below and the
    /// reseeder slips between our `seed(1)` and `int(0,100)` → the draw drifts off `65` (the flake).
    #[test]
    fn seed_then_int_sequence_is_atomic_only_under_rng_lock() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        // THE guard: hold TEST_RNG_LOCK across the entire seed→draw sequence. Removing this line
        // reproduces the golden's flake (the reseeder interleaves; the assertion below fails).
        let _g = TEST_RNG_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        // A sibling rand test: it also takes TEST_RNG_LOCK, so while we hold it this thread is
        // excluded (serialized) — that exclusion is exactly what keeps our draw deterministic. It
        // reads `stop` WITHOUT the lock (so it can observe the stop signal even while we hold the
        // lock) and only takes the lock for the reseed itself.
        let reseeder = std::thread::spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                let _g = TEST_RNG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                let mut h = host(vec![9999]);
                seed(&mut h).unwrap();
                drop(_g);
                std::thread::yield_now();
            }
        });

        for _ in 0..2000 {
            reseed(1);
            let v = match int(&mut host(vec![0, 100])).unwrap() {
                NativeRet::Int(v) => v,
                other => panic!("expected Int, got {other:?}"),
            };
            assert_eq!(
                v, 65,
                "seed(1);int(0,100) drifted off 65 — the seed→draw sequence was not serialized on \
                 TEST_RNG_LOCK (the phase-4d golden's flake)"
            );
        }

        // Signal stop and RELEASE the lock BEFORE joining: the reseeder blocks on TEST_RNG_LOCK
        // while we hold it, so it can only reach its next `stop` check (and exit) once we drop the
        // guard — joining while still holding it would deadlock.
        stop.store(true, Ordering::Relaxed);
        drop(_g);
        reseeder.join().unwrap();
    }
}
