//! W8-7 — the sys-time gate. `docs/gaps.md` row W8-7: the DEFAULT worker count (all cores) was the
//! SLOWEST setting, because every reduction-budget preemption (`MnSched::yield_fiber`, `src/vm/mod.rs`)
//! broadcast `cv.notify_all()`. On a CPU-bound `parallel:` scope with more cores than tasks, each
//! broadcast wakes every idle worker into an O(W) `try_steal` probe that finds nothing and re-parks —
//! O(W^2) mutex/futex churn per time slice, tens of thousands of slices a second. The fix deleted the
//! `notify_all` (the liveness argument lives on `MnSched::yield_fiber`'s doc comment); the observable
//! signature is `sys` time collapsing at high worker counts.
//!
//! **Deliberately its own file/target, not folded into `tests/chezzi_threads_cli.rs`.** Cargo runs
//! separate integration-test targets SEQUENTIALLY (confirmed empirically: each "Running tests/X.rs"
//! block completes before the next starts), but multiple `#[test]` fns WITHIN one target run
//! concurrently up to `--test-threads`. This test spawns a genuinely CPU-bound 4-task `parallel:`
//! subprocess that consumes ~4 real cores for about a second; putting it in the same file as
//! `chezzi_threads_cli.rs`'s W8-8 timing tests measurably destabilized them under
//! `RUST_TEST_THREADS=4` — reproduced directly: 1 failure in 4 runs of the combined file vs 0 once
//! separated. **That evidence is DATED (pre-TICKET-059) and its numbers no longer describe the
//! gate it cites:** the W8-8 tests then asserted a two-sample `t8/t1 > 5.5` ratio, and the observed
//! failure was that ratio dropping to 4.34. TICKET-059 replaced it with a one-sided
//! `cpu <= wall * MAX_CORES_AT_ONE_WORKER` from `wait4` child rusage, which contention can only
//! LOWER — so a CPU-hungry neighbour can no longer push those tests red the way this note describes.
//! The separation is kept anyway (this file still burns ~4 real cores for about a second, and a
//! separate target costs nothing), but do not cite the 4.34 measurement as a live reason: re-measure
//! before concluding anything about merging the files.

#[cfg(unix)]
#[path = "support/child_rusage.rs"]
mod child_rusage;

/// W8-7 — the worker count this gate drives. NOT the default (all cores): the herd is a function of
/// how many workers sit idle while a fiber preempts, so a gate that inherits the core count measures
/// nothing on a small box. 32 workers against this fixture's 4 tasks leaves 28 idle at any core
/// count, so the signal is present on CI hardware as well as a workstation. Measured below.
#[cfg(unix)]
const HERD_WORKERS: usize = 32;

/// W8-7 — the sys/user ceiling. Calibrated against BOTH binaries (pre-fix `088c202a` built in its own
/// `CARGO_TARGET_DIR`, and this branch's), same fixture, same debug profile, `taskset`-pinned to
/// simulate small CI hardware:
///
/// | CPUs | pre-fix sys/user | post-fix sys/user |
/// |---|---|---|
/// | 2  | 0.0268 – 0.0345 | 0.0013 – 0.0019 |
/// | 4  | 0.0803 – 0.0858 | 0.0006 – 0.0029 |
/// | 12 | 0.2246          | 0.0023          |
///
/// 0.015 is the only value with ≥1.8x margin on BOTH sides at every core count down to 2. The
/// previous 0.03 was calibrated at the default worker count on a 12-core box and, measured, could not
/// discriminate at all on 4 CPUs (pre-fix 0.0018 vs post-fix 0.0019 — the herd never formed, so the
/// gate would have passed a fully regressed binary).
#[cfg(unix)]
const MAX_SYS_OVER_USER: f64 = 0.015;

/// W8-7 — with many more workers than tasks (`HERD_WORKERS`, driven explicitly), a reduction-budget
/// preemption must not thundering-herd every idle worker. A FLAT top-level `parallel:` (never
/// nested — a nested eager scope farms no pool helpers at all and is capped at 2 runners regardless
/// of worker count, so it would never raise this herd and would give a false green), 4 real
/// CPU-bound prime-counting tasks (branchy/modulo work, mirroring `examples/primes_parallel.chz`'s
/// shape — not a trivial arithmetic loop, so the reduction-preemption rate is realistic), sized to
/// ~1s wall on the DEBUG binary `cargo test` builds (`CARGO_BIN_EXE_chezzi`, not `--release`).
///
/// Threshold is derived from THIS box's measured DEBUG-binary numbers, not the 0.25 the row itself
/// was measured with on a RELEASE binary with a much bigger workload (`examples/primes_parallel.chz`
/// full 2,000,000 range: sys/user = 10.11/32.60 = 0.31 at default, 10.73/29.52 = 0.36 at
/// `--threads=12`, vs 0.38/17.67 = 0.02 at `--threads=4` where there's no idle-worker herd to wake).
/// Debug's per-op cost is much higher than release's (same total preemption *count* for the same
/// total op count → near-identical absolute `sys`, spread over far more `user`), so the ratio that
/// manifests on THIS fixture at debug-binary speed is smaller in absolute terms even though it's the
/// same bug. Measured pre-fix on this box (3 runs of this exact fixture): sys/user =
/// 0.38/3.86=0.098, 0.30/4.19=0.072, 0.41/3.88=0.106 (RED capture: 0.339207/3.830044=0.0886, see
/// report). Post-fix (3 runs): 0.01/3.04=0.003, 0.00/2.95=0.0, 0.00/3.32=0.0. Those numbers were
/// taken at the DEFAULT worker count on this 12-core box; the threshold actually shipped
/// (`MAX_SYS_OVER_USER`) is the later cross-hardware calibration on its own doc, which supersedes
/// them — this paragraph is kept as the dated record of how the fixture was first sized.
#[cfg(unix)]
#[test]
fn many_idle_workers_do_not_thundering_herd_on_yield() {
    // The herd needs at least two CPUs to form at all — on a genuine 1-vCPU box nothing runs
    // concurrently with the waking workers, so neither binary produces a signal and a threshold
    // there would be vacuous. Everywhere else this gate is hardware-INDEPENDENT, because it drives
    // `CHEZZI_THREADS` explicitly (`HERD_WORKERS`) instead of inheriting the core count: measured,
    // 28 idle workers raise the pre-fix herd on 2 CPUs just as they do on 12. The earlier shape of
    // this test skipped below 8 cores, which meant it never ran on CI at all (`.github/workflows/
    // ci.yml` uses `ubuntu-latest`) and did so SILENTLY, since libtest captures stderr on a passing
    // test.
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if cores < 2 {
        eprintln!(
            "SKIP: {cores} CPU — the idle-worker herd cannot form on a single CPU, so neither a \
             regressed nor a fixed binary produces a signal here. Visible only under `--nocapture`."
        );
        return;
    }

    let dir = std::env::temp_dir().join(format!("chz-threads-w8-7-sys-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("primes_flat_parallel.chz");
    std::fs::write(
        &path,
        "\
fn is_prime(n: int) -> bool:\n    \
    if n < 2:\n        \
        return false\n    \
    i := 2\n    \
    while i * i <= n:\n        \
        if n % i == 0:\n            \
            return false\n        \
        i += 1\n    \
    return true\n\n\
fn count_primes(lo: int, hi: int) -> int:\n    \
    c := 0\n    \
    n := lo\n    \
    while n < hi:\n        \
        if is_prime(n):\n            \
            c += 1\n        \
        n += 1\n    \
    return c\n\n\
fn worker(lo: int, hi: int, out: Channel[int]):\n    \
    out.send(count_primes(lo, hi))\n\n\
fn main():\n    \
    out := Channel[int]()\n    \
    parallel:\n        \
        spawn worker(2, 32000, out)\n        \
        spawn worker(32000, 64000, out)\n        \
        spawn worker(64000, 96000, out)\n        \
        spawn worker(96000, 128000, out)\n    \
    total := 0\n    \
    for _ in 0..4:\n        \
        total += out.recv()\n    \
    print(\"primes: {total}\")\n\n\
main()\n",
    )
    .expect("write program");

    // W8-7's trigger is IDLE WORKERS, not cores: every preemption used to broadcast to all W-1 of
    // them. Driving `CHEZZI_THREADS` EXPLICITLY (rather than leaving it unset and inheriting the
    // core count) is what makes this gate hardware-independent — measured, 28 idle workers raise the
    // pre-fix herd on 2 CPUs just as they do on 12.
    let (wall, user, sys, status, stdout) =
        child_rusage::run_timed(&["run", path.to_str().unwrap()], &HERD_WORKERS.to_string());

    assert!(
        status.success(),
        "chezzi run must exit 0 (wall={wall:?} user={user:?} sys={sys:?}): {stdout}"
    );
    assert_eq!(
        stdout.trim(),
        "primes: 11987",
        "wrong result — the fixture or the engine is broken, not just slow (wall={wall:?})"
    );
    // Negative control: `user` must be non-trivial, or a near-zero/near-zero ratio could pass by
    // doing no real work at all.
    assert!(
        user > std::time::Duration::from_millis(200),
        "program finished too fast (user={user:?}) to be a meaningful measurement — recalibrate"
    );

    let ratio = sys.as_secs_f64() / user.as_secs_f64();
    assert!(
        ratio < MAX_SYS_OVER_USER,
        "a worker count far above the task count must not thundering-herd on every preemption: \
         sys={sys:?} user={user:?} wall={wall:?} ratio={ratio:.4} (must be < {MAX_SYS_OVER_USER}). A high ratio means \
         MnSched::yield_fiber is still notify_all-ing every idle worker on every reduction-budget \
         preemption (W8-7)."
    );

    let _ = std::fs::remove_dir_all(&dir);
}
