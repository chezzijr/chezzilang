//! W13-25 (`docs/gaps.md`): a flat two-task ping-pong (`Channel[int](0)` unbuffered send/recv,
//! `parallel: spawn / spawn`) gets SLOWER as the worker pool grows, where the Go twin
//! (`sync.WaitGroup`, two unbuffered channels) stays flat from `GOMAXPROCS=2` to 28. Same idle-worker
//! family as W8-7, but W8-7's fix (dropping `notify_all` on preemption) did not close this one: the
//! cliff is measured pre-existing on `d5bfa5ab`, after that fix landed.
//!
//! **Counts the work, not the clock** (TICKET-128). The cost is idle workers woken for a fiber they
//! do not get: each wake is a futex park and unpark, which the kernel counts in the child's
//! `ru_nvcsw` (voluntary context switches). A count does not stretch on a busy box the way a wall
//! time does, and `tests/no_wall_clock_ratio_gates.rs` bans dividing two wall-clock samples.
//!
//! **Deliberately its own target**, same reasoning as `tests/chezzi_threads_sys_time.rs`: this
//! fixture burns real wall time (~1s-~10s) at high worker counts, which would destabilize timing
//! gates sharing a target under `RUST_TEST_THREADS`.
//!
//! **Self-contained on purpose.** The pipeline gate copies only this file onto a checkout of base to
//! prove the bug is not already fixed there. A helper this ticket added under `tests/support/` does
//! not exist in that run, and the target failed to compile. [`run_counting_switches`] is therefore a
//! local copy of `child_rusage::run_timed`'s spawn and `wait4`, reading `ru_nvcsw` and no clock.

/// Runs the built `chezzi` with `args` at `CHEZZI_THREADS=threads` and returns its exit status,
/// stdout, and the child's own voluntary context switch count. Uses `libc::wait4` on the pid, not
/// `getrusage(RUSAGE_CHILDREN)`, so no other child this test binary reaps contaminates the count.
/// stdout/stderr drain on background threads so a chatty child cannot block on a full pipe.
// `wait4` IS the reap (it's `waitpid` + rusage in one syscall) — clippy can't see that, only that
// `Child::wait()`/`.output()` was never called on `child`.
#[allow(clippy::zombie_processes)]
#[cfg(unix)]
fn run_counting_switches(args: &[&str], threads: &str) -> (std::process::ExitStatus, String, i64) {
    use std::io::Read;
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_chezzi"));
    cmd.args(args);
    cmd.env("CHEZZI_THREADS", threads);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

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

    let stdout = stdout_reader.join().expect("stdout reader thread");
    let _stderr = stderr_reader.join().expect("stderr reader thread");

    (
        std::process::ExitStatus::from_raw(status),
        stdout,
        rusage.ru_nvcsw as i64,
    )
}

/// W13-25 — the two worker counts this gate compares. `LOW` matches the task count (no idle
/// workers); `HIGH` sits far above it, the regime the row measures the cliff in.
#[cfg(unix)]
const LOW_WORKERS: &str = "2";
#[cfg(unix)]
const HIGH_WORKERS: &str = "28";

/// W13-25 — message count. 20,000 round trips take about 1s at 2 workers and 10s at 28 workers on
/// the DEBUG binary `cargo test` builds, before the fix.
#[cfg(unix)]
const ROUND_TRIPS: i64 = 20_000;

/// W13-25 — the high-pool count may be at most this multiple of the low-pool count, plus one switch
/// per round trip of absolute headroom (the low count is bimodal on a fixed engine: 10-134 or
/// 65k-91k in the same run of five). Measured on the debug binary, 28-thread box, 2026-09-16:
///
/// | binary                | load | nvcsw@2      | nvcsw@28          |
/// |-----------------------|------|--------------|-------------------|
/// | base `3586bcd6`       | ~1   | 81186-91249  | 3184704-3223482   |
/// | base, 28 spinners     | ~15  | 45420        | 632128            |
/// | runnext-handoff proto | ~4   | 14-89388     | 238-276           |
/// | proto, 28 spinners    | ~7   | 10-134       | 213-656           |
///
/// Base fails the bound by 4x under load and 11x idle; the prototype passes it by more than 15x.
#[cfg(unix)]
const MAX_HIGH_OVER_LOW: i64 = 3;

#[cfg(unix)]
#[test]
fn ping_pong_throughput_does_not_degrade_as_worker_pool_grows() {
    let dir = std::env::temp_dir().join(format!("chz-w13-25-pingpong-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("pingpong.chz");
    std::fs::write(
        &path,
        format!(
            "\
fn pp():\n    \
    ping := Channel[int](0)\n    \
    pong := Channel[int](0)\n    \
    parallel:\n        \
        spawn:\n            \
            for i in range({n}):\n                \
                ping.send(i)\n                \
                n := pong.recv()\n        \
        spawn:\n            \
            for i in range({n}):\n                \
                v := ping.recv()\n                \
                pong.send(v)\n\n\
fn main():\n    \
    pp()\n    \
    print(\"done\")\n\n\
main()\n",
            n = ROUND_TRIPS
        ),
    )
    .expect("write program");

    let args = ["run", path.to_str().unwrap()];

    let (status_low, stdout_low, switches_low) = run_counting_switches(&args, LOW_WORKERS);
    assert!(
        status_low.success(),
        "chezzi run at {LOW_WORKERS} workers must exit 0: {stdout_low}"
    );
    assert_eq!(
        stdout_low.trim(),
        "done",
        "wrong output at {LOW_WORKERS} workers"
    );

    let (status_high, stdout_high, switches_high) = run_counting_switches(&args, HIGH_WORKERS);
    assert!(
        status_high.success(),
        "chezzi run at {HIGH_WORKERS} workers must exit 0: {stdout_high}"
    );
    assert_eq!(
        stdout_high.trim(),
        "done",
        "wrong output at {HIGH_WORKERS} workers"
    );

    let bound = MAX_HIGH_OVER_LOW * switches_low + ROUND_TRIPS;
    assert!(
        switches_high <= bound,
        "a flat two-task ping-pong must not get slower as the worker pool grows: \
         voluntary context switches at {HIGH_WORKERS} workers = {switches_high}, at {LOW_WORKERS} \
         workers = {switches_low}; must be <= {MAX_HIGH_OVER_LOW} x low + {ROUND_TRIPS} round trips \
         = {bound}. The Go twin stays flat across this range (W13-25); a high count means each \
         rendezvous still wakes idle workers that do not get the fiber."
    );

    let _ = std::fs::remove_dir_all(&dir);
}
