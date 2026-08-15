//! Shell out to both engines, capture their output, and classify the result.
//!
//! The runner writes the two source renderings to temp files, runs `chezzi run <f>` and
//! `python3 <f>` under a wall-clock timeout, then compares stdout BYTES and classifies the pair.
//! `allowlist::check` is consulted as an extension point for known-by-design divergences, but
//! `MATCHERS` is empty today, so nothing is downgraded to `AllowListed` at HEAD — the one entry
//! that ever existed described a divergence that could not happen and was deleted (`W7-31`).

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
    // Never "or timeout": `run_one` returns `Result<Capture, RunErr>` and a timeout is
    // `Err(RunErr::TimedOut)`, so NO `Capture` is ever built from a timed-out child. A chezzi
    // timeout is no longer a straight `Outcome::Timeout` (F3/W7-36 made it run Python, and on a
    // clean Python re-run chezzi at 3x — so it can end as `ChezziHang`, `HarnessError`, or a full
    // `classify` of the retry's capture), but every one of those paths preserves the invariant:
    // the only `Capture`s that reach `classify` come from a child that actually EXITED, and the
    // one synthesized `code: None` capture (`hang_retry_outcome`'s `ChezziHang`) is built into an
    // `Outcome` directly and never routed through `classify`. So a live `Capture` reaching
    // `classify` can still only have `code: None` from a signal kill.
    // Keep this description true: a WRONG comment on this exact field is the recorded proximate
    // cause of the signal-kill case going unclassified for the oracle's whole life (W7-33).
    pub code: Option<i32>,
    /// WHICH signal killed the child (`None` when it exited normally, or for a synthesized
    /// capture). `code` alone cannot tell a SIGSEGV — a real Chezzi bug — from a SIGKILL, which
    /// on this project's own mandated `systemd-run … MemoryMax=6G` scope is the cgroup OOM-killer
    /// and says NOTHING about the program (CLAUDE.md: "exit 137 = OOM, not a test failure").
    /// Without it `classify` reported every OOM kill as a `HostPanic` finding — the series' own
    /// "a non-bug reported as a bug" mirror image (W7-38).
    pub signal: Option<i32>,
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
    /// Both engines failed, but their stdout DIFFERS within the shared prefix — not just one
    /// side getting further before an unrelated fault of its own (`BothError`'s routine shape).
    /// Compared prefix-compatibly (`chz.stdout[..n] == py.stdout[..n]`, `n = min(len, len)`),
    /// never by plain inequality: CPython failing at parse time writes nothing to stdout while
    /// Chezzi fails at runtime after printing, and plain `!=` would flag that routine
    /// generator-quirk shape (`b"" != b"1\n"`) as a divergence (F4, `docs/gaps.md` §W7-36).
    BothErrorStdout,
    /// The Chezzi child did not exit within the timeout, but Python finished the SAME program
    /// cleanly (exit 0). `generate.rs` bounds every loop by construction (`LOOP_CAP`, a bounded
    /// `for`, a mandatory `while` increment), so a generated program that does not terminate is a
    /// Chezzi bug, not the "slow input" excuse that makes a timeout uninteresting in the
    /// panic-fuzz oracle (F3, `docs/gaps.md` §W7-36). `chz` here is a SYNTHESIZED `Capture`
    /// (`run_one` returns nothing on timeout) — empty stdout, a stderr note, `code: None`. That
    /// is deliberately NOT the same `code: None` `classify` treats as a signal kill: this capture
    /// is built directly in `run_sources` and never passed through `classify`, so that invariant
    /// (`Capture::code`'s doc, pinned by W7-33) is untouched.
    ChezziHang,
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
            // F3: don't give up yet. `generate.rs` bounds every loop by construction
            // (`LOOP_CAP`, a bounded `for`, a mandatory `while` increment), so a generated
            // program that does not terminate in the timeout is a Chezzi bug — PROVIDED Python
            // finishes the SAME program (otherwise it's outside the shared subset and "chezzi
            // timed out" tells us nothing; a slow/hanging CPython on our input is a harness
            // matter, not a Chezzi claim).
            let py_after_chz_timeout =
                run_one(Command::new(&cfg.python).arg(&py_path), cfg.timeout);
            let outcome = match py_after_chz_timeout {
                Ok(py) if py.code == Some(0) => {
                    // False-positive guard: a single wall-clock timeout on a loaded machine is
                    // not proof of a hang (this project has been bitten by exactly that pattern
                    // — commit 0fc437a2 — and this gate runs in CI beside a full `cargo test`).
                    // Re-run ONCE at 3x the configured timeout; only report if it times out
                    // again. This only runs on the already-timed-out path, so it costs nothing
                    // on the normal (non-hanging) run.
                    let retry_timeout = cfg.timeout * 3;
                    let retry = run_one(
                        Command::new(&cfg.chezzi_bin).arg("run").arg(&chz_path),
                        retry_timeout,
                    );
                    hang_retry_outcome(retry, py, prog, retry_timeout)
                }
                // Python failed too, or itself timed out: outside the shared subset, not a
                // Chezzi claim.
                Ok(_) | Err(RunErr::TimedOut) => Outcome::Timeout { which: "chezzi" },
                Err(RunErr::CouldNotRun(msg)) => Outcome::HarnessError(msg),
            };
            cleanup(&chz_path, &py_path);
            return outcome;
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

/// F3's 3x-timeout re-run decision, split out of `run_sources` so it's unit-testable without a
/// real subprocess race. Called only after chezzi's FIRST run already timed out and Python
/// finished the same program cleanly (`code: Some(0)`) — `py` is that clean capture.
fn hang_retry_outcome(
    retry: Result<Capture, RunErr>,
    py: Capture,
    prog: Option<&Program>,
    retry_timeout: Duration,
) -> Outcome {
    match retry {
        // Timed out again: a confirmed hang.
        Err(RunErr::TimedOut) => Outcome::Divergence {
            kind: DivKind::ChezziHang,
            // Synthesized: `run_one` returns nothing on timeout, so there is no real `Capture`
            // to report. `code: None` here is NOT the signal-kill sentinel `classify` checks
            // for — this outcome is built directly, never passed through `classify` (see
            // `DivKind::ChezziHang`'s doc).
            chz: Capture {
                stdout: Vec::new(),
                stderr: format!(
                    "chezzi did not exit within {retry_timeout:?} (3x the configured timeout) — hang"
                )
                .into_bytes(),
                code: None,
                // No signal: nothing killed it, we gave up waiting. This capture never reaches
                // `classify` anyway (see above), so it can never be read as an outside kill.
                signal: None,
            },
            py,
        },
        // A loaded-machine false alarm: chezzi finished on the retry after all. Not a confirmed
        // hang, but it DID produce a real capture — classify it against `py` instead of
        // discarding it, or a genuine divergence that merely took 1-3x longer than the timeout
        // goes unreported (the same "real signal thrown away" class this whole task closes).
        Ok(c) => classify(c, py, prog),
        // The harness itself broke on the retry (e.g. the child could not even spawn) — fatal,
        // per this function's contract on every other arm; must NOT collapse into the
        // non-finding `Timeout` (that would reproduce W7-34's exact bug one call site over).
        Err(RunErr::CouldNotRun(msg)) => Outcome::HarnessError(msg),
    }
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
    // `run_one` returns `Err(RunErr::TimedOut)` on timeout and no `Capture` is ever built from
    // it, so a live `Capture` arriving in `classify` cannot mean "timed out" — see the
    // `Capture::code` doc. (F3's hang verdict does synthesize a `code: None` capture, but it
    // builds `Outcome::Divergence{ChezziHang}` directly and never routes it through here — see
    // `hang_retry_outcome`.) Same rule as the twin oracle: `panicfuzz::classify`
    // (`src/panicfuzz/run.rs`) reports this exact condition as `Outcome::Crash`.
    // ...but `code: None` alone does not say WHICH signal, and only some signals implicate the
    // program. A SIGKILL is delivered from OUTSIDE the process — the cgroup OOM-killer under the
    // `MemoryMax` scope this repo mandates, a CI reaper, a manual `kill -9` — so nothing about
    // Chezzi is implicated and the seed proved nothing: that is a `HarnessError` (fatal to every
    // caller, never scored as a clean pass), not a `HostPanic` finding. The allow-list runs the
    // safe way round — only the signals a crash actually raises are promoted to a finding, and an
    // unrecognized signal DECLINES into the fatal-but-not-a-finding arm rather than emitting a
    // confident "Chezzi crashed" (the standing rule, `docs/gaps.md` W7-12).
    if let Some(sig) = chz.signal
        && !is_crash_signal(sig)
    {
        return Outcome::HarnessError(outside_kill_msg("chezzi", sig));
    }
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
        // Both failed, no host panic, no signal kill. Usually the generator produced something
        // outside the shared subset — but "usually" isn't "always": compare the shared PREFIX of
        // stdout (never plain `!=`) so a real divergence printed before two unrelated faults
        // isn't thrown away unexamined (F4). Prefix, not full equality: one side simply getting
        // further before its own fault (Chezzi prints a line Python's earlier parse-time failure
        // never reached) is the routine shape and must stay `BothError` — plain `!=` would flag
        // it.
        let n = chz.stdout.len().min(py.stdout.len());
        if chz.stdout[..n] != py.stdout[..n] {
            return Outcome::Divergence {
                kind: DivKind::BothErrorStdout,
                chz,
                py,
            };
        }
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

/// Signals a **bug in the child itself** raises. Everything else — SIGKILL (cgroup OOM-killer,
/// `kill -9`), SIGTERM/SIGINT/SIGHUP (a CI reaper, a human) — is an outside kill that implicates
/// nothing about Chezzi. Numbers are Linux's (`man 7 signal`); this crate is already Unix-only
/// (`src/native/fs.rs` imports `std::os::unix::*` unconditionally) and is developed on Linux.
fn is_crash_signal(sig: i32) -> bool {
    matches!(sig, 4 | 5 | 6 | 7 | 8 | 11 | 31)
}

/// Name the signal in reports. "killed by a SIGNAL" is unactionable — SIGSEGV, SIGABRT and
/// SIGKILL want three different responses — and an unactionable report is the exact defect W7-30
/// already had to fix once in `describe`.
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
         seed proved nothing about either engine",
        signal_name(sig)
    )
}

/// The signal that killed the child, if any (`ExitStatus::code()` is `None` for ALL of them).
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
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
        signal: signal_of(&status),
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
            signal: None,
        }
    }

    /// A child killed by `sig` — `code: None` plus the signal number, which is what the OS
    /// actually reports and what tells a SIGSEGV (our bug) from a SIGKILL (someone else's).
    fn cap_signal(stderr: Vec<u8>, sig: i32) -> Capture {
        Capture {
            stdout: Vec::new(),
            stderr,
            code: None,
            signal: Some(sig),
        }
    }

    /// Build a capture with an explicit exit code and stderr — for tests that exercise the
    /// fault arms `cap` (hardcoded `code: Some(0)`) cannot reach.
    fn cap_exit(stdout: Vec<u8>, stderr: Vec<u8>, code: Option<i32>) -> Capture {
        Capture {
            stdout,
            stderr,
            code,
            signal: None,
        }
    }

    /// The CPython differential oracle must diff **bytes**. `String::from_utf8_lossy` is not
    /// injective — `ff fe` and `fe ff` both decode to two U+FFFD — so a decoded compare reports
    /// `Match` for a run where Chezzi and CPython put DIFFERENT bytes on fd 1. Both sides can
    /// emit non-UTF-8 today (`io.stdout().write_bytes` since W6-9; CPython's
    /// `sys.stdout.buffer.write` always could), and this oracle is the one `docs/future.md §2b`
    /// keeps now that `--serial` is deleted. Same class as the parity-oracle hole W6-9b closed.
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
        let chz = cap_signal(b"\nthread 'main' has overflowed its stack\n".to_vec(), 11);
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
        let chz = cap_signal(b"\nthread 'main' has overflowed its stack\n".to_vec(), 11);
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

    /// W7-38. A SIGKILL is delivered from OUTSIDE the process — on this machine, the cgroup
    /// OOM-killer under the `MemoryMax=6G` scope CLAUDE.md mandates for every cargo run ("exit
    /// 137 = OOM, not a test failure"). Nothing about the program is implicated, so reporting it
    /// as `HostPanic` — a finding, exit 1, "chezzi crashed" — is a NON-bug reported as a real
    /// bug, and it fires on the very machine this oracle runs on.
    #[test]
    fn a_sigkill_is_not_a_chezzi_bug() {
        let chz = cap_signal(Vec::new(), 9);
        let py = cap_exit(b"ok\n".to_vec(), Vec::new(), Some(0));
        let outcome = classify(chz, py, None);
        assert!(
            !outcome.is_finding(),
            "an OOM/outside SIGKILL must not be reported as a Chezzi bug, got {outcome:?}"
        );
        match outcome {
            Outcome::HarnessError(msg) => assert!(
                msg.contains("SIGKILL") && msg.contains("proved nothing"),
                "the message must name the signal and say the seed proved nothing: {msg}"
            ),
            other => panic!("expected the fatal-but-not-a-finding HarnessError, got {other:?}"),
        }
    }

    /// The other half of the same rule, so nobody "fixes" the SIGKILL case by demoting every
    /// signal: a SIGSEGV is a real crash in OUR runtime and stays the highest-value finding this
    /// oracle can produce.
    #[test]
    fn a_sigsegv_is_still_a_host_panic() {
        let chz = cap_signal(Vec::new(), 11);
        let py = cap_exit(b"ok\n".to_vec(), Vec::new(), Some(0));
        let outcome = classify(chz, py, None);
        assert!(
            matches!(outcome, Outcome::HostPanic { .. }) && outcome.is_finding(),
            "a SIGSEGV must stay a HostPanic finding, got {outcome:?}"
        );
    }

    /// A report that says only "killed by a SIGNAL" is unactionable — the same defect W7-30 had
    /// to fix once already in `describe`.
    #[test]
    fn a_signal_kill_report_names_the_signal() {
        let outcome = classify(
            cap_signal(Vec::new(), 11),
            cap_exit(b"ok\n".to_vec(), Vec::new(), Some(0)),
            None,
        );
        let report = super::super::describe(0, &outcome, "<chz>", "<py>");
        assert!(
            report.contains("SIGSEGV") && report.contains("signal 11"),
            "the report must name the signal, not just say 'a SIGNAL':\n{report}"
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

    // --- F4: the both-failed arm must diff stdout, not throw it away -------------------------

    /// F4 test #1. Both engines failed but printed genuinely DIFFERENT bytes on stdout before
    /// failing — a real divergence, not the routine "one side got further" shape `BothError`
    /// exists for.
    #[test]
    fn a_both_failed_run_with_divergent_stdout_is_a_finding() {
        let chz = cap_exit(b"1\n".to_vec(), b"runtime error: boom".to_vec(), Some(1));
        let py = cap_exit(b"2\n".to_vec(), b"ZeroDivisionError".to_vec(), Some(1));
        assert!(
            matches!(
                classify(chz, py, None),
                Outcome::Divergence {
                    kind: DivKind::BothErrorStdout,
                    ..
                }
            ),
            "two different bytes on stdout before both sides failed must be a finding"
        );
    }

    /// F4 test #2. The guard against a naive `!=` check: one side simply printed MORE before it
    /// hit its own unrelated fault (a common shape — Chezzi runs one line further than Python
    /// before faulting). Plain inequality WOULD flag this (`b"1\n2\n" != b"1\n"`); the assert
    /// below pins that premise so nobody swaps prefix-compare for `!=` without this test failing.
    /// Prefix-compatibility must not.
    #[test]
    fn a_both_failed_run_whose_stdout_is_a_prefix_stays_a_non_finding() {
        let chz = cap_exit(b"1\n2\n".to_vec(), b"runtime error: boom".to_vec(), Some(1));
        let py = cap_exit(b"1\n".to_vec(), b"ZeroDivisionError".to_vec(), Some(1));
        assert_ne!(
            chz.stdout, py.stdout,
            "premise: a naive != check would flag this pair"
        );
        assert!(
            matches!(classify(chz, py, None), Outcome::BothError),
            "one side simply getting further before its own fault must stay a non-finding"
        );
    }

    /// F4 test #3. The CPython-parse-error shape: Python fails before writing anything to
    /// stdout, Chezzi fails at runtime after printing — a routine generator quirk, not a
    /// divergence (an empty prefix is trivially compatible with anything).
    #[test]
    fn a_both_failed_run_with_empty_python_stdout_stays_a_non_finding() {
        let chz = cap_exit(b"1\n".to_vec(), b"runtime error: boom".to_vec(), Some(1));
        let py = cap_exit(Vec::new(), b"SyntaxError: invalid syntax".to_vec(), Some(1));
        assert!(
            matches!(classify(chz, py, None), Outcome::BothError),
            "CPython failing before writing anything must stay a non-finding"
        );
    }

    // --- F3: a Chezzi hang is a finding when Python survives the same program ----------------

    /// Locate the built `chezzi` binary for a real-subprocess test. `env!("CARGO_BIN_EXE_chezzi")`
    /// does NOT work here: this module is textually pulled into TWO different crates by
    /// `#[path]` (`tests/difftest.rs`, and `src/bin/difffuzz.rs` built as a test harness by a
    /// plain `cargo test`), and the second one fails to COMPILE with that macro — confirmed:
    /// `cargo test --bin difffuzz` errors `environment variable "CARGO_BIN_EXE_chezzi" not
    /// defined at compile time` (Cargo only defines `CARGO_BIN_EXE_<name>` for integration-test
    /// compilation, not for a bin target built in test mode). Mirrors `difffuzz::locate_chezzi`'s
    /// current_exe-relative search, one directory further up: a TEST binary (this one, or
    /// `tests/difftest.rs`'s) lives in `target/{debug,release}/deps/`, not directly in
    /// `target/{debug,release}/` like a plain `cargo build` binary.
    /// NO PATH fallback (W7-38). A bare `PathBuf::from("chezzi")` resolves to whatever `chezzi`
    /// is installed in `~/.cargo/bin` — on this machine a binary predating this work by days —
    /// so a green hang test could be proving something about a STALE binary, and nothing in these
    /// tests pins which one ran. This repo has a documented history of exactly that trap
    /// (CLAUDE.md's worktree/`CARGO_TARGET_DIR` warning: "the binary you verify SILENTLY LACKS
    /// your change — a green two-engine run proving nothing"). Refuse instead.
    fn locate_chezzi_for_test() -> PathBuf {
        let exe = std::env::current_exe().expect("current_exe");
        let cand = exe
            .parent()
            .and_then(|d| d.parent())
            .map(|d| d.join("chezzi"));
        match cand {
            Some(p) if p.exists() => p,
            other => panic!(
                "no sibling `chezzi` binary at {other:?} — build it first \
                 (`cargo build --bin chezzi`, or run the full `cargo test`). Refusing to fall \
                 back to PATH: that can silently test a stale installed binary."
            ),
        }
    }

    /// Short-timeout config for the hang tests: real subprocesses, so keep it small — the 3x
    /// re-run false-positive guard means a confirmed hang costs ~4x this value.
    fn hang_cfg() -> Config {
        let mut c = Config::new(locate_chezzi_for_test());
        c.timeout = Duration::from_millis(500);
        c
    }

    /// F3. `generate.rs` bounds every loop by construction, so a generated program that never
    /// terminates within the timeout is a Chezzi bug — PROVIDED Python finishes the SAME program
    /// (this is the brief's own repro verbatim). Real subprocesses; prints the measured wall time
    /// per the report requirement.
    #[test]
    fn a_chezzi_hang_python_survives_is_a_finding() {
        let cfg = hang_cfg();
        let start = Instant::now();
        let outcome = run_sources(&cfg, "while true:\n    x := 0\n", "print(0)\n", None);
        let elapsed = start.elapsed();
        assert!(
            matches!(
                outcome,
                Outcome::Divergence {
                    kind: DivKind::ChezziHang,
                    ..
                }
            ),
            "a chezzi hang Python survives must be a finding, got {outcome:?}"
        );
        assert!(outcome.is_finding());
        // The 3x re-run guard must actually RUN before a hang is reported — deleting it leaves
        // every other assertion here green (measured: 2.02 s → 519 ms), which is precisely the
        // regression the guard exists to prevent. A wall-clock LOWER bound is the safe
        // direction to assert: load can only make this slower, never faster, so unlike the
        // upper bound that had to be deleted in `0fc437a2` this cannot flake under a busy
        // suite. Budget: 1x for the first timeout + 3x for the re-run = 4x; require 3x so a
        // little scheduling slop is fine but skipping the re-run entirely (~1x) is not.
        assert!(
            elapsed >= cfg.timeout * 3,
            "the 3x re-run guard did not run: {elapsed:?} < {:?}",
            cfg.timeout * 3
        );
    }

    /// F3 guard. Python hanging too means the program is outside the shared subset — a slow or
    /// hanging CPython on our generated input is a harness/generator matter, not a Chezzi claim.
    /// Must stay a non-finding (and must not pay the 3x chezzi re-run: python never survives).
    #[test]
    fn a_chezzi_hang_python_also_hangs_stays_a_non_finding() {
        let cfg = hang_cfg();
        let outcome = run_sources(
            &cfg,
            "while true:\n    x := 0\n",
            "while True:\n    x = 0\n",
            None,
        );
        assert!(
            !outcome.is_finding(),
            "both sides hanging must not be a finding, got {outcome:?}"
        );
    }

    /// F3 guard, the OTHER half of "Python survived the same program". The two tests above cover
    /// Python exiting 0 and Python hanging too; neither one dies if `run_sources`'s
    /// `py.code == Some(0)` guard is deleted (with the guard gone that arm becomes `Ok(py) =>`,
    /// and both still reach the same verdict). This one does: Python FAILS FAST (non-zero exit, no
    /// hang) means the program is simply outside the shared subset, so "chezzi timed out" tells us
    /// nothing about Chezzi and must stay a non-finding. Without the guard it becomes a confident
    /// `Divergence{ChezziHang}`.
    #[test]
    fn a_chezzi_hang_python_fails_fast_stays_a_non_finding() {
        let cfg = hang_cfg();
        let outcome = run_sources(
            &cfg,
            "while true:\n    x := 0\n",
            "import sys\nsys.exit(3)\n",
            None,
        );
        assert!(
            !outcome.is_finding(),
            "python failing fast means the program is outside the shared subset, so a chezzi \
             timeout proves nothing — must not be a finding, got {outcome:?}"
        );
    }

    // --- F3 review follow-up: the 3x-timeout retry's own two outcomes must not be swallowed ---

    /// Adversarial-review finding: a naive retry match (`Err(TimedOut) => Divergence, _ =>
    /// Timeout`) silently absorbs `RunErr::CouldNotRun` — the retry's OWN harness failure (e.g.
    /// the child couldn't even spawn) — into the ordinary non-finding `Timeout`, reproducing
    /// `W7-34`'s exact bug one call site over (a harness error must be FATAL, never scored as
    /// "no finding" — every other `RunErr::CouldNotRun` arm in this file maps to
    /// `Outcome::HarnessError`).
    #[test]
    fn a_hang_retry_harness_error_is_not_silently_a_timeout() {
        let py = cap_exit(b"0\n".to_vec(), Vec::new(), Some(0));
        let outcome = hang_retry_outcome(
            Err(RunErr::CouldNotRun("could not run \"chezzi\": ...".into())),
            py,
            None,
            Duration::from_millis(900),
        );
        assert!(
            matches!(outcome, Outcome::HarnessError(_)),
            "the retry's own harness failure must be fatal, not a Timeout non-finding, got {outcome:?}"
        );
    }

    /// Adversarial-review finding: when the 3x-timeout retry actually SUCCEEDS (a loaded-machine
    /// false alarm, not a genuine hang), the resulting `Capture` must be classified against
    /// Python's, not discarded — otherwise a real divergence that merely took 1-3x longer than
    /// the timeout goes unreported, silently downgraded to `Timeout { which: "chezzi" }`.
    #[test]
    fn a_hang_retry_that_succeeds_is_classified_not_discarded() {
        let py = cap_exit(b"1\n".to_vec(), Vec::new(), Some(0));
        let chz_retry = cap_exit(b"2\n".to_vec(), Vec::new(), Some(0));
        let outcome = hang_retry_outcome(Ok(chz_retry), py, None, Duration::from_millis(900));
        assert!(
            matches!(
                outcome,
                Outcome::Divergence {
                    kind: DivKind::Stdout,
                    ..
                }
            ),
            "a slow-but-successful retry with divergent stdout must be classified as a real \
             finding, not discarded as a non-finding Timeout, got {outcome:?}"
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
