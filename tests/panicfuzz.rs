//! Panic-fuzz harness — CI gate (bug-discovery lever #1).
//!
//! Shares the engine with `src/bin/panicfuzz.rs` via a `#[path]` include (the fuzz engine is not
//! exposed by the `chezzi` library, so it is pulled in by path rather than `use`).
//! `env!("CARGO_BIN_EXE_chezzi")` is provided by cargo for integration tests — it is the
//! **debug** `chezzi`, so overflow-checks are ON and arithmetic-overflow panics ARE caught here.
//!
//! The invariant under test: feeding any malformed / adversarial input to `chezzi check` (the full
//! front-end: lexer → parser → checker) yields a clean diagnostic — never a Rust panic, never a
//! signal crash (SIGSEGV / SIGABRT / stack-overflow). This harness only exercises the front-end's
//! crash-safety; it never runs the VM/interp, so the two-engine parity bar does not apply.

#[path = "../src/panicfuzz/mod.rs"]
mod panicfuzz;

use panicfuzz::run;
use panicfuzz::run::Capture;
use panicfuzz::{Config, Outcome};
use std::time::Duration;

fn config() -> Config {
    let mut c = Config::new(env!("CARGO_BIN_EXE_chezzi"));
    c.timeout = Duration::from_secs(20);
    c
}

/// Load the `examples/*.chz` corpus as raw bytes (for the structure-aware mutation generator).
fn load_corpus() -> Vec<Vec<u8>> {
    // Sort by filename so the seed->base-example mapping (and thus the gate's input set) is
    // stable across machines — `read_dir` order is filesystem-defined.
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) == Some("chz") {
                paths.push(p);
            }
        }
    }
    paths.sort();
    paths
        .into_iter()
        .filter_map(|p| std::fs::read(&p).ok())
        .collect()
}

// ---------------------------------------------------------------------------
// Non-tautology guard — prove `classify` actually distinguishes the four cases.
// ---------------------------------------------------------------------------

#[test]
fn classify_flags_panic_signal_and_clean() {
    // A Rust host panic on stderr => HostPanic (a finding).
    let panic = run::classify(Some(Capture {
        stdout: String::new(),
        stderr: "thread 'main' panicked at src/parser/mod.rs:1234:5".into(),
        code: Some(101),
    }));
    assert!(matches!(panic, Outcome::HostPanic { .. }), "{panic:?}");
    assert!(panic.is_finding());

    // Killed by a signal (code == None, no panic marker) => Crash (a finding).
    let crash = run::classify(Some(Capture {
        stdout: String::new(),
        stderr: String::new(),
        code: None,
    }));
    assert!(matches!(crash, Outcome::Crash { .. }), "{crash:?}");
    assert!(crash.is_finding());

    // A clean non-zero diagnostic => Clean (NOT a finding).
    let clean = run::classify(Some(Capture {
        stdout: String::new(),
        stderr: "error: parse error: expected expression".into(),
        code: Some(1),
    }));
    assert!(matches!(clean, Outcome::Clean { .. }), "{clean:?}");
    assert!(!clean.is_finding());

    // Exit 0 => Clean.
    let ok = run::classify(Some(Capture {
        stdout: "ok: no type errors".into(),
        stderr: String::new(),
        code: Some(0),
    }));
    assert!(matches!(ok, Outcome::Clean { .. }), "{ok:?}");

    // Wall-clock timeout (None capture) => Timeout (NOT a finding).
    let timeout = run::classify(None);
    assert!(matches!(timeout, Outcome::Timeout), "{timeout:?}");
    assert!(!timeout.is_finding());

    // A signal kill that ALSO printed a panic marker is classified as the (more specific)
    // HostPanic, not Crash.
    let panic_and_signal = run::classify(Some(Capture {
        stdout: String::new(),
        stderr: "thread 'main' panicked at src/x.rs:1:1\nstack overflow".into(),
        code: None,
    }));
    assert!(
        matches!(panic_and_signal, Outcome::HostPanic { .. }),
        "{panic_and_signal:?}"
    );
}

// ---------------------------------------------------------------------------
// End-to-end: a valid program and obvious garbage must both be non-findings
// (clean diagnostic), driving the real `chezzi check` subprocess.
// ---------------------------------------------------------------------------

#[test]
fn valid_and_malformed_inputs_are_clean() {
    let cfg = config();

    // A well-formed program: type-checks cleanly, exit 0.
    let ok = run::run_input(&cfg, b"x := 1\nprint(x)\n");
    assert!(!ok.is_finding(), "valid program flagged: {ok:?}");
    assert!(matches!(ok, Outcome::Clean { .. }), "{ok:?}");

    // Obvious garbage incl. an invalid UTF-8 byte: chezzi must emit a clean diagnostic / read
    // error, never a Rust panic or a signal crash.
    let garbage = run::run_input(&cfg, b"{{{:::\n\t}}\xff fn for if := |> ?? ->\n");
    assert!(
        !garbage.is_finding(),
        "garbage triggered a crash: {garbage:?}"
    );
}

// ---------------------------------------------------------------------------
// Generators: deterministic, bounded, and all three strategies get exercised.
// ---------------------------------------------------------------------------

#[test]
fn generators_are_bounded_and_deterministic() {
    use panicfuzz::generate;
    let corpus: Vec<Vec<u8>> = vec![
        b"fn add(a: int, b: int) -> int:\n    return a + b\n".to_vec(),
        b"x := [1, 2, 3]\nfor v in x:\n    print(v)\n".to_vec(),
    ];

    let mut strategies = [0usize; 3];
    for seed in 0..300u64 {
        let a = generate::gen_input(seed, &corpus);
        let b = generate::gen_input(seed, &corpus);
        assert_eq!(a, b, "gen_input not deterministic at seed {seed}");
        assert!(
            a.len() <= 2048,
            "gen_input exceeded bound at seed {seed}: {} bytes",
            a.len()
        );
        strategies[generate::strategy_of(seed, &corpus)] += 1;
    }
    // Over 300 seeds every strategy branch must be reached (error-state coverage).
    for (i, &n) in strategies.iter().enumerate() {
        assert!(
            n > 0,
            "strategy {i} never exercised over 0..300: {strategies:?}"
        );
    }

    // Empty corpus: strategy-3 (mutation) must degrade gracefully (fall back), still bounded.
    let empty: Vec<Vec<u8>> = vec![];
    for seed in 0..50u64 {
        let v = generate::gen_input(seed, &empty);
        assert!(v.len() <= 2048);
    }
}

// ---------------------------------------------------------------------------
// The gate: 2000 reproducible seeds. Any panic / signal crash fails the test,
// listing the seed + triggering input so it can be reproduced via --seed N.
// (Debug chezzi => overflow-checks ON => arithmetic-overflow panics ARE caught.)
// ---------------------------------------------------------------------------

#[test]
fn fuzz_no_panics_seeds_0_2000() {
    let cfg = config();
    let corpus = load_corpus();
    assert!(!corpus.is_empty(), "examples corpus failed to load");

    let mut findings = Vec::new();
    for seed in 0..2000u64 {
        let (outcome, input) = panicfuzz::run_seed(&cfg, seed, &corpus);
        if outcome.is_finding() {
            findings.push(panicfuzz::describe(seed, &outcome, &input));
        }
    }
    assert!(
        findings.is_empty(),
        "front-end panic/crash finding(s) — these are REAL bugs:\n{}",
        findings.join("\n")
    );
}
