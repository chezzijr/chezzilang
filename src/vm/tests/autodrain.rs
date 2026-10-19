//! VM tests: program-exit auto-drain, defer and recover.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ----- C5 (A2): program-exit auto-drain (VM parity) -----

#[test]
fn golden_executor_autodrain_matches_expected_and_interp() {
    let src = include_str!("../../../examples/executor_autodrain.chz");
    let expected = include_str!("../../../examples/executor_autodrain.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    // M:N auto-drains the two jobs on the pool — same lines, order may differ.
    assert_same_lines(&vm_out, &run_capture(src).expect("M:N run"));
}

#[test]
fn executor_autodrain_runs_unshut_at_exit() {
    let src = "import Executor from std.concurrency\nfn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\nmain()\n";
    assert_eq!(run(src), "0\n1\n2\n");
    // M:N drains the two jobs on the pool, racing `1`/`2`; same multiset (`0` is the parent's).
    assert_same_lines(&run(src), &run_capture(src).expect("M:N run"));
}

#[test]
fn executor_autodrain_not_redrained_after_explicit_shutdown() {
    let src = "import Executor from std.concurrency\nfn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.shutdown()\n    print(0)\nmain()\n";
    assert_eq!(run(src), "1\n0\n");
}

#[test]
fn executor_autodrain_survives_gc_stress() {
    // The VM executor registry roots un-shut work so it isn't collected before the exit drain.
    // Under collect-before-every-instruction, the drained closures must still be reachable.
    let src = "import Executor from std.concurrency\nfn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\nmain()\n";
    let normal = run(src);
    assert_eq!(
        run_capture_stress(src),
        normal,
        "VM gc_stress diverged (executor auto-drain rooting bug?)"
    );
}

#[test]
fn a_fire_and_forget_fault_ends_the_run_past_recover() {
    // TICKET-208 (owner decision 3) — a fire-and-forget job's fault ends the whole run (Go: a
    // goroutine panic). It replaces the W7-5 run-all contract, where `shutdown()` raised the
    // lowest-index fault and a `recover:` around it caught it: the fault reaches `main` as a
    // run-wide halt no `recover:` catches, so `done` never prints.
    let src = "import Executor from std.concurrency\nfn boom():\n    x := [1]\n    print(x[9])\nfn run():\n    ex := Executor()\n    ex.submit(fn(): print(\"A\"))\n    ex.submit(fn(): boom())\n    ex.submit(fn(): print(\"C\"))\n    ex.shutdown()\nfn main():\n    r := recover:\n        run()\n        0\n    print(\"done\")\nmain()\n";
    let (out, res) = run_program(src);
    let err = res.expect_err("the job's fault ends the run");
    assert!(
        err.message.contains("index 9 out of bounds"),
        "fault was: {}",
        err.message
    );
    assert!(
        !out.contains("done"),
        "a recover: caught the fault: {out:?}"
    );
}

/// TICKET-208 — with two faulting fire-and-forget jobs, the FIRST fault to land is the run's
/// report (first writer wins), and that job's own output survives.
#[test]
fn the_first_fire_and_forget_fault_is_the_report() {
    let src = r#"
import Executor from std.concurrency

fn boom_a():
    print("a-before-fault")
    panic("boom a")

fn boom_b():
    print("b-before-fault")
    panic("boom b")

fn main():
    ex := Executor()
    ex.submit(boom_a)
    ex.submit(boom_b)
    r: None!Error = recover: ex.shutdown()
    match r:
        ?_: print("no fault")
        !e: print("fault: {e.message()}")

main()
"#;
    let (out, res) = run_program(src);
    let msg = res.expect_err("a job fault ends the run").message;
    let who = match msg.as_str() {
        "boom a" => "a",
        "boom b" => "b",
        _ => panic!("the report must be one job's own fault, got: {msg}"),
    };
    assert!(
        out.contains(&format!("{who}-before-fault")),
        "lost the reporting job's output: {out:?}"
    );
}

#[test]
fn spawn_first_error_aborts_siblings() {
    // The first task to fault aborts the remaining siblings and propagates out of `parallel:`.
    let src = "fn boom():\n    x := [1]\n    print(x[5])\nfn quiet():\n    print(\"ran\")\nfn main():\n    parallel:\n        spawn boom()\n        spawn quiet()\nmain()\n";
    let vm = run_err(src);
    let interp = match run_capture(src) {
        Ok(o) => panic!("expected error, got {o:?}"),
        Err(e) => e.message,
    };
    assert_eq!(vm, interp, "VM/interp error divergence");
}

#[test]
fn spawn_composes_with_recover() {
    // A task fault is catchable by a `recover:` enclosing the nursery (parity-checked).
    let src = "fn boom():\n    x := [1]\n    print(x[9])\nfn main():\n    r := recover:\n        parallel:\n            spawn boom()\n        0\n    print(\"recovered\")\nmain()\n";
    let vm = run_capture(src).expect("vm run");
    assert_eq!(vm, "recovered\n");
}

/// Block-scoped defer: assert VM == interp == `expected` for a snippet.
fn assert_defer_scope(src: &str, expected: &str) {
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected, "vm output");
    assert_eq!(
        run_capture(src).expect("repeat run"),
        expected,
        "interp output"
    );
}

/// A `for`-body defer runs at the END of each iteration (block scope), not at function return.
#[test]
fn defer_for_body_runs_per_iteration() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..3:\n        defer log(\"i={i}\")\n    log(\"done\")\nmain()\n",
        "i=0\ni=1\ni=2\ndone\n",
    );
}

/// A `while`-body defer runs at the END of each iteration.
#[test]
fn defer_while_body_runs_per_iteration() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    i := 0\n    while i < 3:\n        defer log(\"w={i}\")\n        i = i + 1\n    log(\"done\")\nmain()\n",
        "w=0\nw=1\nw=2\ndone\n",
    );
}

/// An `if`-branch defer fires at the branch's end, before the statement after the `if`.
#[test]
fn defer_if_branch_runs_at_branch_end() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    if true:\n        defer log(\"cleanup\")\n        log(\"work\")\n    log(\"after\")\nmain()\n",
        "work\ncleanup\nafter\n",
    );
}

/// A statement-form `match` arm defer fires at the arm's end.
#[test]
fn defer_match_arm_runs_at_arm_end() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    x := 1\n    match x:\n        1:\n            defer log(\"arm\")\n            log(\"body\")\n        _: log(\"other\")\n    log(\"after\")\nmain()\n",
        "body\narm\nafter\n",
    );
}

/// `continue` drains the current iteration's loop-body defers then advances; `break` drains them
/// then leaves the loop.
#[test]
fn defer_break_continue_drain_loop_body() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..4:\n        defer log(\"d{i}\")\n        if i == 1:\n            continue\n        if i == 2:\n            break\n        log(\"body{i}\")\nmain()\n",
        "body0\nd0\nd1\nd2\n",
    );
}

/// `defer:` block form — the body runs top-to-bottom at scope exit, but is LIFO *as a unit*
/// relative to a surrounding single-call defer. `MakeClosure` + `DeferCall(0)` on the VM;
/// `Deferred::Block` on the interp. Asserted byte-identical.
#[test]
fn defer_block_form_runs_in_order_lifo_as_unit() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    defer log(\"outer\")\n    defer:\n        log(\"b1\")\n        log(\"b2\")\n    log(\"body\")\nmain()\n",
        "body\nb1\nb2\nouter\n",
    );
}

/// Uniform by-reference capture (E1): a `defer:` block shares the enclosing binding's cell (it runs
/// in the same task), so it sees the LATEST value of a captured local when it runs at frame exit —
/// `x` is `2` by then, not the `1` it held at the defer point.
#[test]
fn defer_block_form_shares_latest_value() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    x := 1\n    defer:\n        log(\"x={x}\")\n    x = 2\n    log(\"now={x}\")\nmain()\n",
        "now=2\nx=2\n",
    );
}

/// An error handled INSIDE a `defer:` block: a `?` there is a check error (TICKET-274: the block
/// has no caller), so the cleanup `match`es the error or catches it in a nested `recover:`, and the
/// rest of the block still runs.
#[test]
fn defer_block_form_handles_an_error_inside_the_block() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn risky(ok: bool) -> int!:\n    if ok:\n        return ?1\n    return !\"boom\"\nfn main() -> int!:\n    defer:\n        log(\"clean start\")\n        match risky(false):\n            ?n: log(\"clean {n}\")\n            !e: log(\"clean failed: {e}\")\n        r := recover: risky(false)?\n        log(\"recovered {r}\")\n        log(\"clean end\")\n    log(\"body\")\n    return ?0\nmain()\n",
        "body\nclean start\nclean failed: boom\nrecovered !boom\nclean end\n",
    );
}

/// A `defer:` block in a loop body runs per-iteration and drains on `break` (exercises
/// `EnterDeferScope`/`LeaveDeferScope` wrapping the closure-thunk defer).
#[test]
fn defer_block_form_per_iteration_and_break() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..3:\n        defer:\n            log(\"d{i}a\")\n            log(\"d{i}b\")\n        if i == 1:\n            break\n        log(\"body{i}\")\n    log(\"done\")\nmain()\n",
        "body0\nd0a\nd0b\nd1a\nd1b\ndone\n",
    );
}

/// A `break` nested inside an `if` (its own defer scope) inside the loop drains BOTH the
/// if-branch and the loop-body defers, inner-first, before leaving — the post-loop `done` must
/// print AFTER the cleanup (proving the drain happens at break, not at function return).
#[test]
fn defer_break_inside_if_drains_inner_first() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..3:\n        defer log(\"loop{i}\")\n        if i == 1:\n            defer log(\"if{i}\")\n            break\n        log(\"body{i}\")\n    log(\"done\")\nmain()\n",
        "body0\nloop0\nif1\nloop1\ndone\n",
    );
}

/// A `recover:`-block defer runs at the recover boundary on the **Ok** path — after the trailing
/// expression is evaluated, before the result is bound.
#[test]
fn defer_recover_runs_on_ok_path() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn risky(ok: bool) -> int!:\n    if ok:\n        return ?1\n    return !\"boom\"\nfn main():\n    y := 0\n    r := recover:\n        defer log(\"release\")\n        y = risky(true)?\n    log(\"got\")\n    match r:\n        ?_: print(\"ok {y}\")\n        !e: print(\"err {e.message()}\")\nmain()\n",
        "release\ngot\nok 1\n",
    );
}

/// A `recover:`-block defer runs on the **`?` short-circuit** path, before the propagated
/// `Err`/`None` is bound as the recover's result.
#[test]
fn defer_recover_runs_on_try_path() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn risky(ok: bool) -> int!:\n    if ok:\n        return ?1\n    return !\"boom\"\nfn main():\n    r := recover:\n        defer log(\"release\")\n        x := risky(false)?\n        x\n    log(\"got\")\n    match r:\n        ?v: print(\"ok {v}\")\n        !e: print(\"err {e.message()}\")\nmain()\n",
        "release\ngot\nerr boom\n",
    );
}

/// A `recover:`-block defer runs on the **genuine-fault** path, as the panic unwinds to the
/// boundary, before the `Err(message)` is bound.
#[test]
fn defer_recover_runs_on_fault_path() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    r := recover:\n        defer log(\"release\")\n        xs := [1]\n        y := xs[5]\n        y\n    log(\"got\")\n    match r:\n        ?v: print(\"ok {v}\")\n        !e: print(\"err {e.message()}\")\nmain()\n",
        "release\ngot\nerr index 5 out of bounds (len 1)\n",
    );
}

/// A defer that itself faults during a `recover:` unwind supersedes the in-flight result — its
/// fault becomes the recover's `Err`.
#[test]
fn defer_recover_fault_supersedes() {
    assert_defer_scope(
        "fn boom():\n    xs := [1]\n    x := xs[9]\nfn main():\n    r := recover:\n        defer boom()\n        42\n    match r:\n        ?v: print(\"ok {v}\")\n        !e: print(\"err {e.message()}\")\nmain()\n",
        "err index 9 out of bounds (len 1)\n",
    );
}

/// A `break` in an INNER loop drains only that loop's body defers; the outer loop-body defer
/// still fires at the end of each outer iteration. Locks the per-loop `defer_floor` capture.
#[test]
fn defer_inner_loop_break_drains_only_inner() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..2:\n        defer log(\"outer{i}\")\n        for j in 0..3:\n            defer log(\"inner{i}-{j}\")\n            if j == 1:\n                break\n        log(\"after{i}\")\nmain()\n",
        "inner0-0\ninner0-1\nafter0\nouter0\ninner1-0\ninner1-1\nafter1\nouter1\n",
    );
}

/// A defer scope (here an `if`) nested INSIDE a `recover:` block that faults must not leak its
/// scope marker past the recover boundary: the enclosing loop-body defer still drains at each
/// iteration's end, not at function return. (Regression: VM leaked `defer_markers` on the recover
/// catch path, corrupting later `LeaveDeferScope`s and diverging from the interp.)
#[test]
fn defer_nested_scope_in_faulting_recover_no_marker_leak() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    for i in 0..2:\n        defer log(\"loop{i}\")\n        r := recover:\n            if true:\n                defer log(\"inner{i}\")\n                xs := [1]\n                y := xs[5]\n                y\n            0\n        log(\"end{i}\")\nmain()\n",
        "inner0\nend0\nloop0\ninner1\nend1\nloop1\n",
    );
}

/// Same leak via the `?`-short-circuit catch path (not a genuine fault): a defer scope nested in
/// the recover block must not strand its marker when `?` jumps to the boundary.
#[test]
fn defer_nested_scope_in_try_recover_no_marker_leak() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn boom() -> int!:\n    return !\"x\"\nfn main():\n    for i in 0..2:\n        defer log(\"loop{i}\")\n        r := recover:\n            if true:\n                defer log(\"inner{i}\")\n                n := boom()?\n                n\n            0\n        log(\"end{i}\")\nmain()\n",
        "inner0\nend0\nloop0\ninner1\nend1\nloop1\n",
    );
}

/// Top-level (module-body) defers run LIFO when the program ends normally.
#[test]
fn defer_top_level_runs_lifo_at_exit() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\ndefer log(\"first\")\ndefer log(\"second\")\nlog(\"body\")\n",
        "body\nsecond\nfirst\n",
    );
}
