//! W13-26 (`docs/gaps.md`): a channel wake broadcasts every PEER sched's idle workers.
//! `nested.chz` wraps a flat two-task ping-pong four `parallel: spawn:` nurseries deep, and each
//! inline nursery owner publishes its own eager sched. `MnSched::wake_run_wide` (`src/vm/mod.rs`)
//! calls `wake_key` on every other live sched once per channel wake, and `wake_key` used to call
//! `notify_waiters()` even when its bucket drain requeued NO fiber — so all four peers woke every
//! idle worker they had, per message, to find nothing runnable and re-park. Counted on the release
//! binary before the fix: 3,199,996 `wake_key` calls and 3,178,667 idle-worker sleeps for 200,000
//! round trips at 8 workers.
//!
//! The originally filed diagnosis — that TICKET-128's handoff is filed into the WAKER's own `wid`
//! `runnext` and then stolen after `HANDOFF_GRACE` — was MEASURED FALSE under TICKET-130: zero
//! `runnext` steals, and all 399,999 handoffs were popped by their own worker. Do not re-derive a
//! fix from it.
//!
//! **Counts the work, not the clock**, same reasoning as `chezzi_pingpong_worker_scaling.rs`: each
//! extra wake this bug causes is a futex park/unpark, which shows up in the child's `ru_nvcsw`
//! (voluntary context switches). This avoids `tests/no_wall_clock_ratio_gates.rs`'s ban on dividing
//! two wall-clock samples.
//!
//! **Deliberately its own target**, same reasoning as `chezzi_pingpong_worker_scaling.rs`: this
//! fixture burns real wall time at the high worker count, which would destabilize timing gates
//! sharing a target under `RUST_TEST_THREADS`.
//!
//! **Self-contained on purpose.** The pipeline gate copies only this file onto a checkout of base to
//! prove the bug is not already fixed there, so it does not reach into `tests/support/` or reuse
//! `chezzi_pingpong_worker_scaling.rs`'s private helper.

/// Runs the built `chezzi` with `args` at `CHEZZI_THREADS=threads` and returns its exit status,
/// stdout, and the child's own voluntary context switch count. Local copy of
/// `chezzi_pingpong_worker_scaling.rs`'s `run_counting_switches` (that file is not a library target,
/// and the gate runs this file standalone against base).
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

/// W13-26 — the two worker counts this gate compares. `LOW` matches the visible task count (the pair
/// plus the four nursery owners fit inside 2 workers with no slot to spread across); `HIGH` is wide
/// enough that the pair and the four owners land on different `wid`s, the regime the row measures the
/// cliff in.
#[cfg(unix)]
const LOW_WORKERS: &str = "2";
#[cfg(unix)]
const HIGH_WORKERS: &str = "8";

/// W13-26 — message count. 20,000 round trips through four nested single-`spawn:` nurseries takes
/// under 2s at either worker count on the DEBUG binary `cargo test` builds.
#[cfg(unix)]
const ROUND_TRIPS: i64 = 20_000;

/// W13-26 — the high-pool count may be at most this multiple of the low-pool count, plus one switch
/// per round trip of absolute headroom. Measured on the debug binary at `d9487bae`, 2026-09-17:
///
/// | workers | nvcsw   |
/// |---------|---------|
/// | 2       | 172,490 |
/// | 8       | 326,014 |
///
/// High is ~1.9x low; base (pre-TICKET-128, broadcast wake) has no such cliff because it never files
/// a handoff into a specific `wid` at all.
#[cfg(unix)]
const MAX_HIGH_OVER_LOW: i64 = 1;

#[cfg(unix)]
#[test]
fn nested_ping_pong_does_not_degrade_as_worker_pool_grows() {
    let dir =
        std::env::temp_dir().join(format!("chz-w13-26-nested-pingpong-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("nested_pingpong.chz");
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
    parallel:\n        \
        spawn:\n            \
            parallel:\n                \
                spawn:\n                    \
                    parallel:\n                        \
                        spawn:\n                            \
                            parallel:\n                                \
                                spawn:\n                                    \
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

    let bound = MAX_HIGH_OVER_LOW * switches_low + ROUND_TRIPS + switches_low / 2;
    assert!(
        switches_high <= bound,
        "a ping-pong nested four `parallel: spawn:` levels deep must not get slower as the worker \
         pool grows: voluntary context switches at {HIGH_WORKERS} workers = {switches_high}, at \
         {LOW_WORKERS} workers = {switches_low}; must be <= {MAX_HIGH_OVER_LOW} x low + 1.5x low + \
         {ROUND_TRIPS} round trips = {bound}. Each nested nursery level publishes its own eager \
         sched, and a channel wake that requeues no fiber on a peer sched must not notify that \
         peer's idle workers (W13-26); an unconditional `notify_waiters` in `MnSched::wake_key` \
         woke every idle worker of every peer once per message."
    );

    let _ = std::fs::remove_dir_all(&dir);
}
