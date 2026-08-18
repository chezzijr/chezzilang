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
//! `chezzi_threads_cli.rs`'s W8-8 timing tests (which assert a >5.5x serialization ratio) measurably
//! destabilized them under `RUST_TEST_THREADS=4` — reproduced directly: 1 failure in 4 runs of the
//! combined file (`threads_one_serializes_cpu_bound_parallel_tasks` dropped to ratio 4.34, under its
//! 5.5 floor) vs 0 failures once separated. A separate target sidesteps the interference entirely
//! without touching the W8-8 tests (out of scope here).

#[cfg(unix)]
use std::process::Command;

/// Per-child wall/user/sys via `libc::wait4` on the spawned pid, rather than `Child::wait()` (which
/// surfaces no rusage) or `getrusage(RUSAGE_CHILDREN)` (which would aggregate every child this test
/// binary has ever reaped — contaminated by any other subprocess-spawning test sharing the process).
/// stdout/stderr are drained on background threads before the blocking `wait4` call, so a chatty
/// child can't deadlock on a full pipe buffer while we wait.
// `wait4` IS the reap (it's `waitpid` + rusage in one syscall) — clippy can't see that, only that
// `Child::wait()`/`.output()` was never called on `child`.
#[allow(clippy::zombie_processes)]
#[cfg(unix)]
fn run_timed(
    args: &[&str],
) -> (
    std::time::Duration,
    std::time::Duration,
    std::time::Duration,
    std::process::ExitStatus,
    String,
) {
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.args(args);
    // The defect is specifically about the DEFAULT (unset) worker count — all cores.
    cmd.env_remove("CHEZZI_THREADS");
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let wall_start = std::time::Instant::now();
    let mut child = cmd.spawn().expect("spawn chezzi");
    let pid = child.id() as libc::pid_t;

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stdout_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout_pipe.read_to_string(&mut s);
        s
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });

    let mut status: libc::c_int = 0;
    let mut rusage: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `pid` was just returned by `child.id()` for a process we own and have not yet waited
    // on; `&mut status`/`&mut rusage` are valid, appropriately-sized out-params for the call.
    let ret = unsafe { libc::wait4(pid, &mut status, 0, &mut rusage) };
    assert_eq!(ret, pid, "wait4({pid}) failed");
    let wall = wall_start.elapsed();

    let stdout = stdout_reader.join().expect("stdout reader thread");
    let _stderr = stderr_reader.join().expect("stderr reader thread");

    let user = std::time::Duration::new(
        rusage.ru_utime.tv_sec as u64,
        (rusage.ru_utime.tv_usec as u32) * 1000,
    );
    let sys = std::time::Duration::new(
        rusage.ru_stime.tv_sec as u64,
        (rusage.ru_stime.tv_usec as u32) * 1000,
    );
    (
        wall,
        user,
        sys,
        std::process::ExitStatus::from_raw(status),
        stdout,
    )
}

/// W8-7 — at the DEFAULT worker count (all cores, `CHEZZI_THREADS` unset), a reduction-budget
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
/// report). Post-fix (3 runs): 0.01/3.04=0.003, 0.00/2.95=0.0, 0.00/3.32=0.0. `0.03` sits with a
/// ~2.4x margin below the pre-fix floor and a ~9x margin above the post-fix ceiling.
#[cfg(unix)]
#[test]
fn default_worker_count_does_not_thundering_herd_on_yield() {
    // M6 — the herd this gate detects is O(idle-worker-count^2): with only 4 CPU-bound tasks, a box
    // with too few cores has no idle worker left to thundering-herd at all, so the pre-fix ratio would
    // ALREADY sit under 0.03 with nothing broken. That makes the gate go VACUOUS rather than failing
    // on a fully regressed binary — worse than no gate, since it looks like coverage. Skip loudly
    // below the floor instead of asserting a threshold that can't discriminate there; 8 was chosen so
    // 4 idle workers remain after the 4 tasks (this box measured the fix on 12 cores / 8 idle).
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    if cores < 8 {
        eprintln!(
            "SKIP: only {cores} cores available (need >= 8) — too few idle workers for the \
             thundering-herd this gate detects to raise a signal; the pre-fix ratio would be vacuously \
             low here, not a real pass"
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

    let (wall, user, sys, status, stdout) = run_timed(&["run", path.to_str().unwrap()]);

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
        ratio < 0.03,
        "default worker count must not thundering-herd on every yield_fiber preemption: \
         sys={sys:?} user={user:?} wall={wall:?} ratio={ratio:.4} (must be < 0.03). A high ratio means \
         MnSched::yield_fiber is still notify_all-ing every idle worker on every reduction-budget \
         preemption (W8-7)."
    );

    let _ = std::fs::remove_dir_all(&dir);
}
