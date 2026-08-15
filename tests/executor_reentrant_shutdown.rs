//! C1 fix regression, subprocess-only: a task that calls `shutdown_now()` on its own `Executor`
//! mid-drain (reentrant, from inside a job that `Executor` itself is running).
//!
//! **Why this lives in `tests/` and not `src/vm/tests.rs`.** The in-process suite shares one
//! bounded process-wide worker pool across the whole `cargo test --lib` run, which was task 2's
//! original reason for deleting the old in-process version of this test. A fresh process gets its
//! own full-size pool, matching how a real program actually runs.
//!
//! **Reference: CPython 3.14.6.** The `Executor` here dispatches each `submit` EAGERLY to a worker
//! (docs/gaps.md N-family; also `gc_tests::eager_executor_self_capturing_closure_survives_gc_stress_parallel`),
//! so by the time the `stop` job's `shutdown_now()` runs, jobs A and C have typically already
//! started and cannot be cancelled — only jobs not yet dispatched are discarded. `ThreadPoolExecutor`
//! behaves the same way (`submit` hands off to a worker immediately if one is free); the direct
//! Python equivalent —
//! ```python
//! from concurrent.futures import ThreadPoolExecutor
//! def stop(ex): ex.shutdown(wait=False, cancel_futures=True)
//! def main():
//!     ex = ThreadPoolExecutor()
//!     ex.submit(lambda: print("A"))
//!     ex.submit(lambda: stop(ex))
//!     ex.submit(lambda: print("C"))
//!     ex.shutdown(wait=True)
//!     print("end")
//! main()
//! ```
//! measured 40/40 identical: `A\nC\nend\n`, exit 0. That is the reference this test pins against,
//! and the reason it exists — Chezzi's `Executor` must not drift from the ancestor it models.
//!
//! **This was a retry loop until the self-join fix.** It tolerated an intermittent
//! `waiting for this Executor's jobs: deadlock` (measured 7/60 on the debug binary, 0/60 on
//! release) as a "known unrelated race". It was neither unrelated nor a mere flake: a job joining
//! the executor it is running under was waiting for its OWN outstanding slot to clear, so its
//! blocked-party entry was permanently unsatisfiable and the process-wide verdict called a healthy
//! run dead. `Vm::join_eager_jobs` now discounts the joiner's own job; the retry is gone and this
//! is a bare single-run assertion again. See the `Join` variant in `vm::quiesce`.
//!
//! **What changed from the old (now-removed cooperative-engine) pin.** The deleted in-process test
//! asserted `"A\nend\n"`: on the cooperative single-thread engine, jobs were NOT eagerly dispatched,
//! so `stop`'s `shutdown_now()` reached the queue before job C had even started and discarded it.
//! M:N's eager dispatch means C has already started by then, so it runs — and so does CPython's.

use std::process::Command;

#[test]
fn executor_reentrant_shutdown_now_during_drain() {
    let dir = std::env::temp_dir().join(format!(
        "chz-executor-reentrant-shutdown-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(
        &path,
        "import std.concurrency\nfn stop(e: Executor):\n    e.shutdown_now()\nfn main():\n    ex := Executor()\n    ex.submit(fn(): print(\"A\"))\n    ex.submit(fn(): stop(ex))\n    ex.submit(fn(): print(\"C\"))\n    ex.shutdown()\n    print(\"end\")\nmain()\n",
    )
    .expect("write program");

    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("run")
        .arg(&path)
        .output()
        .expect("run chezzi");
    let _ = std::fs::remove_dir_all(&dir);

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "reentrant shutdown_now during drain must not fault: status {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status.code(),
    );
    assert_eq!(
        stdout, "A\nC\nend\n",
        "reentrant shutdown_now during drain: wrong output/order (see module doc for the CPython reference)"
    );
}
