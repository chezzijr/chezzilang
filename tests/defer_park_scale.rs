//! A wait in a `defer` body parks its task at scale (plan 2026-11-07-defer-drain-frames,
//! Acceptance 1): 2000 spawned tasks each `defer ch.recv()`, one sender sends 2000 values after
//! 200 ms, at `--threads=2`. The run must exit 0, print every line, report `demotes=0` under
//! `CHEZZI_SCHED_STATS=1`, keep at most 5 OS threads (Go 1.27: 5 threads, 0.21 s), and finish
//! within 2x the same program with `ch.recv()` in the body, run by the same binary in the same test.
//! Before the plan each deferred wait demoted its worker: about 2005 threads and 38 s.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Spawned tasks, and values the sender sends.
const TASKS: usize = 2000;

/// The most OS threads the defer run may use at `--threads=2` (T + 3, as Go's 5).
const MAX_THREADS: usize = 5;

/// A run past this is killed and fails: the demoting engine crawls for ~38 s.
const LIMIT: Duration = Duration::from_secs(60);

/// The program: each task runs `wait` (`defer ch.recv()` or `ch.recv()`), then prints `x`.
fn program(wait: &str) -> String {
    format!(
        "import time from std
ch := Channel[int](0)

fn f():
    {wait}
    print(\"x\")

fn main():
    parallel:
        for _ in 0..{TASKS}:
            spawn f()
        spawn:
            time.sleep_ms(200)
            for _ in 0..{TASKS}:
                ch.send(1)
main()
"
    )
}

/// What one run did.
struct Probe {
    /// exit code; `None` = killed at [`LIMIT`]
    code: Option<i32>,
    stdout: String,
    stderr: String,
    /// the most entries `/proc/<pid>/task` held while the child ran
    peak_threads: usize,
    wall: Duration,
}

impl Probe {
    /// `demotes=<n>` from the `[sched-stats]` line.
    fn demotes(&self) -> Option<usize> {
        self.stderr
            .lines()
            .filter_map(|l| l.strip_prefix("[sched-stats] "))
            .flat_map(|l| l.split(' '))
            .find_map(|f| f.strip_prefix("demotes="))
            .and_then(|n| n.parse().ok())
    }
}

/// Runs `src` at `--threads=2` under `CHEZZI_SCHED_STATS=1`, sampling the child's thread count
/// every 5 ms until it exits or [`LIMIT`] passes. The sleep is a sampling interval: a peak has no
/// event the child could report.
fn probe(name: &str, src: &str) -> Probe {
    let dir = std::env::temp_dir().join(format!("chz-defer-scale-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("main.chz");
    std::fs::write(&path, src).expect("write program");
    let start = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", "--threads=2", path.to_str().unwrap()])
        .env("CHEZZI_SCHED_STATS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chezzi");
    let pid = child.id();
    // Drain the pipes on their own threads: 2000 lines can fill a pipe before the child exits.
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_reader = std::thread::spawn(move || {
        let mut s = String::new();
        out_pipe.read_to_string(&mut s).unwrap();
        s
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        err_pipe.read_to_string(&mut s).unwrap();
        s
    });
    let mut peak_threads = 0;
    let code = loop {
        if let Some(s) = child.try_wait().expect("wait") {
            break Some(s.code().unwrap_or(-1));
        }
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{pid}/task")) {
            peak_threads = peak_threads.max(entries.count());
        }
        if start.elapsed() > LIMIT {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let wall = start.elapsed();
    let stdout = out_reader.join().unwrap();
    let stderr = err_reader.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    Probe {
        code,
        stdout,
        stderr,
        peak_threads,
        wall,
    }
}

#[cfg(target_os = "linux")]
#[test]
fn two_thousand_tasks_waiting_in_a_defer_park() {
    let body = probe("body", &program("ch.recv()"));
    assert_eq!(body.code, Some(0), "body twin: {}", body.stderr);
    let deferred = probe("defer", &program("defer ch.recv()"));
    let summary = format!(
        "defer run: code={:?} demotes={:?} peak_threads={} wall={:?} (body twin: demotes={:?} \
         peak_threads={} wall={:?}); stderr tail: {:?}",
        deferred.code,
        deferred.demotes(),
        deferred.peak_threads,
        deferred.wall,
        body.demotes(),
        body.peak_threads,
        body.wall,
        deferred.stderr.lines().rev().take(3).collect::<Vec<_>>()
    );
    assert_eq!(deferred.code, Some(0), "{summary}");
    assert_eq!(
        deferred.stdout,
        "x\n".repeat(TASKS),
        "every task prints; {summary}"
    );
    assert!(
        deferred.stderr.contains("[sched-stats] workers=2 "),
        "{summary}"
    );
    assert_eq!(deferred.demotes(), Some(0), "{summary}");
    assert!(deferred.peak_threads <= MAX_THREADS, "{summary}");
    assert!(deferred.wall <= body.wall * 2, "{summary}");
}
