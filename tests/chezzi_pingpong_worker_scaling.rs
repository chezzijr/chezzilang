//! W13-25 (`docs/gaps.md`): a flat two-task ping-pong (`Channel[int](0)` unbuffered send/recv,
//! `parallel: spawn / spawn`) gets SLOWER as the worker pool grows, where the Go twin
//! (`sync.WaitGroup`, two unbuffered channels) stays flat from `GOMAXPROCS=2` to 28. Same idle-worker
//! family as W8-7, but W8-7's fix (dropping `notify_all` on preemption) did not close this one: the
//! cliff is measured pre-existing on `d5bfa5ab`, after that fix landed.
//!
//! **Deliberately its own target**, same reasoning as `tests/chezzi_threads_sys_time.rs`: this
//! fixture burns real wall time (~1s-~10s) at high worker counts, which would destabilize timing
//! gates sharing a target under `RUST_TEST_THREADS`.

#[cfg(unix)]
#[path = "support/child_rusage.rs"]
mod child_rusage;

/// W13-25 — the two worker counts this gate compares. `LOW` matches the task count (no idle
/// workers); `HIGH` sits far above it, the regime the row measures the cliff in.
#[cfg(unix)]
const LOW_WORKERS: &str = "2";
#[cfg(unix)]
const HIGH_WORKERS: &str = "28";

/// W13-25 — message count. Sized so `LOW_WORKERS` finishes in about a second on the DEBUG binary
/// `cargo test` builds (measured on this box: 20,000 round-trips ≈ 1.0s at 2 workers, ≈ 9.5s at 28
/// workers, repeated twice with < 8% spread each side).
#[cfg(unix)]
const ROUND_TRIPS: u32 = 20_000;

/// W13-25 — the flat-parallel ceiling. Go's twin is flat (ratio ≈ 1.0-1.4x) from 2 to 28
/// `GOMAXPROCS`; measured pre-fix Chezzi on this box was ≈ 9.4x-9.6x (2 runs). 3x is well above any
/// plausible flat-engine noise and well below the measured regression, so it discriminates a fixed
/// engine from a regressed one without being noise-sensitive.
#[cfg(unix)]
const MAX_HIGH_OVER_LOW_RATIO: f64 = 3.0;

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

    let (wall_low, _user_low, _sys_low, status_low, stdout_low) =
        child_rusage::run_timed(&args, LOW_WORKERS);
    assert!(
        status_low.success(),
        "chezzi run at {LOW_WORKERS} workers must exit 0 (wall={wall_low:?}): {stdout_low}"
    );
    assert_eq!(
        stdout_low.trim(),
        "done",
        "wrong output at {LOW_WORKERS} workers"
    );

    let (wall_high, _user_high, _sys_high, status_high, stdout_high) =
        child_rusage::run_timed(&args, HIGH_WORKERS);
    assert!(
        status_high.success(),
        "chezzi run at {HIGH_WORKERS} workers must exit 0 (wall={wall_high:?}): {stdout_high}"
    );
    assert_eq!(
        stdout_high.trim(),
        "done",
        "wrong output at {HIGH_WORKERS} workers"
    );

    assert!(
        wall_low > std::time::Duration::from_millis(200),
        "program finished too fast at {LOW_WORKERS} workers (wall={wall_low:?}) to be a meaningful \
         measurement — recalibrate ROUND_TRIPS"
    );

    let ratio = wall_high.as_secs_f64() / wall_low.as_secs_f64();
    assert!(
        ratio < MAX_HIGH_OVER_LOW_RATIO,
        "a flat two-task ping-pong must not get slower as the worker pool grows: \
         wall@{LOW_WORKERS}={wall_low:?} wall@{HIGH_WORKERS}={wall_high:?} ratio={ratio:.2} \
         (must be < {MAX_HIGH_OVER_LOW_RATIO}). The Go twin stays flat across this range (W13-25); \
         a high ratio means Chezzi's M:N scheduler still degrades with idle workers on this workload."
    );

    let _ = std::fs::remove_dir_all(&dir);
}
