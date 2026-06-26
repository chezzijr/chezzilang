//! Shell out to `chezzi check <tmpfile>` on a candidate input, capture its output under a
//! wall-clock timeout, and classify the result.
//!
//! This mirrors `src/difftest/run.rs`'s subprocess machinery (dedicated reader-thread drain +
//! `try_wait` poll + kill-on-timeout — the mandatory anti-pipe-deadlock pattern) but runs a single
//! command and asks one question: did the front-end (`lexer` → `parser` → `checker`, invoked by
//! `chezzi check`) **crash** on this input? The crash-safety invariant is: malformed input must
//! produce a clean diagnostic, never a Rust panic or a signal kill. (This is a deliberate copy, not
//! an import — `src/difftest/` is owned by a parallel task and must not be touched.)

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct Capture {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>, // None => killed by signal (SIGSEGV/SIGABRT/stack-overflow)
}

/// Classified result of feeding one candidate input to `chezzi check`.
#[derive(Clone, Debug)]
pub enum Outcome {
    /// Exit 0, or a non-zero exit with a clean diagnostic and no panic marker. Non-finding.
    Clean { cap: Capture },
    /// stderr carried the canonical Rust panic marker `panicked at` — a host panic. BUG.
    HostPanic { cap: Capture },
    /// Exit code is `None` — the child was killed by a signal (SIGSEGV / SIGABRT /
    /// stack-overflow). BUG.
    Crash { cap: Capture },
    /// Wall-clock timeout (the child was killed). NOT a finding — a slow input, not a crash.
    Timeout,
}

impl Outcome {
    pub fn is_finding(&self) -> bool {
        matches!(self, Outcome::HostPanic { .. } | Outcome::Crash { .. })
    }
}

/// Config for a run. `chezzi_bin` is the path to the built binary; `timeout` is the per-input
/// wall-clock budget.
pub struct Config {
    pub chezzi_bin: PathBuf,
    pub timeout: Duration,
}

impl Config {
    pub fn new(chezzi_bin: impl Into<PathBuf>) -> Self {
        Config {
            chezzi_bin: chezzi_bin.into(),
            timeout: Duration::from_secs(10),
        }
    }
}

/// Write the raw candidate bytes to a temp `.chz` file, run `chezzi check <file>` under the
/// timeout, clean up, and classify. Returns `Outcome::Timeout` (a non-finding) if the staging
/// failed or the run timed out.
pub fn run_input(cfg: &Config, input: &[u8]) -> Outcome {
    let dir = std::env::temp_dir().join("chezzi-panicfuzz");
    let _ = std::fs::create_dir_all(&dir);
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = dir.join(format!("p_{pid}_{n}.chz"));
    if write_file(&path, input).is_err() {
        return Outcome::Timeout; // cannot stage the temp file — treat as a non-finding skip
    }

    let cap = run_one(
        Command::new(&cfg.chezzi_bin).arg("check").arg(&path),
        cfg.timeout,
    );
    cleanup(&path);

    classify(cap)
}

/// Classify a captured run. Exposed so a non-tautology unit test can drive synthetic captures.
///
/// - `None`             => `Timeout` (the child was killed at the wall-clock budget). Non-finding.
/// - `panicked at`      => `HostPanic` (a Rust host panic). BUG.
/// - `code == None`     => `Crash` (killed by a signal: SIGSEGV/SIGABRT/stack-overflow). BUG.
/// - otherwise          => `Clean` (exit 0 or a clean non-zero diagnostic). Non-finding.
pub fn classify(cap: Option<Capture>) -> Outcome {
    let cap = match cap {
        Some(c) => c,
        None => return Outcome::Timeout,
    };
    if is_host_panic(&cap.stderr) {
        return Outcome::HostPanic { cap };
    }
    if cap.code.is_none() {
        return Outcome::Crash { cap };
    }
    Outcome::Clean { cap }
}

/// A genuine Rust-level panic (vs a clean Chezzi diagnostic). Match only the canonical panic
/// marker `panicked at`: broader substrings (`index out of bounds`, `attempt to `) also occur in
/// *clean* Chezzi diagnostics, so matching them would mislabel an ordinary error as a host panic. A
/// real Rust arithmetic-overflow / index panic always carries `panicked at`, so nothing is lost.
fn is_host_panic(stderr: &str) -> bool {
    stderr.contains("panicked at")
}

fn write_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(contents)
}

fn cleanup(p: &std::path::Path) {
    let _ = std::fs::remove_file(p);
}

/// Run a configured command with a wall-clock timeout. Returns `None` on timeout / `try_wait`
/// error (after killing the child); `Some(Capture)` otherwise (`code == None` => signal kill).
///
/// stdout/stderr are drained on dedicated threads so a child that fills an OS pipe buffer before
/// exiting cannot deadlock the poll loop.
fn run_one(cmd: &mut Command, timeout: Duration) -> Option<Capture> {
    use std::io::Read;

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let mut out_pipe = child.stdout.take()?;
    let mut err_pipe = child.stderr.take()?;
    let out_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Reader threads unblock once the pipes close on kill.
                    let _ = out_h.join();
                    let _ = err_h.join();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(_) => {
                // try_wait failed — reap the child and join the readers so we never leak a zombie
                // process, its pipe fds, or the two reader threads.
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_h.join();
                let _ = err_h.join();
                return None;
            }
        }
    };

    let stdout = out_h.join().ok()?;
    let stderr = err_h.join().ok()?;
    Some(Capture {
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        code: status.code(),
    })
}
