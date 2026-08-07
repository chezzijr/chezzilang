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
    /// WHICH signal (`None` when the child exited normally). `code` alone cannot tell a SIGSEGV —
    /// a real front-end bug — from a SIGKILL, which under this project's mandated
    /// `systemd-run … MemoryMax=6G` scope is the cgroup OOM-killer and implicates nothing
    /// (CLAUDE.md: "exit 137 = OOM, not a test failure"). See `docs/gaps.md` W7-38; mirrors
    /// `difftest::run::Capture::signal`.
    pub signal: Option<i32>,
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
    /// The oracle itself could not run this input at all — the child failed to spawn (e.g. the
    /// `chezzi` binary is missing: `ENOENT`), or the candidate could not be staged to a temp
    /// file. `is_finding()` stays false — this is not a Chezzi bug — but a caller must still
    /// treat it as FATAL, not silently skip it: a panic-fuzz sweep that never started a child has
    /// proven nothing about the front-end's crash-safety, and reporting that as "0 findings" is a
    /// false negative dressed up as a clean pass. Mirrors `difftest::run::Outcome::HarnessError`
    /// (`docs/gaps.md` W7-34/W7-35).
    HarnessError(String),
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
/// timeout, clean up, and classify. A real timeout is `Outcome::Timeout` (a non-finding); a
/// harness that could not even start the child (bad staging, spawn failure) is fatal —
/// `Outcome::HarnessError` — never collapsed into `Timeout` (`docs/gaps.md` W7-35).
pub fn run_input(cfg: &Config, input: &[u8]) -> Outcome {
    let dir = std::env::temp_dir().join("chezzi-panicfuzz");
    let _ = std::fs::create_dir_all(&dir);
    let n = NONCE.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = dir.join(format!("p_{pid}_{n}.chz"));
    if let Err(e) = write_file(&path, input) {
        cleanup(&path);
        return Outcome::HarnessError(format!("could not write {}: {e}", path.display()));
    }

    let result = run_one(
        Command::new(&cfg.chezzi_bin).arg("check").arg(&path),
        cfg.timeout,
    );
    cleanup(&path);

    match result {
        Ok(cap) => classify(cap),
        Err(RunErr::TimedOut) => Outcome::Timeout,
        Err(RunErr::CouldNotRun(msg)) => Outcome::HarnessError(msg),
    }
}

/// Classify a captured run. Exposed so a non-tautology unit test can drive synthetic captures.
///
/// - `panicked at`      => `HostPanic` (a Rust host panic). BUG.
/// - `code == None`     => `Crash` (killed by a signal: SIGSEGV/SIGABRT/stack-overflow). BUG.
/// - otherwise          => `Clean` (exit 0 or a clean non-zero diagnostic). Non-finding.
///
/// Takes a `Capture`, never an `Option<Capture>`: "the child did not produce a capture" is routed
/// by `run_input` from the typed `RunErr` (`TimedOut` => `Timeout`, `CouldNotRun` => the FATAL
/// `HarnessError`), and a `None` sentinel here could only collapse those two back together — which
/// is precisely the `W7-35` bug. Matching `difftest::classify(chz: Capture, ..)`, this makes it
/// unrepresentable rather than merely fixed; the two sibling oracles diverging on exactly this
/// shape is the mechanism behind both `W7-33` and `W7-35`.
pub fn classify(cap: Capture) -> Outcome {
    if is_host_panic(&cap.stderr) {
        return Outcome::HostPanic { cap };
    }
    // Only the signals a crash in the child itself raises are a finding. A SIGKILL (cgroup
    // OOM-killer / `kill -9`) or SIGTERM comes from OUTSIDE and implicates nothing about the
    // front-end, so it is the fatal-but-not-a-finding `HarnessError`: this input proved nothing.
    // An unrecognized signal DECLINES the same way rather than emitting a confident "chezzi
    // crashed" (`docs/gaps.md` W7-12, W7-38).
    if let Some(sig) = cap.signal
        && !is_crash_signal(sig)
    {
        return Outcome::HarnessError(outside_kill_msg("chezzi", sig));
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

/// Signals a **bug in the child itself** raises; everything else (SIGKILL from the OOM-killer,
/// SIGTERM from a CI reaper) is an outside kill. Linux numbering (`man 7 signal`); this crate is
/// already Unix-only. Mirrors `difftest::run::is_crash_signal`.
fn is_crash_signal(sig: i32) -> bool {
    matches!(sig, 4 | 5 | 6 | 7 | 8 | 11 | 31)
}

/// Name the signal in reports — "killed by a signal" is unactionable when SIGSEGV, SIGABRT and
/// SIGKILL want three different responses.
pub fn signal_name(sig: i32) -> &'static str {
    match sig {
        1 => "SIGHUP",
        2 => "SIGINT",
        4 => "SIGILL",
        5 => "SIGTRAP",
        6 => "SIGABRT",
        7 => "SIGBUS",
        8 => "SIGFPE",
        9 => "SIGKILL",
        11 => "SIGSEGV",
        13 => "SIGPIPE",
        15 => "SIGTERM",
        31 => "SIGSYS",
        _ => "unknown signal",
    }
}

fn outside_kill_msg(who: &str, sig: i32) -> String {
    format!(
        "{who} was killed by {} (signal {sig}) — a kill from OUTSIDE the process (cgroup \
         OOM-killer / `MemoryMax`, a CI reaper, a manual kill), not a crash in the program; this \
         input proved nothing about the front-end",
        signal_name(sig)
    )
}

/// The signal that killed the child, if any (`ExitStatus::code()` is `None` for ALL of them).
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

fn write_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    let mut f = std::fs::File::create(path)?;
    f.write_all(contents)
}

fn cleanup(p: &std::path::Path) {
    let _ = std::fs::remove_file(p);
}

/// Why `run_one` failed. `TimedOut` is an ordinary, expected outcome (a malformed input can hang
/// the front-end) — `run_input` maps it to `Outcome::Timeout`. `CouldNotRun` means the harness
/// itself is broken (the child never even started, or we lost track of it) — `run_input` maps it
/// to `Outcome::HarnessError`, which callers must treat as fatal, not score as "no finding".
/// Mirrors `difftest::run::RunErr`.
#[derive(Debug)]
enum RunErr {
    TimedOut,
    /// Carries the underlying `io::Error` text plus the program name, so the message names the
    /// actual problem, e.g. `could not run "chezzi": No such file or directory (os error 2)`.
    CouldNotRun(String),
}

/// Run a configured command with a wall-clock timeout. `Err(RunErr::TimedOut)` on timeout (after
/// killing the child); `Err(RunErr::CouldNotRun(_))` if the child could never be observed at all
/// (spawn failed, a stdio pipe wasn't there to take, or `try_wait` errored).
///
/// stdout/stderr are drained on dedicated threads so a child that fills an OS pipe buffer before
/// exiting cannot deadlock the poll loop.
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
                // try_wait failed — reap the child and join the readers so we never leak a zombie
                // process, its pipe fds, or the two reader threads.
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
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        code: status.code(),
        signal: signal_of(&status),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `panicfuzz`'s identical twin to `difftest::run`'s F1 bug (`docs/gaps.md` W7-34/W7-35): a
    /// missing `chezzi` binary must produce `Outcome::HarnessError` naming the real problem, not
    /// `Outcome::Timeout` — a crash-detector sweep against a `chezzi_bin` that cannot spawn must
    /// not report a clean pass over zero executed programs.
    #[test]
    fn a_missing_chezzi_binary_is_a_harness_error_not_a_timeout() {
        let cfg = Config::new("/nonexistent/chezzi-does-not-exist");
        let outcome = run_input(&cfg, b"x := 1\nprint(x)\n");
        match outcome {
            Outcome::HarnessError(msg) => assert!(
                msg.contains("chezzi-does-not-exist") && msg.contains("No such file or directory"),
                "the message must name the actual problem, not just \"failed\": {msg}"
            ),
            other => panic!(
                "expected HarnessError naming the missing binary, got {other:?} \
                 (before the W7-35 fix this wrongly came back as Timeout)"
            ),
        }
    }

    /// A child killed by `sig`, as the OS actually reports it.
    fn cap_signal(sig: i32) -> Capture {
        Capture {
            stdout: String::new(),
            stderr: String::new(),
            code: None,
            signal: Some(sig),
        }
    }

    /// W7-38, twin of `difftest`'s: a SIGKILL is the cgroup OOM-killer under the `MemoryMax`
    /// scope this repo mandates — an outside kill that says nothing about the front-end. Reporting
    /// it as `Crash` is a non-bug reported as a real bug, on the very machine this fuzzer runs on.
    #[test]
    fn a_sigkill_is_not_a_chezzi_bug() {
        let outcome = classify(cap_signal(9));
        assert!(
            !outcome.is_finding(),
            "an OOM/outside SIGKILL must not be a crash finding, got {outcome:?}"
        );
        match outcome {
            Outcome::HarnessError(msg) => assert!(
                msg.contains("SIGKILL") && msg.contains("proved nothing"),
                "the message must name the signal and say the input proved nothing: {msg}"
            ),
            other => panic!("expected the fatal-but-not-a-finding HarnessError, got {other:?}"),
        }
    }

    /// The other half: a SIGSEGV is a real front-end crash and stays a finding.
    #[test]
    fn a_sigsegv_is_still_a_crash_finding() {
        let outcome = classify(cap_signal(11));
        assert!(
            matches!(outcome, Outcome::Crash { .. }) && outcome.is_finding(),
            "a SIGSEGV must stay a Crash finding, got {outcome:?}"
        );
    }

    /// A report that says only "killed by signal" is unactionable.
    #[test]
    fn a_crash_report_names_the_signal() {
        let report = super::super::describe(0, &classify(cap_signal(11)), b"x := 1\n");
        assert!(
            report.contains("SIGSEGV") && report.contains("signal 11"),
            "the report must name the signal:\n{report}"
        );
    }

    /// So nobody later "fixes" the abort in `fuzz_range`/the `panicfuzz` bin by making this a
    /// finding — it is the harness that is broken, not Chezzi.
    #[test]
    fn a_harness_error_is_not_a_finding() {
        assert!(!Outcome::HarnessError("boom".into()).is_finding());
    }

    /// A REAL timeout must stay `Timeout`, not collapse into `HarnessError` — otherwise the two
    /// tests above would still pass with the distinction deleted. Exercised at the `run_one`
    /// level with `sleep 5` under a 50ms timeout (faster and non-flaky vs. spawning the real
    /// `chezzi` binary on a hanging input).
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
