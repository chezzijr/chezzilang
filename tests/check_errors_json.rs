//! CLI-level diagnostic-shape tests for `chezzi check` resolve errors (the two diagnostic-quality
//! bugs). Drives the real `env!("CARGO_BIN_EXE_chezzi")` binary via `std::process::Command`, so it
//! verifies the end-to-end CLI contract (`--errors=json` JSON shape + plain-text rendering), not a
//! unit helper. These are diagnostic-only: the accept/reject decision is unchanged.

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A unique temp directory, removed on drop.
struct TmpDir(PathBuf);
impl TmpDir {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("chezzi_cej_{}_{}", std::process::id(), n));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    fn write(&self, rel: &str, contents: &str) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, contents).unwrap();
        p
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Build the repro project: main.chz imports `deep`, and deep.chz has a bad `import ghost from
/// doesnotexist` on line 4. Returns the temp dir (keep alive) and the entry path.
fn missing_module_project() -> (TmpDir, PathBuf) {
    let t = TmpDir::new();
    let main = t.write("main.chz", "import deep\nfn main(): print(1)\n");
    t.write(
        "deep.chz",
        "# pad\n# pad\n# pad\nimport ghost from doesnotexist\nfn f(): print(1)\n",
    );
    (t, main)
}

#[test]
fn resolve_error_json_is_clean_and_attributed() {
    let (_t, main) = missing_module_project();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", main.to_str().unwrap(), "--errors=json"])
        .output()
        .expect("run chezzi check --errors=json");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stdout = stdout.trim();

    // (a) Exactly one JSON object in the array. Cheap structural check without a JSON dep.
    assert!(
        stdout.starts_with('[') && stdout.ends_with(']'),
        "expected a JSON array, got: {stdout}"
    );
    assert_eq!(
        stdout.matches("{").count(),
        1,
        "expected exactly one error object, got: {stdout}"
    );

    // (b) The message names the importing module + the missing module.
    assert!(
        stdout.contains("in module 'deep': cannot find module 'doesnotexist'"),
        "message must name importer + missing module, got: {stdout}"
    );
    // (c) No doubled Display prefix embedded inside the JSON message.
    assert!(
        !stdout.contains("resolve error ("),
        "JSON message must not embed the `resolve error (...)` Display prefix, got: {stdout}"
    );
    // (d) Carries the line of the bad import (line 4 in deep.chz).
    assert!(
        stdout.contains("\"line\":4"),
        "must carry line 4, got: {stdout}"
    );
}

#[test]
fn resolve_error_plaintext_unchanged_and_attributed() {
    let (_t, main) = missing_module_project();
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", main.to_str().unwrap()])
        .output()
        .expect("run chezzi check");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim_end();

    // Plain-text keeps the `resolve error (line N, col M):` Display prefix (byte-identical rendering),
    // now followed by the module attribution.
    assert!(
        stderr.starts_with("resolve error (line 4, col 1):"),
        "plain text must keep the Display prefix, got: {stderr}"
    );
    assert!(
        stderr.contains("in module 'deep': cannot find module 'doesnotexist'"),
        "plain text must name importer + missing module, got: {stderr}"
    );
}

/// Crash-safety regression: a valid but very long left-associative binary chain or postfix chain
/// used to build an AST deep enough to overflow the recursive front-end walkers → host stack
/// overflow (SIGABRT, exit code None). The `MAX_AST_DEPTH` parser cap + the dedicated front-end
/// stack turn that into either a clean parse diagnostic (over the cap) or a normal run (under it) —
/// NEVER a signal kill. Drives the real binary end-to-end (a parser unit test cannot observe the
/// process abort). See docs/bug-discovery.md (post-parse walker depth axis).
#[test]
fn deep_chains_never_signal_crash_the_host() {
    let t = TmpDir::new();
    let over_cap = chezzi::parser::MAX_AST_DEPTH + 100;

    // (a) An over-cap `1+1+…` chain (the original repro was 6000 terms, which the raised
    // `MAX_AST_DEPTH` now legitimately accepts): `check` must exit with a code (a clean diagnostic),
    // never be killed by a signal (SIGABRT → code() == None).
    let big_add = format!("x := 1{}\n", "+1".repeat(over_cap));
    let f = t.write("big_add.chz", &big_add);
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", f.to_str().unwrap()])
        .output()
        .expect("run chezzi check");
    assert!(
        out.status.code().is_some(),
        "deep + chain must not signal-crash the host (got signal kill, no exit code)"
    );
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("too deeply"),
        "over-cap chain should be a clean 'too deeply' diagnostic"
    );

    // (b) Same for a deep postfix field chain and via `run` (compiler + VM path).
    let big_field = format!("x := a{}\n", ".f".repeat(over_cap));
    let f2 = t.write("big_field.chz", &big_field);
    let out2 = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", f2.to_str().unwrap()])
        .output()
        .expect("run chezzi run");
    assert!(
        out2.status.code().is_some(),
        "deep postfix chain via `run` must not signal-crash the host"
    );

    // (c) A chain UNDER the cap runs and prints the right value (no over-rejection).
    let ok = format!("print(1{})\n", "+1".repeat(400));
    let f3 = t.write("ok_add.chz", &ok);
    let out3 = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", f3.to_str().unwrap()])
        .output()
        .expect("run chezzi run");
    assert!(out3.status.success(), "under-cap chain must run cleanly");
    assert_eq!(String::from_utf8_lossy(&out3.stdout).trim(), "401");
}

/// **THE MARGIN ORACLE.** `deep_chains_never_signal_crash_the_host` (above) proves the guard fires;
/// this proves the guard fires *early enough*. It builds the WORST program the parser now accepts —
/// `parser::MAX_DEPTH` recursion and `parser::MAX_AST_DEPTH` fold depth spent together, in the shape
/// that composes them (`f(g(inner) + 1 …)`, where the folds land on top of the descent) — and drives
/// `ast`, `check` and `run` over it. It is the only thing that exercises `MAX_DEPTH`, `MAX_AST_DEPTH`,
/// `FRONTEND_STACK_BYTES`, `VM_STACK_BYTES` and W7-50's `cmd_ast` stack hop together, end to end, in
/// the DEBUG profile whose frames bind. A regression here is a host abort (`code() == None`), which
/// is why the assertion is on `status.code()` and not on the output.
///
/// Measured margin at the shipped constants: this program is ~16 000 AST nodes deep; debug
/// `chezzi run` (the 384 MiB `VM_STACK_BYTES` worker — the smaller of the two big stacks) survives
/// 33 000 and aborts at 33 500, so the margin is ~2.06×.
///
/// **Cost note — what was traded.** `chezzi ast`'s `{:#?}` render is worse than quadratic in depth
/// (measured, debug: 200 nodes 2.2 s, 400 nodes 17 s, 800 nodes > 90 s), so the `ast` arm runs on a
/// ~200-node chain with stdout to `Stdio::null()`, while `check` and `run` — the arms that actually
/// carry the deep desugar/checker/compiler walks, and the ones whose stack the constants are sized
/// against — keep the full accepted depth. Full-depth `ast` would take hours; this keeps the whole
/// test a few seconds. The `ast` arm is therefore a smoke check that `cmd_ast`'s front-end stack hop
/// is wired, not a depth test: the depth at which `ast` becomes a stack hazard is unreachable in any
/// tolerable wall-clock, which is why W7-50 classified it a LATENT crash path, not a live one.
#[test]
fn worst_accepted_nesting_never_signal_crashes() {
    // Each level adds 1 `Call` (`f(`) + 1 `Call` (`g(`) + `folds` `Binary` nodes to the deepest
    // root-to-leaf path, and the folds are parsed AFTER the descent — the composition an ambient
    // depth counter cannot see. `lv` is chosen to land just under `MAX_AST_DEPTH`.
    let folds = 98usize;
    let per_level = folds + 2;
    let deepest = |lv: usize| {
        let mut s = String::from("0");
        for _ in 0..lv {
            s = format!("f(g({s}){})", "+1".repeat(folds));
        }
        format!(
            "fn f(a: int) -> int:\n    return a\nfn g(a: int) -> int:\n    return a\nprint({s})\n"
        )
    };
    // Bisect the largest level count the binary actually accepts, so the fixture cannot silently go
    // shallow when a constant moves.
    let t = TmpDir::new();
    let accepted = |lv: usize| {
        let f = t.write("probe.chz", &deepest(lv));
        let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .args(["check", f.to_str().unwrap()])
            .output()
            .expect("run chezzi check");
        assert!(
            out.status.code().is_some(),
            "chezzi check must not signal-crash at lv={lv}"
        );
        !String::from_utf8_lossy(&out.stderr).contains("too deeply")
    };
    let mut lo = 1usize;
    let mut hi = chezzi::parser::MAX_AST_DEPTH / per_level + 20;
    assert!(!accepted(hi), "no fold-depth boundary below lv={hi}");
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if accepted(mid) { lo = mid } else { hi = mid }
    }
    let lv = lo;
    assert!(
        lv * per_level > chezzi::parser::MAX_AST_DEPTH / 2,
        "worst accepted AST is only ~{} nodes deep — the fixture went shallow, not the parser",
        lv * per_level
    );

    let src = deepest(lv);
    let f = t.write("worst.chz", &src);
    let path = f.to_str().unwrap();

    // `ast` on a ~200-node chain (see the cost note above), stdout discarded.
    let shallow = t.write(
        "worst_shallow.chz",
        &format!("x := 1{}\n", "+1".repeat(200)),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["ast", shallow.to_str().unwrap()])
        .stdout(std::process::Stdio::null())
        .output()
        .expect("run chezzi ast");
    assert!(
        out.status.code().is_some(),
        "chezzi ast on a deep AST must not signal-crash the host"
    );

    for cmd in ["check", "run"] {
        let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .args([cmd, path])
            .output()
            .unwrap_or_else(|e| panic!("run chezzi {cmd}: {e}"));
        assert!(
            out.status.code().is_some(),
            "chezzi {cmd} on the worst accepted nesting (lv={lv}, ~{} nodes deep) must not \
             signal-crash the host — lower parser::MAX_AST_DEPTH",
            lv * per_level
        );
        assert!(
            out.status.success(),
            "chezzi {cmd} on the worst accepted nesting must succeed, got {:?}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

/// One level of `"{ <deep> }".len()` nesting, the shape whose budgets used to compose.
///
/// `quote` must DIFFER per level: the fragment text is spliced back into a literal of the enclosing
/// level, so reusing a delimiter fails to lex at level 3. Four styles nest four levels unescaped.
/// The `k` paren wraps go OUTSIDE the closing quote on purpose — that is what makes the enclosing
/// parser and the fragment parser each spend a budget of their own. And every value stays `int`
/// (hence `.len()` after each literal, and `+1` folds), because a type error short-circuits before
/// the walk that used to abort.
fn interp_layer(inner: &str, quote: &str, k: usize, folds: usize) -> String {
    let mut s = format!("{quote}{{ {inner}{} }}{quote}.len()", "+1".repeat(folds));
    for _ in 0..k {
        s = format!("({s}{})", "+1".repeat(499));
    }
    s
}

fn interp_nest(levels: usize, k: usize, folds: usize) -> String {
    const QUOTES: [&str; 4] = ["\"", "'", "\"\"\"", "'''"];
    let mut s = String::from("1");
    for i in 0..levels {
        s = interp_layer(&s, QUOTES[i % 4], k, folds);
    }
    format!("print({s})\n")
}

/// **THE COMPOSED-BUDGET REGRESSION** (W7-50 task 3b). `parser::MAX_AST_DEPTH` is enforced by the
/// `Parser` that builds a tree, and an interpolated `{…}` fragment is built by a *second* `Parser`
/// (`interpolation::parse_expr_str`) whose counters start at zero — so the budgets composed and the
/// bound was per-parse, not global. `desugar::Walker::walk_expr` now bounds the composed depth,
/// because it re-enters its own walk on the fragment's subtree and therefore sees the real total.
///
/// **Pre-fix observation (`e1137096`), recorded so this test's detection is not a guess:** debug
/// `chezzi run` on the three-level fixture below exited 134 with
/// `thread '<unknown>' has overflowed its stack / fatal runtime error: stack overflow, aborting` —
/// an uncatchable host abort on a program that `chezzi check` accepted (rc 0). Release did not abort
/// but accepted ~46 000 nodes against a ~33 100-node walker cliff. So arm (b) fails pre-fix in BOTH
/// profiles: `code() == None` in debug, a missing diagnostic in release. Driven through the real
/// binary in a subprocess so a regression is an assertion failure instead of killing the test run.
///
/// This cannot be a `tests/chz/` test: `recover:` catches runtime faults, and this refusal is a
/// compile-time resolve error — and a host SIGABRT is not observable from inside the program at all.
#[test]
fn composed_interp_depth_is_bounded_globally() {
    let t = TmpDir::new();
    let cap = chezzi::parser::MAX_AST_DEPTH;

    // (a0) THE TIGHT one, and a FIXED number on purpose. A cap-relative fixture cannot see a
    // narrowing narrower than its own slack, so it would miss exactly the regression this arm
    // exists for: charging an interpolation level twice cost ~2 nodes and pushed a lone
    // `x := "{ 1+1×15996 }".len()` — inside the parser's own flat ceiling of 15 997 — over the cap.
    // 15 996 is the last folds count that builds; it must keep building.
    let lone = t.write(
        "interp_lone.chz",
        "x := \"{ 1{} }\".len()\n"
            .replace("{}", &"+1".repeat(15_996))
            .as_str(),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["check", lone.to_str().unwrap()])
        .output()
        .expect("run chezzi check");
    assert!(
        out.status.success(),
        "a LONE 15 996-fold interpolation fragment must still build — the composed bound is \
         charging an interpolation level more than the one AST level it occupies: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // (a) NO OVER-REJECTION, near the boundary: one level, ~cap−95 nodes deep, still runs. The
    // fragment holds `1 +1×folds` (value `folds + 1`), the literal stringifies it, `.len()` is its
    // digit count, and the single paren wrap adds 499 more.
    let folds = cap - 600;
    let deep_ok = t.write("interp_ok.chz", &interp_nest(1, 1, folds));
    let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
        .args(["run", deep_ok.to_str().unwrap()])
        .output()
        .expect("run chezzi run");
    assert!(
        out.status.success(),
        "a one-level interpolation ~{} nodes deep must still run — the composed bound has narrowed \
         a single-parse program: {}",
        folds + 502,
        String::from_utf8_lossy(&out.stderr)
    );
    let expect = (folds + 1).to_string().len() + 499;
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        expect.to_string(),
        "…and must still compute the right value"
    );

    // (b) THE REGRESSION: three levels, each tuned just under the cap on its own
    // (`folds + k*499 = 15 485`), so ONLY composition can reject it. Must be a clean diagnostic —
    // never a signal kill, never an accept.
    let f = t.write("interp_deep.chz", &interp_nest(3, 15, 8_000));
    for cmd in ["check", "run"] {
        let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .args([cmd, f.to_str().unwrap()])
            .output()
            .unwrap_or_else(|e| panic!("run chezzi {cmd}: {e}"));
        assert!(
            out.status.code().is_some(),
            "chezzi {cmd} on three composed interpolation levels signal-crashed the host — the \
             composed AST-depth bound is gone"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !out.status.success() && stderr.contains("too deeply"),
            "chezzi {cmd} must refuse three composed levels with a depth diagnostic, got {:?}: \
             {stderr}",
            out.status
        );
        // Diagnostic quality: the enclosing context consumed the budget, so a bare "too deeply"
        // pointing inside a fragment would leave the user with no idea why.
        assert!(
            stderr.contains("interpolated") && stderr.contains(&cap.to_string()),
            "the diagnostic must name the interpolation and the limit, got: {stderr}"
        );
    }

    // (c) THE KNOWN RESIDUAL, pinned at the only invariant that actually holds today. A default
    // argument spliced in on desugar's SECOND pass is never walked (`regs` is raw, `normalize_call`
    // splices in the walk's tail, there is no pass 3), so a well-formed interpolated literal inside
    // one still reaches the checker and compiler un-converted and doubles the reachable depth to
    // ~31 986 nodes — ~1.03× the measured ~33 100-node cliff. Latent and PRE-EXISTING (accepted on
    // `e1137096` too), and the fix belongs in the two-pass driver W7-51 is rewriting, so this does
    // not assert a refusal. What it asserts is the line that must never be crossed: it may be
    // accepted, it may be refused, it must NEVER signal-crash the host. See
    // `desugar::Walker::walk_expr`'s residual note and `docs/gaps.md` W7-50.
    let f = 15_990;
    let splice = t.write(
        "interp_default_splice.chz",
        &format!(
            "fn g(a: int = \"{{ 1{f1} }}\".len()) -> int:\n    return a\n\n\
             fn h(b: int = g()) -> int:\n    return b\n\n\
             x := h(){f2}\nprint(x)\n",
            f1 = "+1".repeat(f),
            f2 = "+1".repeat(f),
        ),
    );
    for cmd in ["check", "run"] {
        let out = Command::new(env!("CARGO_BIN_EXE_chezzi"))
            .args([cmd, splice.to_str().unwrap()])
            .output()
            .unwrap_or_else(|e| panic!("run chezzi {cmd}: {e}"));
        assert!(
            out.status.code().is_some(),
            "chezzi {cmd} signal-crashed on the pass-2 default-splice seam — that residual went \
             from latent to LIVE; see desugar::Walker::walk_expr"
        );
    }
}
