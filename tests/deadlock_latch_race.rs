//! The deadlock verdict and the `--timeout` deadline timer race for one parked task. The verdict
//! reads "every task asleep" and then latches; the timer (`MnSched::arm_deadline_wake`) is a waker
//! the count does not hold, so it can requeue the task in between. Whichever latches first wins:
//! a verdict that read every task asleep lists that task, and a timer that fires after it requeues
//! nothing. The debug-only `CHEZZI_TEST_DEADLOCK_LATCH_WINDOW_MS` hook holds that window open past
//! the deadline, so every run lands the timer inside it.
//!
//! Each program deadlocks before its 300 ms deadline, so the deadlock is the verdict (Go:
//! `fatal error: all goroutines are asleep - deadlock!` as soon as the last goroutine blocks).

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HEADER: &str = "deadlock: every task is blocked";

/// Main alone parks on an empty channel.
const MAIN_RECV: &str = "test fn t():
    ch := Channel[int](1)
    v := ch.recv()
    assert false, \"REACHED {v}\"
";

/// The `hard_timeout-get` shape of `executor_task_grid::a_hard_abort_in_a_job_is_never_a_task_outcome`
/// with a job that blocks instead of spinning: main parks at `t.get()`, the job at its `recv()`.
const JOB_GET: &str = "import Executor, Task from std.concurrency
fn stuck() -> int:
    ch := Channel[int](1)
    return ch.recv() ?? 0
test fn t():
    ex := Executor()
    t := ex.submit(stuck)
    v := t.get()
    assert false, \"REACHED get {v}\"
";

/// Runs `chezzi test --timeout=300` on `src` at `threads` workers with the latch window held
/// 500 ms. Returns the exit code (`None` when killed at nine seconds) and stdout + stderr.
fn run(name: &str, src: &str, threads: &str) -> (Option<i32>, String) {
    let dir = std::env::temp_dir().join(format!("chz-latch-race-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("cell_test.chz");
    std::fs::write(&path, src).expect("write program");
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .arg("test")
        .arg("--timeout=300")
        .arg(&path)
        .env("CHEZZI_THREADS", threads)
        .env("CHEZZI_TEST_DEADLOCK_LATCH_WINDOW_MS", "500")
        .env_remove("CHEZZI_SCHED_SEED")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let start = Instant::now();
    let code = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s.code().unwrap_or(-1));
        }
        if start.elapsed() > Duration::from_secs(9) {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let out = child.wait_with_output().expect("output");
    let _ = std::fs::remove_dir_all(&dir);
    (
        code,
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

#[test]
fn a_deadline_timer_inside_the_latch_window_never_hides_the_deadlock() {
    let mut misses = Vec::new();
    for (name, src) in [("main_recv", MAIN_RECV), ("job_get", JOB_GET)] {
        for threads in ["1", "2"] {
            for rep in 0..3 {
                let (code, report) = run(&format!("{name}-{threads}-{rep}"), src, threads);
                if code != Some(1)
                    || !report.contains(HEADER)
                    || report.contains("TIMED-OUT")
                    || report.contains("REACHED")
                {
                    misses.push(format!(
                        "{name} T={threads} rep={rep}: code={code:?}\n{report}"
                    ));
                }
            }
        }
    }
    assert!(
        misses.is_empty(),
        "{} red run(s):\n{}",
        misses.len(),
        misses.join("\n")
    );
}
