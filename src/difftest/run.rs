//! Shell out to both engines, capture their output, and classify the result.
//!
//! The runner writes the two source renderings to temp files, runs `chezzi run <f>` and
//! `python3 <f>` under a wall-clock timeout, then compares stdout. Known-by-design
//! divergences are downgraded to `AllowListed` (see `allowlist`).

use super::allowlist;
use super::ast::Program;
use super::{emit_chezzi, emit_python};
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
    pub code: Option<i32>, // None => killed by signal / timeout
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DivKind {
    /// Both ran to exit 0 but printed different stdout.
    Stdout,
    /// Chezzi failed (non-zero exit) while Python succeeded.
    ChezziFault,
    /// Chezzi succeeded while Python failed (usually our generator emitting something
    /// Python rejects — a harness bug, not a Chezzi bug, but worth surfacing).
    PythonFault,
}

#[derive(Clone, Debug)]
pub enum Outcome {
    Match,
    AllowListed(&'static str),
    Divergence {
        kind: DivKind,
        chz: Capture,
        py: Capture,
    },
    /// Chezzi exited non-zero with a *Rust host panic* on stderr — always a bug.
    HostPanic {
        chz: Capture,
    },
    Timeout {
        which: &'static str,
    },
    /// Both engines errored (no host panic). Usually the generator emitted something outside
    /// the shared subset — not a Chezzi bug. Non-finding.
    BothError,
}

impl Outcome {
    pub fn is_finding(&self) -> bool {
        matches!(self, Outcome::Divergence { .. } | Outcome::HostPanic { .. })
    }
}

/// Config for a run. `chezzi_bin` is the path to the built binary; `python` defaults to
/// `python3` (override via `CHEZZI_DIFFTEST_PYTHON`).
pub struct Config {
    pub chezzi_bin: PathBuf,
    pub python: String,
    pub timeout: Duration,
}

impl Config {
    pub fn new(chezzi_bin: impl Into<PathBuf>) -> Self {
        Config {
            chezzi_bin: chezzi_bin.into(),
            python: std::env::var("CHEZZI_DIFFTEST_PYTHON").unwrap_or_else(|_| "python3".into()),
            timeout: Duration::from_secs(10),
        }
    }
}

/// Render, run, and diff a program. Returns the classified outcome plus the two source
/// strings (so a finding can be reported / reproduced).
pub fn run_program(cfg: &Config, p: &Program) -> (Outcome, String, String) {
    let chz_src = emit_chezzi::emit(p);
    let py_src = emit_python::emit(p);
    let outcome = run_sources(cfg, &chz_src, &py_src, Some(p));
    (outcome, chz_src, py_src)
}

/// Run pre-rendered sources. `prog` is optional context for the allow-list.
pub fn run_sources(cfg: &Config, chz_src: &str, py_src: &str, prog: Option<&Program>) -> Outcome {
    let dir = std::env::temp_dir().join("chezzi-difftest");
    let _ = std::fs::create_dir_all(&dir);
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let chz_path = dir.join(format!("p_{pid}_{n}.chz"));
    let py_path = dir.join(format!("p_{pid}_{n}.py"));
    if write_file(&chz_path, chz_src).is_err() || write_file(&py_path, py_src).is_err() {
        return Outcome::BothError; // cannot stage temp files — non-finding
    }

    let chz = match run_one(
        Command::new(&cfg.chezzi_bin).arg("run").arg(&chz_path),
        cfg.timeout,
    ) {
        Some(c) => c,
        None => {
            cleanup(&chz_path, &py_path);
            return Outcome::Timeout { which: "chezzi" };
        }
    };
    let py = match run_one(Command::new(&cfg.python).arg(&py_path), cfg.timeout) {
        Some(c) => c,
        None => {
            cleanup(&chz_path, &py_path);
            return Outcome::Timeout { which: "python" };
        }
    };
    cleanup(&chz_path, &py_path);

    classify(chz, py, prog)
}

fn classify(chz: Capture, py: Capture, prog: Option<&Program>) -> Outcome {
    let chz_ok = chz.code == Some(0);
    let py_ok = py.code == Some(0);

    if chz_ok && py_ok {
        if chz.stdout == py.stdout {
            return Outcome::Match;
        }
        if let Some(reason) = allowlist::check(prog, &chz, &py) {
            return Outcome::AllowListed(reason);
        }
        return Outcome::Divergence {
            kind: DivKind::Stdout,
            chz,
            py,
        };
    }

    if !chz_ok {
        // A Rust host panic is the single highest-value finding — never an allow-list case.
        if is_host_panic(&chz.stderr) {
            return Outcome::HostPanic { chz };
        }
        if py_ok {
            // Chezzi faulted on a program Python ran cleanly — a real divergence.
            if let Some(reason) = allowlist::check(prog, &chz, &py) {
                return Outcome::AllowListed(reason);
            }
            return Outcome::Divergence {
                kind: DivKind::ChezziFault,
                chz,
                py,
            };
        }
        // both failed, no host panic — generator produced something outside the shared subset
        return Outcome::BothError;
    }

    // chz_ok && !py_ok — almost always a harness/generator quirk (Python rejected the
    // rendering); surface so we can fix the emitter, but it is not a Chezzi bug.
    if let Some(reason) = allowlist::check(prog, &chz, &py) {
        return Outcome::AllowListed(reason);
    }
    Outcome::Divergence {
        kind: DivKind::PythonFault,
        chz,
        py,
    }
}

/// A genuine Rust-level panic (vs a clean Chezzi runtime fault rendered by `vm::format_trace`).
///
/// Match only the canonical panic marker `panicked at`. Broader substrings like
/// `"index out of bounds"` / `"attempt to "` also appear in *clean* Chezzi runtime-fault
/// messages — matching them would (a) mislabel an ordinary fault as a host panic and, worse,
/// (b) promote a both-engines-errored case (a non-finding) into a `HostPanic` finding. A real
/// Rust arithmetic-overflow / index panic always carries `panicked at`, so nothing is lost.
fn is_host_panic(stderr: &str) -> bool {
    stderr.contains("panicked at")
}

fn write_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(contents.as_bytes())
}

fn cleanup(a: &std::path::Path, b: &std::path::Path) {
    let _ = std::fs::remove_file(a);
    let _ = std::fs::remove_file(b);
}

/// Run a configured command with a wall-clock timeout. Returns `None` on timeout (after
/// killing the child).
///
/// stdout/stderr are drained on dedicated threads so a child that fills an OS pipe buffer
/// before exiting cannot deadlock the poll loop.
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
                // try_wait failed — reap the child and join the readers so we never leak a
                // zombie process, its pipe fds, or the two reader threads.
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
