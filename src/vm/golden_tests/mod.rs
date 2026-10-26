// Extracted from vm/mod.rs (test module). `super::` == the `vm` module.
//! Single-engine (M:N) regression tests, most as a literal golden.
//!
//! This file was `parity_tests.rs` until 2026-08-16: it drove a cross-engine `assert_eq!` between
//! the cooperative `--serial` VM and the M:N VM, and its helpers (`assert_parity_out`,
//! `parity_entry` and friends) ran a program on BOTH engines and compared them. `--serial` and the
//! oracle it enabled are gone, so every helper here now runs the M:N engine ONCE and compares
//! against a real expectation instead — the file and helpers were renamed to `golden_tests.rs` /
//! `golden_entry`+`assert_golden_out` to match. Kept for institutional memory: a large fraction of
//! the tests below still carry a `_parity` suffix in their own names from that era; that's cosmetic
//! now; renaming ~300 individual test names was judged out of scope for this cleanup.
use super::*;
use crate::test_support::TmpDir;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Assert the M:N engine's stdout equals `expect`. A hang here means a `continue` is landing on
/// the wrong target (re-test without advancing → infinite loop).
fn assert_golden_out(src: &str, expect: &str) {
    assert_eq!(
        vm_outcome(src).expect("program should run"),
        expect,
        "for:\n{src}"
    );
}

fn fixture(rel: &str) -> PathBuf {
    crate::native::std_embed::checkout_root().join(rel)
}

/// Convenience: a single entry file (the common std-module case).
pub(super) fn golden_entry(src: &str) -> String {
    golden_file_entry(&[("main.chz", src)], "main.chz")
}

/// Run an entry through the M:N engine with a freshly-built [`crate::native::HostConfig`]. Returns
/// stdout (raw bytes, W6-9b, decoded here — so a non-UTF-8-emitting program can still be asserted
/// precisely by a caller that goes through [`crate::vm::run_file_engine`] directly).
fn golden_entry_cfg(src: &str, mk_cfg: impl Fn() -> crate::native::HostConfig) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, _err, _result, _code) =
        crate::vm::run_file_engine(&entry, mk_cfg(), None, None, false);
    captured(out)
}

/// Like [`golden_entry_cfg`], but documents that the returned stdout is a deterministic MULTISET
/// with a nondeterministic ORDER — a shared, consumable stdin read from several tasks: which task
/// gets which line is nondeterministic BY DESIGN (Go/Python), so a byte-equal stdout assert here
/// would be a flake built on purpose. Callers assert with [`assert_lines_multiset`].
fn golden_entry_cfg_lines(src: &str, mk_cfg: impl Fn() -> crate::native::HostConfig) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (out, _err, result, _code) = run_file_with(&entry, mk_cfg());
    result.expect("program should run");
    out
}

/// Like [`golden_entry`], but for a program that must FAULT: runs the graph path on the M:N engine
/// and returns the fault message so the caller can assert its content.
#[cfg(test)]
pub(super) fn golden_entry_fault(src: &str) -> String {
    let t = TmpDir::new();
    let p = t.write("main.chz", src);
    let (_out, _err, result, _code) = run_file(&p);
    let e = result.expect_err("program must fault");
    e.to_string()
}

/// Run a multi-file program (one or more `.chz` files) through the M:N engine via `run_file_engine`,
/// and return its stdout. `files` is `(relative_path, contents)`; `entry` names the file to run.
/// Needed because the single-file `run_capture` can't exercise imports (and std modules require the
/// import path). RAW BYTES (W6-9b) so a non-UTF-8-emitting program can still be asserted precisely.
fn golden_file_entry(files: &[(&str, &str)], entry: &str) -> String {
    let t = TmpDir::new();
    let mut entry_path = None;
    for (rel, contents) in files {
        let p = t.write(rel, contents);
        if *rel == entry {
            entry_path = Some(p);
        }
    }
    let entry_path = entry_path.expect("entry must be one of the files");
    let (out, _err, _result, _code) = crate::vm::run_file_engine(
        &entry_path,
        crate::native::HostConfig::default(),
        None,
        None,
        false,
    );
    captured(out)
}

/// Wrap a `main()` body in `import std.cancel` + run it through the module graph, return stdout.
fn run_cancel_snippet(tag: &str, body: &str) -> String {
    let src = format!("import std.cancel\n\nfn main():\n{body}\nmain()\n");
    run_cancel_src(tag, &src)
}

/// Run a full `std.cancel`-importing source through the module graph, return stdout.
fn run_cancel_src(tag: &str, src: &str) -> String {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let entry = std::env::temp_dir().join(format!("chezzi_{tag}_{}_{seq}.chz", std::process::id()));
    std::fs::write(&entry, src).expect("write temp .chz");
    let (out, _err, res, _) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "snippet faulted: {res:?}");
    out
}

/// D6c — run a `--parallel` net program from a temp file under a 30 s watchdog (net round-trips
/// can legitimately take longer than `run_parallel_watchdog`'s 5 s, and a regressed timeout would
/// HANG rather than fault). Returns the captured stdout, or panics loudly on a hang.
fn run_net_timeout_watchdog(tag: &str, src: &str) -> String {
    run_net_watchdog_engine(tag, src)
}

/// B3.5 — run an entry on the `--parallel` engine under a watchdog: a missing/broken deadlock
/// detector would hang the nursery forever, so we run it on a side thread and fail loudly if it
/// doesn't finish, instead of wedging the whole test binary. (On a clean detector none of these
/// ever time out — the leak only happens on the failure path we're guarding against.)
fn run_parallel_watchdog(src: &str) -> RunOutput {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(&entry, crate::native::HostConfig::default()));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(5)) {
        Ok(r) => r,
        Err(_) => {
            panic!("hung: --parallel nursery did not terminate (deadlock detection missing/broken)")
        }
    }
}

/// The decoded shape — `assert_eq!(vm_outcome(src).unwrap(), "6\n")`, `.contains(..)`, the
/// fn-pointer arrays. Pins the M:N engine's output against a literal, so the lossy decode can't
/// hide anything.
fn vm_outcome(src: &str) -> Result<String, String> {
    vm_outcome_bytes(src).map(captured)
}

/// Run a single-file (importing std) program on the VM with GC stress on (collect before every
/// instruction) and the given config — surfaces any native-return value the collector might free
/// while still reachable.
pub(super) fn vm_run_file_stress(src: &str, cfg: crate::native::HostConfig) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let graph = crate::resolver::build_graph(&entry).unwrap();
    let program = crate::compiler::compile_graph(&graph).unwrap();
    let mut vm = Vm::new(Arc::new(program));
    vm.gc_stress = true;
    vm.host = cfg;
    vm.run()
        .unwrap_or_else(|e| panic!("unexpected error under GC stress: {e}"));
    captured(vm.out)
}

/// Assert the M:N engine produces `want`.
fn widen_three_engines(src: &str, want: &str) {
    assert_eq!(run_capture(src).expect("program should run"), want);
}

/// The same watchdog, on the M:N engine — a regression there is a HANG, so the watchdog is mandatory
/// rather than a nicety.
fn run_net_watchdog_engine(tag: &str, src: &str) -> String {
    let t = TmpDir::new();
    let entry = t.write("main.chz", src);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_with(&entry, crate::native::HostConfig::default()));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "{tag}: program faulted: {res:?}");
            out
        }
        Err(_) => panic!("{tag}: hung — D6c socket timeout regressed (the op parked forever)"),
    }
}

/// Outcome of a run in the sink's RAW BYTES, for a caller that needs the exact bytes (non-UTF-8
/// output) rather than the lossily-decoded `String` (W6-9b).
fn vm_outcome_bytes(src: &str) -> Result<Vec<u8>, String> {
    run_capture_bytes(src).map_err(|e| e.to_string())
}

mod airlock_parallel;
mod airlock_values;
mod captures_data;
mod examples;
mod ffi;
mod io_cancel;
mod lang_parity;
mod modules;
mod net;
mod std_parity;
