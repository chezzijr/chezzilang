//! Golden tests: widening, generic fn values, free-variable capture, generators crossing tasks,
//! turbofish, struct keys, socket byte reads.
//! Split out of `src/vm/golden_tests.rs`. `super::*` is the parent module.

use super::*;

// ===== Generic fn as a VALUE (scope A + B) — runtime is generic-ERASED, so a RUN test is still
// required per accepted case (the bind-import trap: a checker-accept that faults at runtime).

#[test]
fn generic_fn_value_turbofish_parity() {
    // B — turbofish on a fn value.
    assert_golden_out(
        "fn ident[T](x: T) -> T:\n    return x\n\ng := ident[int]\nprint(g(5) + 1)\n",
        "6\n",
    );
}

#[test]
fn generic_fn_value_annot_parity() {
    // A1 — annotated binding.
    assert_golden_out(
        "fn ident[T](x: T) -> T:\n    return x\n\ng: fn(int) -> int = ident\nprint(g(5) + 1)\n",
        "6\n",
    );
}

#[test]
fn generic_fn_value_hofarg_parity() {
    // A2 — HOF argument.
    assert_golden_out(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn applyit(f: fn(int) -> int, x: int) -> int:\n    return f(x)\n\nprint(applyit(ident, 5) + 1)\n",
        "6\n",
    );
}

#[test]
fn generic_fn_value_return_parity() {
    // A3 — return position.
    assert_golden_out(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn getf() -> fn(int) -> int:\n    return ident\n\ng := getf()\nprint(g(5) + 1)\n",
        "6\n",
    );
}

#[test]
fn generic_fn_name_shadowed_local_index_parity() {
    // Compiler-erase shadow-safety: a fn-local binding that shadows a top-level generic fn name is a
    // REAL index (not an erased turbofish) on BOTH engines. (The shadow must be a fn-LOCAL — a
    // top-level `ident := …` would collide with `fn ident`, which the checker rejects.)
    assert_golden_out(
        "fn ident[T](x: T) -> T:\n    return x\n\nfn h():\n    ident := [10, 20, 30]\n    print(ident[1])\n\nh()\n",
        "20\n",
    );
}

// ===== Finding D — free-variable capture (closures/nested-fns capture only referenced names) =====
// Pre-fix bug: every MakeClosure site captured ALL visible locals via snapshot_entries(), dragging an
// unused non-sendable sibling (a closure value / live generator) across the spawn airlock → check-OK
// but run-fault ("spawn: this task value can't cross a worker boundary yet"). Fix: capture only the
// body's free-variable set. These tests prove the fix AND no under-capture.

#[test]
fn d_repro_unused_sibling_nested_fn_parity() {
    // The exact #D repro: unused non-sendable sibling closure value; task is a nested fn using only g.
    assert_golden_out(
        "g := 7\nfn main():\n    sibling := fn(x: int): x + 1\n    fn task():\n        print(g)\n    parallel:\n        spawn task()\nmain()\n",
        "7\n",
    );
}

#[test]
fn d_repro_unused_sibling_closure_value_parity() {
    // Same, but the spawned task is a CLOSURE VALUE (site 1: compile_closure).
    assert_golden_out(
        "g := 7\nfn main():\n    sibling := fn(x: int): x + 1\n    task := fn(): print(g)\n    parallel:\n        spawn task()\nmain()\n",
        "7\n",
    );
}

#[test]
fn d_repro_unused_generator_sibling_parity() {
    // The unused non-sendable sibling is a live GENERATOR (frame-holding, non-sendable); task uses only g.
    assert_golden_out(
        "g := 7\nfn gen() -> Iterator[int]:\n    yield 1\nfn main():\n    it := gen()\n    fn task():\n        print(g)\n    parallel:\n        spawn task()\nmain()\n",
        "7\n",
    );
}

#[test]
fn d_used_sibling_closure_crosses_by_value_parity() {
    // B3.3: the task ACTUALLY captures + uses a sibling CLOSURE (`inc`). This used to fault at the
    // spawn (a closure lowered to a by-reference `Handle`); since M3.4 the task gets the parent's
    // `inc` by handle and `inc(7)` runs → prints 8. (The Finding-D free-var filter is still
    // exercised: only `inc` is captured.)
    assert_golden_out(
        "g := 7\nfn main():\n    inc := fn(x: int): x + 1\n    fn task():\n        print(inc(g))\n    parallel:\n        spawn task()\nmain()\n",
        "8\n",
    );
}

#[test]
fn d_spawn_block_free_capture_parity() {
    // Site 4: `spawn:` block references one outer local while an unused non-sendable sibling is in scope.
    assert_golden_out(
        "fn main():\n    sibling := fn(x: int): x + 1\n    n := 5\n    parallel:\n        spawn:\n            print(n)\nmain()\n",
        "5\n",
    );
}

#[test]
fn d_defer_block_free_capture_parity() {
    // Site 5: `defer:` block references one outer local while an unused non-sendable sibling is in scope.
    assert_golden_out(
        "fn main():\n    sibling := fn(x: int): x + 1\n    n := 9\n    defer:\n        print(n)\n    print(\"body\")\nmain()\n",
        "body\n9\n",
    );
}

#[test]
fn d_no_undercapture_method_call_on_captured_receiver_parity() {
    // A method call on a captured receiver (outer local `xs`) inside a nested fn.
    assert_golden_out(
        "fn main():\n    xs := [1, 2]\n    fn add():\n        xs.push(3)\n    add()\n    print(xs)\nmain()\n",
        "[1, 2, 3]\n",
    );
}

#[test]
fn d_no_undercapture_interpolation_ref_parity() {
    // A local referenced ONLY inside string interpolation must still be captured.
    assert_golden_out(
        "fn main():\n    n := 42\n    f := fn(): print(\"n={n}\")\n    f()\nmain()\n",
        "n=42\n",
    );
}

#[test]
fn d_no_undercapture_grandparent_through_two_closures_parity() {
    // Transitive capture: a grandparent local referenced only by an inner nested fn (two levels
    // deep) must surface through the middle frame's free set.
    assert_golden_out(
        "fn main():\n    g := 100\n    fn outer():\n        fn inner():\n            print(g)\n        inner()\n    outer()\nmain()\n",
        "100\n",
    );
}

#[test]
fn d_no_undercapture_recursion_self_ref_parity() {
    // LETREC: the recursive self-name is free in its own body → stays captured → self-call resolves.
    assert_golden_out(
        "fn main():\n    fn fact(n: int) -> int:\n        if n <= 1:\n            return 1\n        return n * fact(n - 1)\n    print(fact(5))\nmain()\n",
        "120\n",
    );
}

#[test]
fn d_no_undercapture_ref_capture_mutate_parity() {
    // A nested fn that reads AND mutates a captured local by reference.
    assert_golden_out(
        "fn main():\n    c := 0\n    fn bump():\n        c = c + 1\n    bump()\n    bump()\n    print(c)\nmain()\n",
        "2\n",
    );
}

#[test]
fn d_no_undercapture_match_and_comprehension_parity() {
    // A match-bind and a comprehension over a captured collection inside a nested fn.
    assert_golden_out(
        "fn main():\n    xs := [1, 2, 3]\n    fn work():\n        doubled := [x * 2 for x in xs]\n        print(doubled)\n    work()\nmain()\n",
        "[2, 4, 6]\n",
    );
}

#[test]
fn d_no_undercapture_nonrecursive_nested_fn_parity() {
    // A NON-recursive nested fn: its own name is NOT free in its body → dropped from captures
    // harmlessly; must still bind and run.
    assert_golden_out(
        "fn main():\n    fn greet():\n        print(\"hi\")\n    greet()\nmain()\n",
        "hi\n",
    );
}

// ===== B3.3 (runtime) — closures / bare funcs reach a spawned task or a Channel receiver =====
// History: B3.3 copied a closure as data at the crossing; since M3.4 a task gets it by handle (the
// wire form is deleted, M3.5). A `spawn f()` callee whose captured environment contains a NESTED
// closure (or is a bare fn) runs, as it has since B3.3.

#[test]
fn closure_as_data_into_spawn_callee_parity() {
    // `work` is a closure whose captured env holds another closure (`double`). Crossing the spawn
    // callee used to fault (the nested closure lowered to a `Handle`); it now crosses by handle.
    let src = "\
fn main():
    double := fn(x: int) -> int: x * 2
    ch := Channel[int]()
    work := fn(): ch.send(double(21))
    parallel:
        spawn work()
    print(ch.recv())
main()";
    assert_golden_out(src, "42\n");
}

#[test]
fn nestedfn_as_data_into_spawn_callee_parity() {
    // Same shape, but with nested `fn` declarations rather than closure values.
    let src = "\
fn main():
    fn double(x: int) -> int:
        return x * 2
    ch := Channel[int]()
    fn work():
        ch.send(double(21))
    parallel:
        spawn work()
    print(ch.recv())
main()";
    assert_golden_out(src, "42\n");
}

#[test]
fn generator_captured_into_spawn_callee_crosses_by_value() {
    // F3 path C: a spawn callee capturing a LOCAL generator (`it`, Pending) crosses it by handle —
    // the task drives the parent's generator. The module global `g := 7` is a non-generator, unrelated to the
    // crossing. Byte-identical.
    let src = "\
g := 7
fn gen() -> Iterator[int]:
    yield 1
fn main():
    it := gen()
    fn task():
        for x in it:
            print(x)
    parallel:
        spawn task()
main()";
    assert_eq!(vm_outcome(src).expect("program should run"), "1\n");
}

// ----- A generator a task captures or receives is the parent's one generator -----
// F3 path C once rebuilt a crossing generator as an INDEPENDENT copy. M3.4: a spawn passes values by
// handle, so the task drives the parent's generator and the parent sees what is left (CPython
// `m34py/gen_spawn.py`, a thread: `pending [1, 2, 3, 0]`, `suspended [2, 3, 0]`,
// `closure_backed [101, 102, 0]`, `deep_parked_slot [1]`, `got 1` twice). All tests run on the M:N
// engine against a literal golden.

/// A PENDING generator (never driven) captured into a `spawn:` block: the task drives it fully, so
/// the sender's loop then yields nothing (CPython `pending [1, 2, 3, 0]`).
#[test]
fn generator_pending_captured_by_spawn_is_the_one_generator() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
    yield 3
fn main():
    it := gen()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
            out.send(0)
    print(out.recv())
    print(out.recv())
    print(out.recv())
    print(out.recv())
    for x in it:
        print(x)
main()";
    let expect = "1\n2\n3\n0\n";
    assert_eq!(vm_outcome(src).expect("program should run"), expect);
}

/// A SUSPENDED generator (driven once → one parked frame) captured into a `spawn:` block: the task
/// drives the REMAINDER and the sender's loop yields nothing (CPython `suspended [2, 3, 0]`).
#[test]
fn generator_suspended_captured_by_spawn_is_the_one_generator() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
    yield 3
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
            out.send(0)
    print(out.recv())
    print(out.recv())
    print(out.recv())
    for x in it:
        print(x)
main()";
    let expect = "2\n3\n0\n";
    assert_eq!(vm_outcome(src).expect("program should run"), expect);
}

/// A CLOSURE-backed generator (a nested `fn` capturing a local) captured into a `spawn:` block: the
/// task drives it over the captured `base`, and the sender's loop yields nothing (CPython
/// `closure_backed [101, 102, 0]`).
#[test]
fn generator_closure_backed_captured_by_spawn_is_the_one_generator() {
    let src = "\
fn main():
    base := 100
    fn gen() -> Iterator[int]:
        yield base + 1
        yield base + 2
    it := gen()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
            out.send(0)
    print(out.recv())
    print(out.recv())
    print(out.recv())
    for x in it:
        print(x)
main()";
    let expect = "101\n102\n0\n";
    assert_eq!(vm_outcome(src).expect("program should run"), expect);
}

/// A generator crossing over an explicit `Channel[Iterator[int]]` into a task: the parent SENDS it,
/// and M3.2 moves the generator itself, so the task drives the sender's `it` to its end and the
/// sender's loop then yields nothing. Go (`rewrites.go`): a stateful closure sent into a goroutine
/// prints `10`, `20`, and the sender's loop then prints nothing.
#[test]
fn generator_sent_over_channel_into_task_both() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 10
    yield 20
fn main():
    gench := Channel[Iterator[int]]()
    out := Channel[int]()
    it := gen()
    gench.send(it)
    parallel:
        spawn:
            g := gench.recv()
            for x in g:
                out.send(x)
    print(out.recv())
    print(out.recv())
    for x in it:
        print(x)
main()";
    let expect = "10\n20\n";
    assert_eq!(vm_outcome(src).expect("program should run"), expect);
}

/// A suspended generator whose PARKED SLOT holds a >10000-DEEP acyclic nested list once tripped the
/// `MAX_STRUCTURAL_DEPTH` guard of `to_wire_depth` at the crossing. M3.4: the spawn walks nothing, so
/// the task resumes the one generator (CPython `deep_parked_slot [1]`).
#[test]
fn generator_parked_slot_deep_value_resumes_in_a_task() {
    let src = "\
fn gen() -> Iterator[int]:
    keep: List[int] = []
    deep: List[List[int]] = [keep]
    for i in 0..10001:
        deep = [deep]
    yield 1
    yield deep.len()
fn main():
    it := gen()
    started := it.next()
    out := Channel[int](4)
    parallel:
        spawn:
            for x in it:
                out.send(x)
    print(out.recv())
main()";
    assert_eq!(vm_outcome(src).expect("program should run"), "1\n");
}

/// A PENDING generator `gen(box)` parks `box` in its call args and `box` holds the generator, so
/// `box -> g -> box` is a cycle through it. The copy once had to reject this cycle (a second copy of
/// the generator would have been a silent duplicate). M3.4: the task drives the one generator
/// (CPython `got 1`).
#[test]
fn generator_in_data_cycle_is_the_one_generator() {
    let src = "\
fn gen(box: List[Iterator[int]]) -> Iterator[int]:
    yield box.len()
fn main():
    box: List[Iterator[int]] = []
    g := gen(box)
    box.push(g)
    parallel:
        spawn:
            for x in g:
                print(\"got {x}\")
main()";
    assert_eq!(vm_outcome(src).expect("program should run"), "got 1\n");
}

/// The SUSPENDED shape of `generator_in_data_cycle_is_the_one_generator`: `keep := box` parks `box`
/// across the first `yield`, and `box.push(g)` closes `box -> g -> box`. M3.4: the task resumes the
/// one generator (CPython `got 1`).
#[test]
fn suspended_generator_in_data_cycle_is_the_one_generator() {
    let src = "\
fn gen(box: List[Iterator[int]]) -> Iterator[int]:
    keep := box
    yield 0
    yield keep.len()
fn main():
    box: List[Iterator[int]] = []
    g := gen(box)
    started := g.next()
    box.push(g)
    parallel:
        spawn:
            for x in g:
                print(\"got {x}\")
main()";
    assert_eq!(vm_outcome(src).expect("program should run"), "got 1\n");
}

/// A suspended generator whose PARKED SLOT holds a recursive local `fn` (`keep := rec`, a self-cycle
/// closure): the spawned task resumes the one generator and `keep(3) == 3` (M3.4: by handle).
#[test]
fn generator_carrying_recursive_closure_round_trips_both() {
    let src = "\
fn gen() -> Iterator[int]:
    fn rec(n: int) -> int:
        if n <= 0:
            return 0
        return rec(n - 1) + 1
    keep := rec
    yield 1
    yield keep(3)
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
    print(out.recv())
main()";
    assert_golden_out(src, "3\n");
}

// ----- A MODULE-GLOBAL live generator is the run's one generator (M3.1) ----- A task reads the
// run's module globals, so a task that reaches a module-global generator drives the one generator
// the parent holds, as a CPython thread drives a module-global generator. Goldens: CPython
// (`m31go/py/gen_globals.py`). A captured or passed generator is the one generator too (M3.4).

/// A module-global PENDING generator reached by a spawned task drives fully; the parent then finds
/// the one generator exhausted. CPython: `[1, 2, 3]`, then `[]`.
#[test]
fn generator_module_global_reached_is_the_one_generator() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
    yield 3
g := gen()
fn main():
    out := Channel[int]()
    parallel:
        spawn:
            for x in g:
                out.send(x)
    print(out.recv())
    print(out.recv())
    print(out.recv())
    print(\"parent:\")
    for x in g:
        print(x)
main()";
    let expect = "1\n2\n3\nparent:\n";
    assert_eq!(vm_outcome(src).expect("program should run"), expect);
}

/// A module-global generator driven ONCE at top level (suspended, one parked frame) then reached by
/// a spawned task RESUMES from its parked position.
#[test]
fn generator_module_global_suspended_reached_resumes_both() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
    yield 3
g := gen()
started := g.next()
fn main():
    out := Channel[int]()
    parallel:
        spawn:
            for x in g:
                out.send(x)
    print(out.recv())
    print(out.recv())
main()";
    let expect = "2\n3\n";
    assert_eq!(vm_outcome(src).expect("program should run"), expect);
}

/// TWO tasks reach the SAME module-global generator, one nursery after the other (two tasks that
/// drive one generator at once race, and the loser faults `generator already running`, CPython's
/// `ValueError: generator already executing`). The first drains it; the second and the parent find
/// it exhausted. CPython: `[1, 2, 3]`, `[]`, `[]`.
#[test]
fn generator_module_global_two_tasks_share_the_one_generator() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
    yield 3
g := gen()
fn main():
    a := Channel[int](3)
    b := Channel[int](3)
    parallel:
        spawn:
            for x in g:
                a.send(x)
    parallel:
        spawn:
            for x in g:
                b.send(x)
    print(\"a:\")
    print(a.recv())
    print(a.recv())
    print(a.recv())
    print(\"b: {b.len()}\")
    print(\"parent:\")
    for x in g:
        print(x)
main()";
    let expect = "a:\n1\n2\n3\nb: 0\nparent:\n";
    assert_eq!(vm_outcome(src).expect("program should run"), expect);
}

/// A module-global generator whose parked slot is a >10000-deep acyclic nest (past
/// `MAX_STRUCTURAL_DEPTH`): nothing crosses, so a task that reaches it resumes the one generator.
/// It used to fault (the snapshot could not copy it). CPython: `[1]`.
#[test]
fn generator_module_global_parked_slot_too_deep_to_copy_resumes_in_a_task() {
    let src = "\
fn gen() -> Iterator[int]:
    keep: List[int] = []
    deep: List[List[int]] = [keep]
    for i in 0..10001:
        deep = [deep]
    yield 1
    yield deep.len()
g := gen()
started := g.next()
fn main():
    out := Channel[int](2)
    parallel:
        spawn:
            for x in g:
                out.send(x)
    print(out.recv())
main()";
    assert_eq!(vm_outcome(src).expect("program should run"), "1\n");
}

/// TICKET-137 — a module-global generator in a reference CYCLE through `box`: the task drives the
/// run's one generator (M3.1), so `box.len()` is `1`. (A frame-local one does too since M3.4:
/// `generator_in_data_cycle_is_the_one_generator`.)
#[test]
fn generator_module_global_in_data_cycle_crosses_as_one_copy() {
    let src = "\
fn gen(box: List[Iterator[int]]) -> Iterator[int]:
    yield box.len()
box: List[Iterator[int]] = []
g := gen(box)
pushed := box.push(g)
fn main():
    parallel:
        spawn:
            for x in g:
                print(\"got {x}\")
main()";
    assert_eq!(vm_outcome(src).expect("program should run"), "got 1\n");
}

/// REGRESSION LOCK (backlog item B remediation): a module that merely HOLDS a module-global
/// generator with a parked slot nested past `MAX_STRUCTURAL_DEPTH`, and whose spawned task never
/// reaches it, runs CLEAN (M3.1: a spawn copies no module global). Reached-behaviour:
/// `generator_module_global_parked_slot_too_deep_to_copy_resumes_in_a_task`.
#[test]
fn generator_module_global_unreached_nonsendable_runs_clean_both() {
    let src = "\
fn gen() -> Iterator[int]:
    keep: List[int] = []
    deep: List[List[int]] = [keep]
    for i in 0..10001:
        deep = [deep]
    yield 1
    yield deep.len()
g := gen()
started := g.next()
fn hello():
    print(\"hello\")
fn main():
    parallel:
        spawn hello()
    print(\"done\")
main()";
    let expect = "hello\ndone\n";
    assert_eq!(vm_outcome(src).expect("program should run"), expect);
}

/// The deleted `gate_executor_queue` path: a module-global generator reached inside an
/// `Executor.submit` job drives the run's one generator (M3.1).
#[test]
fn generator_module_global_via_executor_crosses_both() {
    let src = "import Executor from std.concurrency\n\
fn gen() -> Iterator[int]:
    yield 1
    yield 2
g := gen()
out := Channel[int]()
fn job():
    for x in g:
        out.send(x)
fn main():
    ex := Executor()
    ex.submit(job)
    ex.shutdown()
    print(out.recv())
    print(out.recv())
main()";
    let expect = "1\n2\n";
    assert_eq!(vm_outcome(src).expect("program should run"), expect);
}

/// Backlog arm (b) — a generator suspended INSIDE a `recover:` block (a LIVE handler in its parked
/// context) now CROSSES a spawn and RESUMES with its recover boundary intact. The generator is
/// advanced once at top level (suspending at `yield 1`, inside the recover), sent into a spawned
/// task, and driven to exhaustion there. The recover block completes NORMALLY after crossing (the
/// trailing `99` becomes `Ok(99)`), and the bound `r` flows through the post-crossing `match` — so
/// the resumed values are `2, 3, 99`: the task drives the parent's generator, `Handler` included
/// (M3.4). Was a clean HARD-ARM reject; now a resume-semantics test.
#[test]
fn generator_recover_suspended_resumes_both() {
    let src = "\
fn gen() -> Iterator[int]:
    v := 0
    r := recover:
        yield 1
        yield 2
        v = 99
    yield 3
    match r:
        ?_: yield v
        !e: yield -1
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
    print(out.recv())
    print(out.recv())
    print(out.recv())
main()";
    assert_golden_out(src, "2\n3\n99\n");
}

/// Backlog arm (b), the item-#2 SEMANTIC guard (not just a non-faulting round-trip): after crossing
/// a spawn, the generator's `recover:` must actually CATCH a post-crossing fault and produce the
/// correct recovered value — not silently drop the handler and let the fault abort the task. The
/// generator suspends at `yield 10` inside the recover; when resumed in the spawned task the `1 / 0`
/// fires, the recover catches it (`r = Err`), and the post-recover `match` yields `99`
/// down the Err arm. Asserted EQUAL to a no-crossing control that drives the identical
/// generator inline (`generator_recover_fault_control_inline`): if the crossing lost the handler,
/// the crossed fault would be UNCAUGHT and abort the task, diverging from the control.
#[test]
fn generator_crossed_recover_catches_fault_matches_control_both() {
    let src = "\
fn gen() -> Iterator[int]:
    r := recover:
        yield 10
        boom := 1 / 0
        boom
    yield 20
    match r:
        ?v: yield v
        !e: yield 99
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            for x in it:
                out.send(x)
    print(out.recv())
    print(out.recv())
main()";
    // Expected value is pinned by the same-heap control below (plain, well-tested generator
    // semantics with no crossing): catch → r=Err → yield 20, then match Err → yield 99.
    assert_golden_out(src, "20\n99\n");
}

/// NO-crossing control for `generator_crossed_recover_catches_fault_matches_control_both`:
/// the identical generator driven INLINE (never crossing a task boundary). This pins the correct
/// recovered semantics with the plain, well-exercised generator+recover path; the crossed test must
/// match this exact output or the crossing changed the computation.
#[test]
fn generator_recover_fault_control_inline() {
    let src = "\
fn gen() -> Iterator[int]:
    r := recover:
        yield 10
        boom := 1 / 0
        boom
    yield 20
    match r:
        ?v: yield v
        !e: yield 99
fn main():
    it := gen()
    started := it.next()
    for x in it:
        print(x)
main()";
    assert_golden_out(src, "20\n99\n");
}

/// Backlog arm (b), the nursery-floor rebase (load-bearing, SERIAL oracle). A generator's `recover:`
/// handler captures an ABSOLUTE `nursery_len` at `PushHandler` — here `0` (the recover is entered by
/// the top-level `it.next()`, before any `parallel:`). The generator is then RESUMED at a DEEPER
/// nursery floor: inside a `parallel:` (nurseries==1) in the parent fiber, alongside a sibling
/// `spawn`. When the resumed `1 / 0` is caught, the recover-catch path calls
/// `drain_escaped_nursery(handler.nursery_len)`. Without the resume-time rebase that stale `0` would
/// truncate the LIVE parallel nursery to `0`, cancelling the sibling `spawn` (`out.send(77)`), and
/// `out.recv()` would then deadlock. A generator provably cannot open a nursery (spawn/parallel are
/// checker-banned inside one), so its handler's escape-drain must be a no-op — `generator_next`
/// rebases every parked frame/handler `nursery_len` to the resuming driver's floor. This was
/// historically serial-only: the drain was deterministic on the since-removed cooperative engine
/// (the sibling is an unstarted `PendingCall` when the drain would fire); on M:N the sibling may
/// already be running on another worker, so the bug was not deterministically reachable there —
/// the shared `generator_next` fix covers both.
#[test]
fn generator_crossed_recover_fault_leaves_siblings_intact_serial() {
    let src = "\
fn gen() -> Iterator[int]:
    r := recover:
        yield 1
        boom := 1 / 0
        yield boom
    yield 2
fn main():
    it := gen()
    started := it.next()
    out := Channel[int]()
    parallel:
        spawn:
            out.send(77)
        for x in it:
            print(x)
    print(out.recv())
main()";
    assert_eq!(
        vm_outcome(src),
        Ok("2\n77\n".to_string()),
        "the sibling spawn's sentinel must survive the recover-caught fault (nursery_len rebase)",
    );
}

/// Backlog item B, cross-module path: a spawned task calls a module-member fn `genmod.bar(10)` whose
/// body drives a live generator global in ITS OWN module. The task drives the run's one global
/// generator (M3.1): `10 + 1 + 2 = 13`.
/// Byte-identical (was the cross-module reach-gate fault).
#[test]
fn generator_cross_module_member_call_crosses_both() {
    let t = TmpDir::new();
    t.write(
        "genmod.chz",
        "fn gen() -> Iterator[int]:\n    yield 1\n    yield 2\ng := gen()\nfn bar(n: int) -> int:\n    total := n\n    for x in g:\n        total = total + x\n    return total\n",
    );
    let ep = t.write(
        "main.chz",
        "import genmod\nparallel:\n    spawn:\n        r := genmod.bar(10)\n        print(r)\n",
    );
    let (so, _, sr, _) = run_file(&ep);
    let (po, _, pr, _) = run_file(&ep);
    sr.expect("serial cross-mod generator crossing");
    pr.expect("M:N cross-mod generator crossing");
    assert_eq!(so, "13\n");
    assert_eq!(po, "13\n");
}

/// A CLEAN cross-module member call (`geomod.helper()` reads no generator) runs clean
/// with a generator global resident in the imported module.
#[test]
fn genreach_cross_module_clean_member_call_ok_both() {
    let t = TmpDir::new();
    t.write(
        "geomod.chz",
        "fn gen() -> Iterator[int]:\n    yield 1\ng := gen()\nfn helper(n: int) -> int:\n    return n + 1\n",
    );
    let ep = t.write(
        "main.chz",
        "import geomod\nparallel:\n    spawn:\n        r := geomod.helper(10)\n        print(r)\n",
    );
    let (so, _, sr, _) = run_file(&ep);
    let (po, _, pr, _) = run_file(&ep);
    sr.expect("serial clean cross-mod member call");
    pr.expect("M:N clean cross-mod member call");
    assert_eq!(so, "11\n");
    assert_eq!(po, "11\n");
}

#[test]
fn bare_func_crosses_spawn_callee_renders_fn_name_parity() {
    // A bare top-level fn captured into a spawn callee stays an `Obj::Func` (not a Closure), so
    // `str()` renders `<fn NAME>` in the task (M3.4: the task gets it by handle).
    let src = "\
fn helper(x: int) -> int:
    return x + 1
fn main():
    ch := Channel[str]()
    f := helper
    fn task():
        ch.send(str(f))
    parallel:
        spawn task()
    print(ch.recv())
main()";
    assert_golden_out(src, "<fn helper>\n");
}

// ===== B3.3 (Task 2a) — closures-as-data at spawn/Channel sites RUN =====
// The checker half (permissive `sendable(Func)` + capture gate) makes these type-check; here we pin
// that they actually RUN (the task gets each closure by handle).

#[test]
fn channel_of_closures_runs_parity() {
    // A `Channel[fn() -> int]`: a capture-free closure is sent over the channel and called by the
    // consumer — crosses by handle, runs 42.
    let src = "\
fn producer(ch: Channel[fn() -> int]):
    ch.send(fn() -> int: 42)
fn main():
    ch := Channel[fn() -> int]()
    parallel:
        spawn producer(ch)
    f := ch.recv()
    print(f())
main()";
    assert_golden_out(src, "42\n");
}

#[test]
fn closure_returned_across_task_runs_parity() {
    // A CAPTURING closure (`adder(100)` closes over the int `n=100`) returned from a factory and sent
    // over a `Channel[fn(int) -> int]` — its capture is sendable, so it crosses by handle; `f(5)` = 105
    //.
    let src = "\
fn adder(n: int) -> fn(int) -> int:
    return fn(x: int) -> int: x + n
fn producer(ch: Channel[fn(int) -> int]):
    ch.send(adder(100))
fn main():
    ch := Channel[fn(int) -> int]()
    parallel:
        spawn producer(ch)
    f := ch.recv()
    print(f(5))
main()";
    assert_golden_out(src, "105\n");
}

#[test]
fn sendable_capturing_closure_through_channel_still_runs() {
    // A closure capturing ONLY sendable data (an int + a `List[int]`) sent over a `Channel[fn() -> int]`
    // still crosses by handle and runs. `21*2 + 1` = 43.
    let src = "\
fn main():
    n := 21
    nums := [1, 2, 3]
    ch := Channel[fn() -> int]()
    fn calc() -> int:
        return n * 2 + nums[0]
    ch.send(calc)
    f := ch.recv()
    print(f())
main()";
    assert_golden_out(src, "43\n");
}

#[test]
fn parallel_block_defer_flushes_after_join_parity() {
    // A `defer` inside an explicit `parallel:` block flushes AFTER the block's children join at the
    // dedent — same order as the implicit function-body nursery. Pre-fix the defer fired BEFORE the
    // spawned child joined (block-defer/child-body/after); now: child-body/block-defer/after.
    let src = "\
fn log(m: str): print(m)
fn child(): log(\"child-body\")
fn main():
    parallel:
        spawn child()
        defer log(\"block-defer\")
    log(\"after\")
main()";
    assert_golden_out(src, "child-body\nblock-defer\nafter\n");
}

#[test]
fn parallel_block_defer_close_after_join_parity() {
    // A deferred channel close inside `parallel:` must run AFTER the spawned send joins — the natural
    // cleanup pattern. Pre-fix the close raced ahead of the send ("send on a closed channel"); now the
    // send buffers before close, so `ch.len()` is 1.
    let src = "\
fn snd(ch: Channel[int]): ch.send(1)
fn main():
    ch := Channel[int]()
    r := recover:
        parallel:
            spawn snd(ch)
            defer ch.close()
        1
    match r:
        ?v: print(\"ok, len={ch.len()}\")
        !e: print(\"err: {e.message()}\")
main()";
    assert_golden_out(src, "ok, len=1\n");
}

#[test]
fn parallel_block_defer_break_flushes_once_parity() {
    // Regression for the break/early-jump path: a loop whose body is a `parallel:` block with a
    // `defer` then a `break` still flushes the block's defer EXACTLY once (the jump path drains it;
    // the fall-through JoinNursery+LeaveDeferScope is skipped). Guards that the reorder didn't botch
    // the defer_scopes/nursery_scopes counter bracketing.
    let src = "\
fn log(m: str): print(m)
fn main():
    i := 0
    while i < 3:
        parallel:
            defer log(\"d\")
            break
        i = i + 1
    log(\"done\")
main()";
    assert_golden_out(src, "d\ndone\n");
}

// ----- B1: qualified generic turbofish in expression position (mod.Type[int].Variant / .static) -----

/// The shared multi-generic fixture: an enum `Tree[T]` (variant + method) and a struct `Box[T]`
/// (static method), imported whole-module so the base is only reachable qualified (`shapes.Tree`).
const SHAPES_MOD: &str = "enum Tree[T]:\n    Leaf(T)\n    Branch(Tree[T], Tree[T])\n    fn first(self) -> T:\n        match self:\n            Tree.Leaf(x): return x\n            Tree.Branch(l, r): return l.first()\n\nstruct Box[T]:\n    v: T\n    fn make(x: T) -> Box[T]:\n        return Box(x)\n";

#[test]
fn qualified_enum_variant_turbofish_runs() {
    let out = golden_file_entry(
        &[
            ("shapes.chz", SHAPES_MOD),
            (
                "main.chz",
                "import shapes\nx := shapes.Tree[int].Leaf(9)\nprint(x.first())\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "9\n");
}

#[test]
fn qualified_struct_static_turbofish_runs() {
    let out = golden_file_entry(
        &[
            ("shapes.chz", SHAPES_MOD),
            (
                "main.chz",
                "import shapes\nb := shapes.Box[int].make(5)\nprint(b.v)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "5\n");
}

#[test]
fn qualified_combined_turbofish_runs() {
    // Combined qualified turbofish `mod.Type[int].static[U](args)` — the enclosing `[int]` AND the
    // method-own `[U]` are both runtime-erased. The checker accepts it (qualified head recognized),
    // so the compiler must lower it (not fall to CallMethod). Assert VALUE correctness, not just
    // engine agreement (identical-but-wrong bytecode would still agree).
    let out = golden_file_entry(
        &[
            (
                "shapes.chz",
                "struct Box[T]:\n    v: T\n    fn wrap[U](x: T, tag: U) -> Box[T]:\n        return Box(x)\n",
            ),
            (
                "main.chz",
                "import shapes\nb := shapes.Box[int].wrap[str](7, \"hi\")\nprint(b.v)\n",
            ),
        ],
        "main.chz",
    );
    assert_eq!(out, "7\n");
}

// ----- Map/Set snapshot a struct/enum key on INSERT (Go value-key model) -----
// A struct/enum key/element is deep-copied when STORED, so a later mutation of the
// caller's original value cannot reach (corrupt) the stored key. Scalars pass through unchanged;
// map VALUES are never copied; the transient lookup key is never snapshotted.

/// Test A — a struct key is snapshotted on insert: mutating the original after `m[a]=..` leaves
/// the stored key intact (probe by an equal fresh key), and `m.keys()` shows the PRE-mutation key.
#[test]
fn map_struct_key_snapshot_on_insert() {
    let src = r#"
struct K:
    x: int
    fn hash(self) -> int: return self.x
fn main():
    a := K(1)
    m: Map[K, str] = {}
    m[a] = "one"
    a.x = 2                    # mutate the live key in place
    print(m[K(1)])             # still resolves — stored key was snapshotted at x=1
    print(m.contains(a))            # a is now K(2), not a stored key -> false (no fault)
    print(m.get(a))            # None (no fault)
    ks := m.keys()
    print(ks[0].x)             # 1 — the snapshot, not the mutated original
main()
"#;
    assert_golden_out(src, "one\nfalse\nNone\n1\n");
}

/// Test B — a set element is snapshotted, so mutating the original after `{a, ..}` cannot break
/// the set invariant; difference/intersection/union/== stay correct.
#[test]
fn set_element_snapshot_algebra() {
    let src = r#"
struct K:
    x: int
    fn hash(self) -> int: return self.x
fn main():
    a := K(1)
    s := {a, K(2)}
    a.x = 2                              # would corrupt s if elements aliased
    print(s.len())                       # 2 (snapshots {x=1, x=2})
    print(s.difference({K(2)}).len())    # 1
    print(s.intersection({K(1)}).len())  # 1
    print(s.union({K(2)}).len())         # 2
    print(s == {K(1), K(2)})             # true
main()
"#;
    assert_golden_out(src, "2\n1\n1\n2\ntrue\n");
}

/// Test C — scalar keys (int/str) are unchanged (regression guard for the zero-clone hot path).
#[test]
fn scalar_keys_unchanged() {
    let src = r#"
fn main():
    m := {1: "a"}
    m[1] = "b"
    print(m[1])
    c: Map[str, int] = {}
    c["x"] = 1
    c["x"] = c["x"] + 1
    print(c["x"])
    print(Set.from_iter([3, 1, 3, 2]).len())
main()
"#;
    assert_golden_out(src, "b\n2\n3\n");
}

/// Test D — map VALUES are NOT snapshotted: mutating a stored value in place is intended.
#[test]
fn map_value_not_snapshotted() {
    let src = r#"
struct V:
    n: int
fn main():
    m: Map[int, V] = {}
    m[1] = V(5)
    m[1].n = 9                 # mutate the stored value in place
    print(m[1].n)              # 9 — value held by reference
main()
"#;
    assert_golden_out(src, "9\n");
}

/// Test — every insert entry point snapshots uniformly (map literal, set literal, set.insert,
/// Map(iterable), Set(iterable), map index-set). Mutate an original inserted via each path.
#[test]
fn all_insert_paths_snapshot() {
    let src = r#"
struct K:
    x: int
    fn hash(self) -> int: return self.x
fn main():
    # map literal
    a := K(1)
    ml := {a: "v"}
    a.x = 9
    print(ml.keys()[0].x)
    # set literal
    b := K(2)
    sl := {b}
    b.x = 9
    print(sl.difference({K(2)}).len())
    # set.insert
    c := K(3)
    sa := {K(0)}
    sa.insert(c)
    c.x = 9
    print(sa.difference({K(3)}).len())
    # Map.from_iter(iterable)
    d := K(4)
    mi := Map.from_iter([(d, "v")])
    d.x = 9
    print(mi.keys()[0].x)
    # Set.from_iter(iterable)
    e := K(5)
    si := Set.from_iter([e])
    e.x = 9
    print(si.difference({K(5)}).len())
    # map index-set (general struct-key path)
    f := K(6)
    ms: Map[K, str] = {}
    ms[f] = "v"
    f.x = 9
    print(ms.keys()[0].x)
main()
"#;
    assert_golden_out(src, "1\n0\n1\n4\n0\n6\n");
}

// ----- Regression: snapshot must NOT reuse a deep_clone (the deleted copy step's fault/remap modes)
// A struct/enum key that embeds a cyclic back-edge, a live generator, or an identity-typed
// (closure/channel/…) sub-value used to be stored BY REFERENCE and worked on a plain serial insert.
// The first cut routed the snapshot through the airlock's deep copy (deleted in M3.5), which FAULTED on the
// first two and gave the identity sub-value a FRESH handle (so `values_equal`, identity-only for it,
// never matched → lookup miss). `snapshot_value` copies only mutable structural arms + keeps
// identity/by-ref sub-values by handle, so all three insert AND resolve again.

/// A self-cyclic struct key inserts (cycle preserved by the visited-map, no depth-overflow fault)
/// and still resolves — regression for the `deep_clone` "maximum structural depth exceeded" fault.
#[test]
fn cyclic_struct_key_inserts_and_resolves() {
    let src = r#"
struct Node:
    val: int
    next: Node?
    fn hash(self) -> int: return self.val
fn main():
    a := Node(1, None)
    a.next = ?a
    s := {a}
    print(s.len())          # 1 — built, not a runtime fault
    m: Map[Node, str] = {}
    m[a] = "x"
    print(m.contains(a))         # true — same held key still resolves
main()
"#;
    assert_golden_out(src, "1\ntrue\n");
}

/// A struct key that transitively holds a live generator inserts + resolves — regression for the
/// `deep_clone` "a generator cannot be sent across tasks" fault on a purely sequential insert.
#[test]
fn generator_field_key_inserts_and_resolves() {
    let src = r#"
fn gen() -> Iterator[int]:
    yield 1
struct K:
    g: Iterator[int]
    id: int
    fn hash(self) -> int: return self.id
fn main():
    k := K(gen(), 7)
    m: Map[K, str] = {}
    m[k] = "one"
    print(m.contains(k))         # true — was a generator/airlock fault before
    print(m[k])             # one
    s := {k}
    print(s.len())          # 1
main()
"#;
    assert_golden_out(src, "true\none\n1\n");
}

/// A struct key with an identity-typed (closure) field resolves after insert — regression for the
/// `deep_clone` "key not found" miss (the cloned closure field got a fresh handle that
/// `values_equal`, identity-only for closures, could never match against the live key's field).
#[test]
fn closure_field_key_resolves_after_insert() {
    let src = r#"
struct H:
    id: int
    cb: fn() -> int
    fn hash(self) -> int: return self.id
fn main():
    h := H(1, fn() -> int: 1)
    m: Map[H, str] = {}
    m[h] = "x"
    print(m[h])             # x — was "key not found" before (fresh-handle closure field)
    print(m.contains(h))         # true
    s := {h}
    print(s.difference({h}).len())   # 0 — same held element compares equal
main()
"#;
    assert_golden_out(src, "x\ntrue\n0\n");
}

/// `map.update`/`map.merge` also snapshot the incoming key (spec: "cover EVERY insert entry point")
/// so the merged map does NOT alias the source map's stored key — mutating one via `keys()` cannot
/// corrupt the other.
#[test]
fn map_update_merge_snapshot_keys() {
    let src = r#"
struct K:
    x: int
    fn hash(self) -> int: return self.x
fn main():
    o: Map[K, str] = {}
    o[K(1)] = "a"
    # update
    m: Map[K, str] = {}
    m.update(o)
    k := o.keys()[0]
    k.x = 9                 # mutate the SOURCE's stored key
    print(m.contains(K(1)))      # true — m's key was snapshotted, not aliased
    print(m.keys()[0].x)    # 1
    # merge
    o2: Map[K, str] = {}
    o2[K(2)] = "b"
    mm := o2.merge(o2)
    k2 := o2.keys()[0]
    k2.x = 8
    print(mm.contains(K(2)))     # true
    print(mm.keys()[0].x)   # 2
main()
"#;
    assert_golden_out(src, "true\n1\ntrue\n2\n");
}

/// A very deep (acyclic) struct key must still insert AND resolve when looked up with the SAME held
/// object. Beyond `MAX_STRUCTURAL_DEPTH` the snapshot walk caps and would otherwise store a distinct
/// tail-aliased handle whose structural `values_equal` trips the same depth guard → a silent
/// `key not found` on the very object just inserted. The store path degrades an over-deep key to
/// by-reference (like a cyclic key) so the identity short-circuit resolves it. Same cap prevents the
/// unbounded `cyclic_walk` host-stack overflow (SIGABRT) on such keys. Both engines identical.
#[test]
fn deep_acyclic_struct_key_inserts_and_resolves() {
    let src = r#"
struct Node:
    val: int
    next: Node?
    fn hash(self) -> int: return self.val
fn main():
    n := Node(0, None)
    for i in 0..10050:
        n = Node(i, ?n)
    m: Map[Node, str] = {}
    m[n] = "deep"
    print(m[n])          # deep — held key still resolves past MAX_STRUCTURAL_DEPTH
    print(m.contains(n))      # true
    s := {n}
    print(s.len())       # 1
main()
"#;
    assert_golden_out(src, "deep\ntrue\n1\n");
}

// ===== widening follow-ups (adversarial review): alias sinks, variadic float param, `Any` elements

/// A mixed int/float literal in an `Any` element SLOT keeps the `int` an `int` — at every position it
/// reaches a literal (an annotated `let`, a call argument, the synthesized variadic pack), as in
/// CPython's `xs = [1, -2.5]`. Nothing widens an int into a float (D3).
#[test]
fn widen_any_collection_const_mix_agrees() {
    widen_three_engines("xs: List[Any] = [1, -2.5]\nprint(xs)\n", "[1, -2.5]\n");
    widen_three_engines(
        "fn show(...xs: Any):\n    print(xs)\nshow(1, 2.0 + 0.5)\n",
        "[1, 2.5]\n",
    );
    // GUARD (the other direction): a TYPED int element is never touched — neither side widens it, so
    // it stays an `Int` under the `Any` element type.
    widen_three_engines(
        "a := 1\nxs: List[Any] = [a, -2.5]\nprint(xs)\n",
        "[1, -2.5]\n",
    );
}

// ===== adversarial-review fixes: a generic TYPE PARAM shadows a module float alias =====

/// A generic TYPE PARAMETER shadows a module-level `type F = float` alias. The backend's alias table
/// is module-scoped; the checker resolves `F` in the DECLARATION's type-param scope (a `Ty::Param`).
/// So no coercion site (param prologue, `ret_is_float`, ctor field, `let` elem hint) may fire on a
/// value whose static type is the type VARIABLE — else a runtime `Float` sits under a static `int`
/// (silent precision loss, no integer-overflow check) and a `str` instantiation hard-faults on a
/// check-clean program.
#[test]
fn float_alias_shadowed_by_type_param_no_coerce() {
    widen_three_engines(
        "type F = float\nfn g[F](x: F) -> F:\n    return x\nr := g(5)\nprint(r)\nprint(r % 2)\nprint(g(\"hi\"))\n",
        "5\n1\nhi\n",
    );
    // generic struct: ctor float-field coercion + method return + a `List[F]` annotated `let`
    widen_three_engines(
        "type F = float\nstruct S[F]:\n    v: F\n\n    fn get(self) -> F:\n        return self.v\n\nprint(S[int](5).get())\nprint(S[str](\"hi\").get())\nfn h[F](x: F) -> List[F]:\n    xs: List[F] = [x]\n    return xs\nprint(h(5))\n",
        "5\nhi\n[5]\n",
    );
}

// ----- checker rejects (bound-method value, index/set_index V-coherence): the RUN-side guards -----
// Both fixes are checker-only, so the risk is OVER-rejection, not divergence. These pin the
// neighbours that must keep RUNNING correctly.

#[test]
fn coherent_index_set_str_parity() {
    // A COHERENT user IndexSet (`index -> str` / `set_index(_, val: str)`): read, write, compound,
    // negative index. The compound's LHS is typed from `index`'s RETURN (`x OP= v` ≡ `x = x OP v`).
    assert_golden_out(
        "\
struct S:
    d: List[str]
    fn index(self, k: int) -> str:
        return self.d[k]
    fn set_index(self, k: int, v: str):
        self.d[k] = v
s := S([\"a\", \"b\"])
print(s[0])
s[0] = \"x\"
print(s[0])
s[1] += \"y\"
print(s[1])
print(s[-1])
",
        "a\nx\nby\nby\n",
    );
}

#[test]
fn compound_index_assign_evaluates_index_once_parity() {
    // Python parity: `t[f()] += v` evaluates the index expression EXACTLY ONCE (the lowering dups
    // it). A side-effecting index must not double-fire — on a user IndexSet, a map, or a list.
    assert_golden_out(
        "\
struct S:
    d: List[int]
    fn index(self, k: int) -> int:
        return self.d[k]
    fn set_index(self, k: int, v: int):
        self.d[k] = v
fn bump(tag: str) -> int:
    print(\"idx {tag}\")
    return 0
s := S([1])
s[bump(\"struct\")] += 1
print(s[0])
m := {\"a\": 1}
m[\"a\"] += 1
print(m[\"a\"])
xs := [1, 2]
xs[bump(\"list\")] += 1
print(xs[0])
",
        "idx struct\n2\n2\nidx list\n2\n",
    );
}

#[test]
fn fn_typed_field_value_parity() {
    // THE closest neighbour to the bound-method reject: a genuinely fn-TYPED FIELD stays a
    // first-class value (`h.f(3)` AND `g := h.f; g(3)`), while a METHOD is call-only.
    assert_golden_out(
        "\
struct H:
    f: fn(int) -> int
    fn twice(self, x: int) -> int:
        return self.f(self.f(x))
fn dbl(x: int) -> int:
    return x * 2
h := H(dbl)
print(h.f(3))
g := h.f
print(g(3))
print(h.twice(3))
",
        "6\n6\n12\n",
    );
}

#[test]
fn asymmetric_index_set_plain_write_parity() {
    // NO-OVER-REJECTION: a plain `obj[k] = v` never READS through `index`, so an asymmetric pair is
    // sound and still runs (only the COMPOUND form is V-coherence-gated). A safe-read container
    // (`index -> int?`) and a widening writer (`index -> str` / `set_index(_, val: int)`).
    assert_golden_out(
        "\
struct T:
    d: Map[int, int]
    fn index(self, k: int) -> int?:
        return self.d.get(k)
    fn set_index(self, k: int, v: int):
        self.d[k] = v
struct W:
    d: List[str]
    fn index(self, k: int) -> str:
        return self.d[k]
    fn set_index(self, k: int, v: int):
        print(\"set {v}\")
t := T({})
t[0] = 9
match t[0]:
    ?v: print(v)
    None: print(\"none\")
w := W([\"a\"])
w[0] = 1
print(w[0])
",
        "9\nset 1\na\n",
    );
}

/// A range is a value (TICKET-253): the `for` iterable, the comprehension clause, a list slice, the
/// `match` range pattern and the `List`/`Set` constructors all RUN through the helpers that skip the
/// checker's rejection.
#[test]
fn range_positions_run() {
    let src = "
fn main():
    total := 0
    for i in 0..5:
        total = total + i
    print(total)
    print([i for i in 0..3])
    print([i for i in 0..10 if i % 2 == 0])
    print(List.from_iter(0..10)[Range(None, None, 2)])
    print(List.from_iter(0..10)[Range(1, 8, 3)])
    print(List.from_iter(0..5).reversed())
    match 3:
        1..5: print(\"in\")
        _: print(\"out\")
    print(List.from_iter(0..3))
    print(Set.from_iter(0..3).len())
main()
";
    assert_golden_out(
        src,
        "10\n[0, 1, 2]\n[0, 2, 4, 6, 8]\n[0, 2, 4, 6, 8]\n[1, 4, 7]\n[4, 3, 2, 1, 0]\nin\n[0, 1, 2]\n3\n",
    );
    assert_eq!(
        vm_outcome(src),
        Ok("10\n[0, 1, 2]\n[0, 2, 4, 6, 8]\n[0, 2, 4, 6, 8]\n[1, 4, 7]\n[4, 3, 2, 1, 0]\nin\n[0, 1, 2]\n3\n".to_string())
    );
}

/// `x := 0..3` is a value of the prelude struct `Range`; it binds, prints and compares.
#[test]
fn a_range_bound_to_a_name_is_a_value() {
    let out = vm_outcome(
        "fn main():\n    print(\"before\")\n    x := 0..3\n    print(x)\n    print(x == Range(0, 3))\nmain()\n",
    )
    .expect("a range is a value");
    assert_eq!(out, "before\n0..3\ntrue\n");
}

/// R1/B1 — BINARY sockets. `write_bytes` / `read_bytes` carry raw bytes over TCP byte-exactly (the
/// payload the str-only `read` can only ever `Err` on), and `read_bytes` at EOF returns the empty
/// `Ok(b"")` sentinel. M:N-only (std.net requires the OS-thread engine).
#[test]
fn socket_bytes_round_trip_binary_payload() {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut got = vec![0u8; 4];
        stream.read_exact(&mut got).unwrap();
        assert_eq!(got, vec![0u8, 255, 0x80, 10], "the client's binary write");
        stream.write_all(&[0u8, 255, 0x80, 10]).unwrap();
        // Close → the client's second read_bytes sees EOF.
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    payload := bytes.from_iter([0, 255, 128, 10])
    n := sock.write_bytes(payload)?
    print(\"WROTE:{{n}}\")
    got := sock.read_bytes(16, 5000)?
    print(\"LEN:{{got.len()}}\")
    for x in got:
        print(x)
    eof := sock.read_bytes(16, 5000)?
    print(\"EOF:{{eof.len()}}\")
    sock.close()
    return ?0

fn run() -> int!:
    parallel:
        spawn client()
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("bytes_round_trip", &src);
    server.join().unwrap();
    assert_eq!(
        out, "WROTE:4\nLEN:4\n0\n255\n128\n10\nEOF:0\ndone\n",
        "the binary payload must survive byte-exactly: {out:?}"
    );
}

/// R1/B1 — the ESCAPE HATCH: after the str-only `read` returns its sticky `Err("invalid utf-8 …")`,
/// the undecodable bytes stay CARRIED on the socket. `read_bytes` drains that carry first, so the
/// caller recovers the exact bytes instead of being forced to `close()`. This is what makes B1
/// honestly fixed rather than merely mitigated.
#[test]
fn socket_read_bytes_recovers_the_sticky_invalid_utf8_carry() {
    use std::io::Write;
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.write_all(b"hi\xFF\xFE").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(300));
    });

    let src = format!(
        "\
import std.net

fn client() -> int!:
    sock := net.connect(\"{addr}\")?
    match sock.read(64, 5000):
        ?s: print(\"GOT:[\" + s + \"]\")
        !e: print(\"ERR\")
    match sock.read(64, 5000):
        ?s: print(\"GOT2:[\" + s + \"]\")
        !e: print(\"ERR2\")
    b := sock.read_bytes(64, 5000)?
    print(\"BYTES:{{b.len()}}\")
    for x in b:
        print(x)
    sock.close()
    return ?0

fn run() -> int!:
    parallel:
        spawn client()
    return ?0

fn main():
    match run():
        ?_: print(\"done\")
        !e: print(\"net error: \" + e.message())

main()
"
    );
    let out = run_net_timeout_watchdog("bytes_carry", &src);
    server.join().unwrap();
    assert!(out.contains("GOT:[hi]"), "{out:?}");
    assert!(
        out.contains("ERR2"),
        "the invalid-utf-8 Err is sticky: {out:?}"
    );
    assert!(
        out.contains("BYTES:2\n255\n254\n"),
        "read_bytes must recover the carried undecodable bytes byte-exactly: {out:?}"
    );
}

// ===== cancellation points + the serial cancel drain (N6) =====
// Cancel is delivered at CHECKPOINTS (loop back-edges + blocking/park ops), so a STARTED task always
// runs its straight-line prologue and a REGISTERED `defer` always runs on cancel.
// Cross-task stdout ORDER stays nondeterministic (one print = one locked write,
// line-atomic); only the line SET / exit code / did-the-defer-run are asserted.

/// N6 — token-sequenced: `consumer` registers `defer cleanup(s)`, hands `boom` a token, then parks in
/// `ch.recv()`; `boom` faults. The defer is GUARANTEED registered before the fault, so it
/// must run (42). The since-removed serial engine used to print `0`: `run_scheduler`'s `run_child(i)?` propagated the
/// faulting child's error straight out and ABANDONED the parked consumer — never resumed, never
/// cancelled, never unwound.
#[test]
fn parity_defer_runs_on_parked_sibling_when_sibling_faults() {
    let src = "import Shared from std.concurrency\nfn cleanup(s: Shared[int]):\n    s.set(42)\n\
               fn consumer(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
               fn boom(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
               fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        go := Channel[int]()\n        parallel:\n            spawn consumer(ch, go, s)\n            spawn boom(go)\n        0\n    print(s.get())\nmain()\n";
    let serial = run_capture(src).expect("the fault is recovered, so the program completes");
    let mn = run_capture(src).expect("the fault is recovered, so the program completes");
    assert_eq!(serial, "42\n", "serial: the parked consumer's defer ran");
    assert_eq!(mn, "42\n", "M:N: the parked consumer's defer ran");
    assert_same_lines(&serial, &mn);
}
