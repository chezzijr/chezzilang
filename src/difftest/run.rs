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

/// What a process actually wrote, as RAW BYTES.
///
/// Not `String`: `String::from_utf8_lossy` is not injective (`ff` and `fe` both become one
/// U+FFFD), so a decoded compare would report `Match` for a run whose two engines put DIFFERENT
/// bytes on fd 1 — and both sides can emit non-UTF-8 (`io.stdout().write_bytes` since W6-9,
/// CPython's `sys.stdout.buffer.write` always). Keeping only the bytes makes the blind compare
/// unrepresentable; [`Capture::stdout_text`] / [`Capture::stderr_text`] decode for *display and
/// text heuristics only*, never for a verdict. Same class as the parity-oracle hole W6-9b closed.
#[derive(Clone, Debug)]
pub struct Capture {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    // `None` means the child was killed by a SIGNAL (SIGSEGV / SIGABRT / a Rust stack overflow).
    // Never "or timeout": `run_one` returns `Result<Capture, RunErr>`, and a timeout is
    // `Err(RunErr::TimedOut)` — returned *before* any `Capture` is constructed (`run_sources`
    // maps it straight to `Outcome::Timeout` right there) — so a live `Capture` reaching
    // `classify` can only have `code: None` from a signal kill.
    pub code: Option<i32>,
}

impl Capture {
    /// stdout decoded for a human (report text, allow-list heuristics). Lossy — never compare
    /// two of these to decide a verdict; compare the `stdout` bytes.
    pub fn stdout_text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// stderr decoded for a human. Same caveat as [`Capture::stdout_text`].
    pub fn stderr_text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DivKind {
    /// Both ran to exit 0 but printed different stdout.
    Stdout,
    /// Chezzi failed with an ordinary non-zero exit while Python succeeded — never a signal
    /// kill or a Rust host panic, both of which `classify` intercepts as `HostPanic` before
    /// this arm is reached.
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
    /// Chezzi crashed at the Rust level: either a `panicked at` marker on stderr (ANY exit
    /// code, including 0 — a background worker thread's panic doesn't change the process's own
    /// exit code) or the process was killed by a signal (`code: None`, no panic text
    /// required). Always a bug.
    HostPanic {
        chz: Capture,
    },
    Timeout {
        which: &'static str,
    },
    /// Both engines errored (no host panic). Usually the generator emitted something outside
    /// the shared subset — not a Chezzi bug. Non-finding.
    BothError,
    /// The oracle itself could not run this seed at all — a child failed to spawn (e.g. the
    /// `chezzi` binary is missing: `ENOENT`), or a temp file could not be staged. `is_finding()`
    /// stays false — this is not a Chezzi bug — but a caller must still treat it as FATAL, not
    /// silently skip it: a differential oracle that never started a child has proven nothing
    /// about either engine, and reporting that as "0 findings" is a false negative dressed up as
    /// a clean pass (see `docs/bug-discovery.md` / `docs/gaps.md` for the concrete trigger).
    HarnessError(String),
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
    if let Err(e) = write_file(&chz_path, chz_src) {
        // `File::create` can succeed and the later `write_all` still fail (transient ENOSPC),
        // leaving a stub `.chz` on disk — `cleanup` removes it (a no-op on the not-yet-created
        // `py_path`). Same class as `run_one`'s `CouldNotRun`: the harness is broken, not a
        // divergence, and every exit from this function must leave the staging dir clean.
        cleanup(&chz_path, &py_path);
        return Outcome::HarnessError(format!("could not write {}: {e}", chz_path.display()));
    }
    if let Err(e) = write_file(&py_path, py_src) {
        // The chz write above already succeeded — clean it up like every other error arm below,
        // or it leaks into the temp dir on every occurrence (e.g. a transient ENOSPC on the
        // second write during a long fuzz run).
        cleanup(&chz_path, &py_path);
        return Outcome::HarnessError(format!("could not write {}: {e}", py_path.display()));
    }

    let chz = match run_one(
        Command::new(&cfg.chezzi_bin).arg("run").arg(&chz_path),
        cfg.timeout,
    ) {
        Ok(c) => c,
        Err(RunErr::TimedOut) => {
            cleanup(&chz_path, &py_path);
            return Outcome::Timeout { which: "chezzi" };
        }
        Err(RunErr::CouldNotRun(msg)) => {
            cleanup(&chz_path, &py_path);
            return Outcome::HarnessError(msg);
        }
    };
    let py = match run_one(Command::new(&cfg.python).arg(&py_path), cfg.timeout) {
        Ok(c) => c,
        Err(RunErr::TimedOut) => {
            cleanup(&chz_path, &py_path);
            return Outcome::Timeout { which: "python" };
        }
        Err(RunErr::CouldNotRun(msg)) => {
            cleanup(&chz_path, &py_path);
            return Outcome::HarnessError(msg);
        }
    };
    cleanup(&chz_path, &py_path);

    classify(chz, py, prog)
}

fn classify(chz: Capture, py: Capture, prog: Option<&Program>) -> Outcome {
    // A Rust host panic is the single highest-value finding this oracle can produce, so it is
    // checked FIRST — before any arm-specific logic and before any `allowlist::check` call —
    // so it can never be allow-listed or buried under a both-failed non-finding. This must run
    // unconditionally (not gated on `!chz_ok`): a panicking *worker thread* (`vm/stream.rs`,
    // `native/request.rs`, `native/rand.rs`, `native/cffi.rs`) doesn't touch the process's own
    // exit code, so a clean `chz.code == Some(0)` proves nothing about stderr.
    if is_host_panic(&chz.stderr_text()) {
        return Outcome::HostPanic { chz };
    }
    // `chz.code` is `None` ONLY when the child was killed by a SIGNAL (SIGSEGV / SIGABRT / a
    // Rust stack overflow, which prints "has overflowed its stack" and dies WITHOUT a
    // `panicked at` marker — the check above misses it). A timeout can never reach here:
    // `run_one` returns `None` on timeout and `run_sources` returns `Outcome::Timeout` before a
    // `Capture` is ever constructed, so a live `Capture` with `code: None` cannot mean "timed
    // out" — see the `Capture::code` doc. Same rule as the twin oracle: `panicfuzz::classify`
    // (`src/panicfuzz/run.rs:98-100`) reports this exact condition as `Outcome::Crash`.
    if chz.code.is_none() {
        return Outcome::HostPanic { chz };
    }
    // Deliberately NOT mirrored on the Python side: a "Python host panic" isn't a thing CPython
    // has, and `py.code.is_none()` (CPython killed by a signal) is real but is not a Chezzi bug
    // to promote to a HostPanic — it already makes `py_ok` false below and falls through to the
    // ordinary `PythonFault` / `BothError` arms. Note `PythonFault` IS a finding (`is_finding()`
    // is true for every `Divergence`) — deliberately so, per its own doc: a CPython crash on our
    // rendering usually means the EMITTER is wrong and is worth surfacing. What this skips is
    // only the HostPanic promotion, which is reserved for a bug in *our* runtime.

    let chz_ok = chz.code == Some(0);
    let py_ok = py.code == Some(0);

    if chz_ok && py_ok {
        // BYTES (see `Capture`) — a decoded compare folds `ff fe` and `fe ff` into the same
        // two-U+FFFD string and would call a genuine divergence a `Match`.
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
        // A host panic (stderr marker OR signal kill) already returned above — never an
        // allow-list case, and never reaches this arm.
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
        // both failed, no host panic, no signal kill — generator produced something outside
        // the shared subset
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

/// Why `run_one` failed. `TimedOut` is an ordinary, expected outcome (a generated program can
/// loop forever) — `run_sources` maps it to `Outcome::Timeout`. `CouldNotRun` means the harness
/// itself is broken (the child never even started, or we lost track of it) — `run_sources` maps
/// it to `Outcome::HarnessError`, which callers must treat as fatal, not score as "no finding".
#[derive(Debug)]
enum RunErr {
    TimedOut,
    /// Carries the underlying `io::Error` text plus the program name, so the message names the
    /// actual problem, e.g. `could not run "chezzi": No such file or directory (os error 2)`.
    CouldNotRun(String),
}

/// Run a configured command with a wall-clock timeout. `Err(RunErr::TimedOut)` on timeout
/// (after killing the child); `Err(RunErr::CouldNotRun(_))` if the child could never be
/// observed at all (spawn failed, a stdio pipe wasn't there to take, or `try_wait` errored).
///
/// stdout/stderr are drained on dedicated threads so a child that fills an OS pipe buffer
/// before exiting cannot deadlock the poll loop.
fn run_one(cmd: &mut Command, timeout: Duration) -> Result<Capture, RunErr> {
    use std::io::Read;

    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| RunErr::CouldNotRun(format!("could not run {program:?}: {e}")))?;

    let mut out_pipe = child
        .stdout
        .take()
        .ok_or_else(|| RunErr::CouldNotRun(format!("{program:?}: stdout pipe was not present")))?;
    let mut err_pipe = child
        .stderr
        .take()
        .ok_or_else(|| RunErr::CouldNotRun(format!("{program:?}: stderr pipe was not present")))?;
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
                    return Err(RunErr::TimedOut);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(e) => {
                // try_wait failed — reap the child and join the readers so we never leak a
                // zombie process, its pipe fds, or the two reader threads.
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_h.join();
                let _ = err_h.join();
                return Err(RunErr::CouldNotRun(format!(
                    "try_wait failed for {program:?}: {e}"
                )));
            }
        }
    };

    let stdout = out_h
        .join()
        .map_err(|_| RunErr::CouldNotRun(format!("{program:?}: stdout reader thread panicked")))?;
    let stderr = err_h
        .join()
        .map_err(|_| RunErr::CouldNotRun(format!("{program:?}: stderr reader thread panicked")))?;
    Ok(Capture {
        stdout,
        stderr,
        code: status.code(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a clean-exit capture from the RAW BYTES a process wrote to stdout. Only the
    /// construction adapts to `Capture`'s field type; the assertions below never change.
    fn cap(stdout: Vec<u8>) -> Capture {
        Capture {
            stdout,
            stderr: Vec::new(),
            code: Some(0),
        }
    }

    /// Build a capture with an explicit exit code and stderr — for tests that exercise the
    /// fault arms `cap` (hardcoded `code: Some(0)`) cannot reach.
    fn cap_exit(stdout: Vec<u8>, stderr: Vec<u8>, code: Option<i32>) -> Capture {
        Capture {
            stdout,
            stderr,
            code,
        }
    }

    /// The CPython differential oracle must diff **bytes**. `String::from_utf8_lossy` is not
    /// injective — `ff fe` and `fe ff` both decode to two U+FFFD — so a decoded compare reports
    /// `Match` for a run where Chezzi and CPython put DIFFERENT bytes on fd 1. Both sides can
    /// emit non-UTF-8 today (`io.stdout().write_bytes` since W6-9; CPython's
    /// `sys.stdout.buffer.write` always could), and this oracle is the one `docs/future.md §2b`
    /// keeps after `--serial` is deleted. Same class as the parity-oracle hole W6-9b closed.
    #[test]
    fn a_byte_only_divergence_is_not_a_match() {
        let chz = cap(vec![0xff, 0xfe]);
        let py = cap(vec![0xfe, 0xff]);
        assert_eq!(
            chz.stdout_text(),
            py.stdout_text(),
            "premise: the lossy decode really does fold these two into one string"
        );
        assert!(
            matches!(
                classify(chz, py, None),
                Outcome::Divergence {
                    kind: DivKind::Stdout,
                    ..
                }
            ),
            "a byte-only stdout divergence must be a Divergence, not a Match"
        );
    }

    /// The report must not contradict the verdict: both decoded stdouts render identically here,
    /// so without the raw-byte line a reader sees "Divergence" over two identical blocks.
    #[test]
    fn a_byte_only_divergence_reports_the_raw_bytes() {
        let outcome = classify(cap(vec![0xff, 0xfe]), cap(vec![0xfe, 0xff]), None);
        let report = super::super::describe(0, &outcome, "<chz>", "<py>");
        // Bind each byte string to its SIDE — `contains` alone passes with the two swapped.
        assert!(
            report.contains("RAW BYTES ONLY")
                && report.contains("chezzi: ff fe")
                && report.contains("python: fe ff"),
            "byte-only divergence report must spell out both byte strings, per side:\n{report}"
        );
    }

    /// W7-31: the float allow-list looked only at stdout, never at exit code, so a Chezzi
    /// FAULT (`1e-05` then exit 1) next to a clean CPython run (`0.00001`, exit 0) was
    /// downgraded to `AllowListed` — a crash reported as a non-finding.
    #[test]
    fn a_chezzi_fault_next_to_a_float_reformat_is_not_allow_listed() {
        let chz = cap_exit(b"1e-05\n".to_vec(), b"runtime error: ...".to_vec(), Some(1));
        let py = cap_exit(b"0.00001\n".to_vec(), Vec::new(), Some(0));
        assert!(
            matches!(
                classify(chz, py, None),
                Outcome::Divergence {
                    kind: DivKind::ChezziFault,
                    ..
                }
            ),
            "a Chezzi fault must never be masked by the float-formatting allow-list"
        );
    }

    /// W7-31, `PythonFault` arm: the same shape must not be allow-listed when it's Python that
    /// faulted, since the excuse is about float *formatting*, and neither side ran to see one.
    #[test]
    fn a_python_fault_next_to_a_float_reformat_is_not_allow_listed() {
        let chz = cap_exit(b"1e-05\n".to_vec(), Vec::new(), Some(0));
        let py = cap_exit(b"0.00001\n".to_vec(), Vec::new(), Some(1));
        assert!(
            matches!(
                classify(chz, py, None),
                Outcome::Divergence {
                    kind: DivKind::PythonFault,
                    ..
                }
            ),
            "a Python fault must never be masked by the float-formatting allow-list"
        );
    }

    // --- F2/F5: a crash must be a crash on every `classify` arm ------------------------------

    /// F2. `chz.code == None` means the child was killed by a SIGNAL (a Rust stack overflow
    /// prints exactly this and dies without a `panicked at` marker). Today `classify` never
    /// looks at `code.is_none()` anywhere, so this fell through to an ordinary `ChezziFault`
    /// divergence — the highest-value finding this oracle can produce, invisible.
    #[test]
    fn a_signal_killed_chezzi_is_a_host_panic() {
        let chz = cap_exit(
            Vec::new(),
            b"\nthread 'main' has overflowed its stack\n".to_vec(),
            None,
        );
        let py = cap_exit(b"ok\n".to_vec(), Vec::new(), Some(0));
        assert!(
            matches!(classify(chz, py, None), Outcome::HostPanic { .. }),
            "a signal-killed chezzi must be a HostPanic even though stderr has no 'panicked at'"
        );
    }

    /// F2, `BothError` shape: today a signal-killed chezzi next to a Python failure is
    /// `BothError` — `is_finding() == false` — which buries a host crash as a non-finding.
    #[test]
    fn a_signal_killed_chezzi_is_a_finding_even_when_python_also_failed() {
        let chz = cap_exit(
            Vec::new(),
            b"\nthread 'main' has overflowed its stack\n".to_vec(),
            None,
        );
        let py = cap_exit(Vec::new(), Vec::new(), Some(1));
        let outcome = classify(chz, py, None);
        assert!(
            matches!(outcome, Outcome::HostPanic { .. }),
            "a signal-killed chezzi must be a HostPanic even when Python also failed, got {outcome:?}"
        );
        assert!(
            outcome.is_finding(),
            "the property that actually matters: this must be a finding"
        );
    }

    /// F5. `classify`'s both-exit-0 arm never consults `is_host_panic` — a worker thread can
    /// panic without touching the process's own exit code, so today this is a `Match`.
    #[test]
    fn a_worker_thread_panic_with_exit_zero_is_a_finding() {
        let chz = cap_exit(
            b"1\n".to_vec(),
            b"thread '<chezzi-worker>' panicked at src/vm/stream.rs:120:\nsomething broke\n"
                .to_vec(),
            Some(0),
        );
        let py = cap_exit(b"1\n".to_vec(), Vec::new(), Some(0));
        assert!(
            matches!(classify(chz, py, None), Outcome::HostPanic { .. }),
            "identical stdout must not hide a worker-thread panic on stderr"
        );
    }

    /// Guard against over-firing: an ordinary Chezzi runtime fault (no `panicked at`, real exit
    /// code, not a signal kill) must stay an ordinary `ChezziFault` divergence, not get
    /// promoted to `HostPanic`. This is exactly the case `is_host_panic`'s own doc comment
    /// warns about — broader substrings like "index out of bounds" also appear in clean faults.
    #[test]
    fn a_clean_chezzi_fault_is_still_an_ordinary_divergence() {
        let chz = cap_exit(
            Vec::new(),
            b"runtime error: index 5 out of range".to_vec(),
            Some(1),
        );
        let py = cap_exit(b"ok\n".to_vec(), Vec::new(), Some(0));
        assert!(
            matches!(
                classify(chz, py, None),
                Outcome::Divergence {
                    kind: DivKind::ChezziFault,
                    ..
                }
            ),
            "an ordinary clean fault must not be promoted to HostPanic"
        );
    }

    /// Guard the both-failed non-finding: two ordinary faults with no panic markers and no
    /// signal kill on either side must stay `BothError`, not get promoted by the new checks.
    #[test]
    fn a_both_ordinary_faults_stays_botherror() {
        let chz = cap_exit(
            Vec::new(),
            b"runtime error: division by zero".to_vec(),
            Some(1),
        );
        let py = cap_exit(Vec::new(), b"ZeroDivisionError: ...".to_vec(), Some(1));
        assert!(
            matches!(classify(chz, py, None), Outcome::BothError),
            "two ordinary faults with no panic marker and no signal kill must stay BothError"
        );
    }

    // --- F1: a harness that cannot even START a child must abort, not score 0 findings -------

    /// F1's concrete trigger: `difffuzz`'s `locate_chezzi()` falls back to the bare name
    /// `"chezzi"` when no sibling binary exists, so a missing build spawns `ENOENT` on every
    /// seed. Today that reaches `run_one`'s `.spawn().ok()?` and comes back as
    /// `Outcome::Timeout { which: "chezzi" }` — a non-finding — which is how a fuzz run over
    /// ZERO executed programs prints "0 finding(s), exit 0". This spawns for real (a bad path
    /// fails immediately, no timeout wait), so it belongs with the other subprocess tests.
    #[test]
    fn a_missing_chezzi_binary_is_a_harness_error_not_a_timeout() {
        let cfg = Config::new("/nonexistent/chezzi-does-not-exist");
        let outcome = run_sources(&cfg, "print(1)\n", "print(1)\n", None);
        match outcome {
            Outcome::HarnessError(msg) => assert!(
                msg.contains("chezzi-does-not-exist") && msg.contains("No such file or directory"),
                "the message must name the actual problem, not just \"failed\": {msg}"
            ),
            other => panic!(
                "expected HarnessError naming the missing binary, got {other:?} \
                 (today this wrongly comes back as Timeout {{ which: \"chezzi\" }})"
            ),
        }
    }

    /// So nobody later "fixes" the abort in `fuzz_range`/`difffuzz` by making this a finding —
    /// it is the harness that is broken, not Chezzi.
    #[test]
    fn a_harness_error_is_not_a_finding() {
        assert!(!Outcome::HarnessError("boom".into()).is_finding());
    }

    /// A REAL timeout must stay `Timeout`, not collapse into `HarnessError` — otherwise the two
    /// tests above would still pass with the distinction deleted. Exercised at the `run_one`
    /// level with `sleep 5` under a 50ms timeout (per the brief: faster and non-flaky vs.
    /// spawning the real `chezzi` binary on an infinite-loop source, and this is what `run_one`
    /// itself is responsible for).
    #[test]
    fn a_real_timeout_is_still_a_timeout_not_a_harness_error() {
        let mut cmd = Command::new("sleep");
        cmd.arg("5");
        let result = run_one(&mut cmd, Duration::from_millis(50));
        assert!(
            matches!(result, Err(RunErr::TimedOut)),
            "expected TimedOut, got {result:?}"
        );
    }
}
