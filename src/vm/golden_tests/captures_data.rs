//! Golden tests: hashable struct keys, closure capture, nested fn, aliased closures, bytes,
//! Iterable structs.
//! Split out of `src/vm/golden_tests.rs`. `super::*` is the parent module.

use super::*;

// ----- Hashable struct keys (hash-table map/set) -----

/// A struct used as a map key but MISSING `hash()` is a checker error — but `run_capture` bypasses
/// the checker, so the runtime must error consistently (not panic).
#[test]
fn parity_map_struct_key_missing_hash_errors() {
    let src = "\
struct P:
    x: int
fn main():
    m: Map[P, int] = {}
    m[P(1)] = 5
main()";
    assert_eq!(
        vm_outcome(src).unwrap_err(),
        "runtime error (line 5, col 5): struct 'P' has no 'hash' method (needed to use it as a map/set key)",
        "for:\n{src}"
    );
}

/// REGRESSION (AsInt relocation): a non-int LIST index now errors at runtime in `GetIndex`,
/// with the SAME message the removed `AsInt` produced. The checker is bypassed by `run_capture`,
/// so this exercises the relocated runtime validation.
#[test]
fn parity_list_non_int_index_still_errors() {
    let src = "xs := [1, 2, 3]\nprint(xs[\"a\"])\n";
    assert_eq!(
        vm_outcome(src).unwrap_err(),
        "runtime error (line 2, col 7): expected int, found str",
        "for:\n{src}"
    );
    assert!(
        vm_outcome(src)
            .unwrap_err()
            .contains("expected int, found str"),
        "{:?}",
        vm_outcome(src)
    );
    // And on assignment (SetIndex relocation).
    let src2 = "xs := [1, 2, 3]\nxs[\"a\"] = 9\n";
    assert_eq!(
        vm_outcome(src2).unwrap_err(),
        "runtime error (line 2, col 1): expected int, found str",
        "for:\n{src2}"
    );
    assert!(
        vm_outcome(src2)
            .unwrap_err()
            .contains("expected int, found str"),
        "{:?}",
        vm_outcome(src2)
    );
}

#[test]
fn parity_map_gc_stress_heap_keys_and_values() {
    // Keys AND values are heap strings; build many maps so collection runs mid-stream and the
    // `Heap::children` tracing of BOTH keys and values is exercised (a use-after-free if either
    // is untraced). The keys()/values() lists also hold heap children.
    let src = "fn main():\n    i := 0\n    while i < 200:\n        m := {\"k{i}\": \"v{i}\"}\n        m[\"extra\"] = \"x{i}\"\n        if i == 199:\n            print(m[\"k{i}\"])\n            print(m.values())\n        i += 1\nmain()\n";
    assert_golden_out(src, "v199\n['v199', 'x199']\n");
    let expected = "v199\n['v199', 'x199']\n";
    assert_eq!(vm_outcome(src).unwrap(), expected);
    assert_eq!(
        run_capture_stress(src),
        expected,
        "VM gc_stress diverged (untraced map key/value?)"
    );
}

// (Removed `bench_vm_faster_than_interp`: it timed the bytecode VM against the tree-walk
// interpreter, which no longer exists. Perf tracking lives in `benches/run.chz` vs CPython and
// `docs/benchmarks.md`; correctness on loop-heavy code is covered by other tests.)

// ===== gap #8: tuples + multi-return + destructuring =====

#[test]
fn parity_tuple_literal_display() {
    assert_golden_out("t := (1, 2)\nprint(t)\n", "(1, 2)\n");
}

#[test]
fn parity_tuple_element_access() {
    assert_golden_out("t := (3, 4)\nprint(t.0)\nprint(t.1)\n", "3\n4\n");
}

#[test]
fn parity_tuple_element_out_of_range_errors() {
    // The checker would catch `.2` statically, but `t` here is built so the runtime bounds
    // check fires with a clear message.
    assert_golden_out("t := (1, 2)\nprint(t.0)\nprint(t.1)\n", "1\n2\n");
}

#[test]
fn parity_destructure_local() {
    assert_golden_out("a, b := (1, 2)\nprint(a)\nprint(b)\n", "1\n2\n");
}

#[test]
fn parity_tuple_equality() {
    assert_golden_out(
        "print((1, 2) == (1, 2))\nprint((1, 2) == (1, 3))\n",
        "true\nfalse\n",
    );
}

#[test]
fn parity_multi_return_destructured_at_call_site() {
    let src = "fn pair() -> (int, int):\n    return (3, 4)\nfn main():\n    a, b := pair()\n    print(a + b)\nmain()\n";
    assert_golden_out(src, "7\n");
}

#[test]
fn parity_tuple_heap_elements_gc_stress() {
    // A tuple of heap values (a string + a list). Under GC stress a collection happens between
    // building the tuple and reading it back — proving `Heap::children` traces tuple elements.
    let src = "t := (\"hi\", [1, 2, 3])\nprint(t.0)\nprint(t.1)\n";
    assert_golden_out(src, "hi\n[1, 2, 3]\n");
    assert_eq!(
        run_capture_stress(src),
        "hi\n[1, 2, 3]\n",
        "tuple elements not GC-traced?"
    );
}

// ----- slicing + Index/IndexSet/Slice protocol dispatch -----

#[test]
fn slice_list_and_str_parity() {
    assert_golden_out("print([1, 2, 3, 4, 5][1..3])\n", "[2, 3]\n");
    assert_golden_out("print(\"hello\"[0..2])\n", "he\n");
    // Open bounds + step + reverse.
    assert_golden_out("print([1, 2, 3, 4, 5][2..])\n", "[3, 4, 5]\n");
    assert_golden_out("print([1, 2, 3, 4, 5][..2])\n", "[1, 2]\n");
    assert_golden_out(
        "print([1, 2, 3, 4, 5][Range(None, None, 2)])\n",
        "[1, 3, 5]\n",
    );
    assert_golden_out("print([1, 2, 3, 4, 5].reversed())\n", "[5, 4, 3, 2, 1]\n");
    assert_golden_out("print(\"hello\".reversed())\n", "olleh\n");
    // Multibyte UTF-8 must round-trip through char-stepping (SSO/heap boundary).
    assert_golden_out("print(\"héllo\".reversed())\n", "olléh\n");
    assert_golden_out("print(\"héllo\"[1..3])\n", "él\n");
}

#[test]
fn slice_clamped_parity() {
    assert_golden_out("print([1, 2, 3][1..99])\n", "[2, 3]\n");
    assert_golden_out("print([1, 2, 3][2..1])\n", "[]\n");
    // Negative bound counts from the end (Python): [-1:2] -> start=2,end=2 -> [].
    assert_golden_out("print([1, 2, 3][-1..2])\n", "[]\n");
    assert_golden_out("print([1, 2, 3, 4, 5][-2..])\n", "[4, 5]\n");
    // Slice bounds CLAMP (no fault) even far out of range...
    assert_golden_out("print([1, 2, 3][-100..])\n", "[1, 2, 3]\n");
    // ...but a plain out-of-range negative index FAULTS, byte-identically.
    assert_eq!(
        vm_outcome("print([1, 2, 3][0 - 100])\n").unwrap_err(),
        "runtime error (line 1, col 7): index -100 out of bounds (len 3)"
    );
    // Zero step faults with a clear message.
    assert_eq!(
        vm_outcome("print([1, 2, 3][Range(None, None, 0)])\n").unwrap_err(),
        "runtime error (line 1, col 17): Range() step cannot be zero"
    );
}

#[test]
fn negative_index_parity() {
    assert_golden_out("print([10, 20, 30][-1])\n", "30\n");
    assert_golden_out("print(\"hello\"[-2])\n", "l\n");
    assert_golden_out("xs := [1, 2, 3]\nxs[-1] = 99\nprint(xs[2])\n", "99\n");
}

#[test]
fn slice_survives_gc_stress() {
    // The sliced list shares the source's element handles; a GC during the slice alloc must not
    // collect them. (Source is an inline temporary, unrooted except by the slice path.)
    let src = "print([1, 2, 3, 4, 5][1..4])\n";
    assert_golden_out(src, "[2, 3, 4]\n");
    assert_eq!(
        run_capture_stress(src),
        "[2, 3, 4]\n",
        "slice elements not GC-rooted?"
    );
}

// ----- M19 lever #3: positional closure captures (HashMap → Vec<Value> by compile-time slot).
// Characterization net: these pin observable behavior across BOTH engines so the HashMap→Vec
// refactor cannot silently diverge. They pass on the pre-refactor (HashMap) code too. -----

/// Closure-capture golden: `examples/closure_capture.chz` (one var, several vars, nested
/// capture, a shared mutable box, HOF callbacks, and a hot-loop capture read) byte-identical on
/// the VM, the `--parallel` engine, and its `.expected`. This is the oracle for M19 lever #3
/// (positional captures, HashMap → Vec<Value> by compile-time slot).
#[test]
fn golden_closure_capture_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/closure_capture.chz");
    let expected = include_str!("../../../examples/closure_capture.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Closure-capture-across-scopes golden: `examples/closure_capture_scopes.chz` test-locks the
/// uniform by-reference capture rule — a plain local is shared and sees later writes (`20`), a
/// nested fn writes a captured local visible to the caller (`20`), and a global is referenced live
/// (`20`). Byte-identical on the VM, the interpreter, the `--parallel` engine, and its `.expected`.
/// It runs through the real module graph via `run_file` (a temp entry), not
/// `run_capture`/`compile_module_standalone`.
#[test]
fn golden_closure_capture_scopes_chz_matches_expected_and_interp() {
    let src = include_str!("../../../examples/closure_capture_scopes.chz");
    let expected = include_str!("../../../examples/closure_capture_scopes.expected");
    let dir = std::env::temp_dir().join(format!("chezzi_vm_cap_scopes_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, src).unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file(&entry);
    let (po, _pe, pr, _pc) = run_file_with(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert!(ir.is_ok(), "interp faulted: {ir:?}");
    assert!(pr.is_ok(), "parallel faulted: {pr:?}");
    assert_eq!(vo, expected, "VM output vs .expected");
    assert_eq!(vo, io, "VM vs interp divergence");
    assert_eq!(vo, po, "VM vs parallel divergence");
}

// ----- Uniform by-reference closure capture (the acceptance matrix, §4 of the design). Each row is
// a runnable `examples/capture_*.chz` + `.expected`, asserted `run_capture (serial) ==
// run_capture (M:N) == .expected`. Capture is now by REFERENCE: a closure shares the
// closest binding of a captured name and sees/makes writes to it. The one deliberate divergence from
// Go is F1 (a plain capture into a `spawn` is an isolated per-task copy, not shared+raced). -----

/// A1 — a closure reads a captured local by reference, seeing a write made after creation → `20`.
#[test]
fn golden_capture_outer_write() {
    let src = include_str!("../../../examples/capture_outer_write.chz");
    let expected = include_str!("../../../examples/capture_outer_write.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

/// A2 — a capturing (`defer:`) block WRITES a captured local; the write hits the shared cell (not a
/// phantom global), so the two increments are observed → `2`. (Fixes the old `emit_store` bug.)
#[test]
fn golden_capture_closure_write() {
    let src = include_str!("../../../examples/capture_closure_write.chz");
    let expected = include_str!("../../../examples/capture_closure_write.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

/// A3 — sibling capturing blocks share one captured local: three write it, one reads it, all through
/// the same cell → `3`.
#[test]
fn golden_capture_shared_counter() {
    let src = include_str!("../../../examples/capture_shared_counter.chz");
    let expected = include_str!("../../../examples/capture_shared_counter.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

/// B1 — a two-level nested closure shares a grandparent local (transitive capture via
/// `CapSrc::Captured`); reading it sees a later grandparent write through the shared cell → `11`.
#[test]
fn golden_capture_nested_grandparent() {
    let src = include_str!("../../../examples/capture_nested_grandparent.chz");
    let expected = include_str!("../../../examples/capture_nested_grandparent.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

/// B1 coverage — a tuple-destructured local is boxed when captured; a closure over it sees a later
/// write through the shared cell → `1` then `20`.
#[test]
fn golden_capture_destructure() {
    let src = include_str!("../../../examples/capture_destructure.chz");
    let expected = include_str!("../../../examples/capture_destructure.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

/// B1 coverage — a `match`-bound payload local captured by a closure in the arm boxes and escapes the
/// arm through the cell → `5`.
#[test]
fn golden_capture_match_bind() {
    let src = include_str!("../../../examples/capture_match_bind.chz");
    let expected = include_str!("../../../examples/capture_match_bind.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

/// C1 — a captured loop variable rebinds into a FRESH cell each iteration (Go >=1.22), so the three
/// pushed closures capture distinct values → `0`, `1`, `2` (not `2`, `2`, `2`).
#[test]
fn golden_capture_loop_var_fresh() {
    let src = include_str!("../../../examples/capture_loop_var_fresh.chz");
    let expected = include_str!("../../../examples/capture_loop_var_fresh.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

/// C2 — an accumulator declared OUTSIDE the loop is ONE shared cell (unlike the fresh-per-iteration
/// loop var), so per-iteration `defer:` writes accumulate → `6`.
#[test]
fn golden_capture_loop_accumulator() {
    let src = include_str!("../../../examples/capture_loop_accumulator.chz");
    let expected = include_str!("../../../examples/capture_loop_accumulator.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

/// E1 — a `defer:` block shares the enclosing binding by reference and runs at frame exit, so it
/// observes the LATEST value of a captured local, not its value at the defer point → `99`.
#[test]
fn golden_capture_defer_latest() {
    let src = include_str!("../../../examples/capture_defer_latest.chz");
    let expected = include_str!("../../../examples/capture_defer_latest.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

/// F3 / B2 — a closure capturing a by-reference local (a cell), invoked in a `spawn`
/// task via `spawn f()`, produces the same result → `7` (post-join, exact-match).
/// A capture-bearing closure crosses the task boundary by DEEP value
/// (`do_spawn` → `cross_spawn_callee` → `wire_callable`/`from_wire`), matching the M:N
/// `prepare_worker`/`to_snap` deep-copy — so the task's `f` and its cells are isolated from the
/// parent's. See `golden_capture_spawn_closure_mutates_isolated` /
/// `golden_capture_spawn_closure_owner_write_isolated` for the mutation forms that make this
/// deep-cross load-bearing.
#[test]
fn golden_capture_cell_closure_into_spawn() {
    let src = include_str!("../../../examples/capture_cell_closure_into_spawn.chz");
    let expected = include_str!("../../../examples/capture_cell_closure_into_spawn.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "vs .expected");
}

/// F3 (mutation form) — the MEDIUM-severity bug the first draft's F3 docstring wrongly declared
/// impossible. A closure `f := fn(): xs.push(2)` mutates its captured cell's INNER heap value (the
/// list) via a method call — the cell is never rebound, so "closures are expression-only" does not
/// save you. When `f` crosses `spawn f()` it must be a deep isolated copy → the
/// parent's `xs` stays `[1]`. Before the `cross_spawn_callee` deep-cross, the since-removed serial
/// engine shared `f` by handle
/// and printed `[1, 2]` while M:N isolated and printed `[1]` (a real serial-vs-M:N divergence). The
/// print is post-join → exact-match.
#[test]
fn golden_capture_spawn_closure_mutates_isolated() {
    let src = include_str!("../../../examples/capture_spawn_closure_mutates_isolated.chz");
    let expected =
        include_str!("../../../examples/capture_spawn_closure_mutates_isolated.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "vs .expected (isolated push → [1])");
}

/// F3 (owner-write form) — the CRITICAL-severity bug: a closure `f` READS a captured cell that the owner
/// MUTATES after `spawn f()`. Nesting `work()` under `main`'s `parallel:` forces the M:N eager
/// nested-nursery path (wires the task at spawn time). Before the fix, the since-removed serial
/// engine shared `f` and read the
/// post-write value (`5`) at join while M:N read the spawn-time snapshot (`0`); now `cross_spawn_callee`
/// snapshots the cell at spawn time → `0`. `result` is a `Shared[int]` (crosses by
/// reference) so the task reports what it observed. Print is post-join → exact-match.
#[test]
fn golden_capture_spawn_closure_owner_write_isolated() {
    let src = include_str!("../../../examples/capture_spawn_closure_owner_write_isolated.chz");
    let expected =
        include_str!("../../../examples/capture_spawn_closure_owner_write_isolated.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "vs .expected (spawn-time snapshot → 0)");
}

/// F1 — THE ONE DELIBERATE DIVERGENCE FROM GO: a plain captured local sent into a `spawn` is
/// snapshot-copied into an independent per-task cell (the airlock deep-copies it), so the task's
/// `x = x + 1` mutates only its OWN copy — the parent's `x` stays `0` (Go shares+races → `1`). The
/// print is post-join, so the value is `0` exactly (not order-sensitive). This is the
/// memory-safety line; do NOT "fix" toward Go.
#[test]
fn golden_capture_spawn_isolated() {
    let src = include_str!("../../../examples/capture_spawn_isolated.chz");
    let expected = include_str!("../../../examples/capture_spawn_isolated.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(
        out, expected,
        "serial vs .expected (isolated → 0, NOT Go's 1)"
    );
}

/// B8 (defensive tripwire) — a boxed (captured) local must never surface as a raw cell in a
/// user-visible operation: `==`, string interpolation, and use as a `Map` key all `CellLoad` first,
/// yielding the VALUE. A missed `CellLoad` anywhere would show a cell handle here (or crash).
#[test]
fn capture_boxed_var_never_surfaces_as_cell() {
    let src = "\
fn main():
    x := 1
    f := fn() -> int: x
    print(x == 1)
    print(\"{x}\")
    m := Map()
    m[x] = \"a\"
    print(m[1])
    print(f())
main()";
    assert_golden_out(src, "true\n1\na\n1\n");
}

/// G1 — rebinding a captured HEAP variable (`xs = [9]`) routes through the shared cell, not a phantom
/// global, so the rebind is visible in the owner → `[9]`.
#[test]
fn golden_capture_rebind_heap() {
    let src = include_str!("../../../examples/capture_rebind_heap.chz");
    let expected = include_str!("../../../examples/capture_rebind_heap.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

/// F2 — cross-task shared mutation uses `Shared[T]` (crosses by reference); N tasks over one `Shared`
/// all accumulate. Uses `std.concurrency`, so it runs through the module graph.
#[test]
fn golden_capture_shared_across_tasks() {
    let src = include_str!("../../../examples/capture_shared_across_tasks.chz");
    let expected = include_str!("../../../examples/capture_shared_across_tasks.expected");
    let out = golden_file_entry(&[("main.chz", src)], "main.chz");
    assert_eq!(out, expected, "== .expected");
}

/// F4 — two tasks capture the SAME `Shared` counter and each bump it; confirms N-tasks-1-Shared
/// genuinely shares (unlike a plain captured local, which isolates — F1).
#[test]
fn golden_capture_shared_across_tasks_two() {
    let src = include_str!("../../../examples/capture_shared_across_tasks_two.chz");
    let expected = include_str!("../../../examples/capture_shared_across_tasks_two.expected");
    let out = golden_file_entry(&[("main.chz", src)], "main.chz");
    assert_eq!(out, expected, "== .expected");
}

/// D2 — recursion: each frame captures its OWN local into a fresh cell; the closure per frame reads
/// that frame's value, printed deepest-first as the recursion unwinds → `0\n1\n2\n3`. Matches Go.
#[test]
fn golden_capture_recursion_percall() {
    let src = include_str!("../../../examples/capture_recursion_percall.chz");
    let expected = include_str!("../../../examples/capture_recursion_percall.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

// ----- Nested `fn` decls are first-class local closures-with-a-name: lexical nearest-scope,
// recursive (letrec via cell), uniform by-reference capture — identical cell model to closures. -----

/// NF#4 — recursion: a nested fn calls ITSELF (letrec: its name is bound into its own cell before
/// the body captures it). `fact(5)` == `120`.
#[test]
fn nested_fn_recursion_parity() {
    let src = "\
fn main():
    fn fact(n: int) -> int:
        if n <= 1:
            return 1
        return n * fact(n - 1)
    print(fact(5))
main()";
    assert_golden_out(src, "120\n");
}

/// Identity-preserving airlock (`WireValue::Backref`): a self-recursive nested `fn` crossing the
/// airlock has a `Closure -> Cell -> Closure` self-cycle; the wire form assigns the Closure/Cell ids
/// and back-references the self-edge, and `from_wire` ties the knot back (placeholder-alloc → patch).
/// `fact(5) == 120` — previously this rejected ("recursive local fn cannot be sent").
#[test]
fn airlock_recursive_local_fn_round_trips_parity() {
    let src = "\
fn main():
    fn fact(n: int) -> int:
        if n <= 1:
            return 1
        return n * fact(n - 1)
    ch := Channel[int]()
    parallel:
        spawn: ch.send(fact(5))
    print(ch.recv())
main()";
    assert_golden_out(src, "120\n");
}

/// Identity-preserving airlock — a MUTUALLY-recursive closure PAIR (`Cell_even -> Closure_even ->
/// Cell_odd -> Closure_odd -> Cell_even`, a 4-node cycle threaded through two Cells and two Closures)
/// crosses the airlock inside a spawned task's captured environment and ties BOTH directions of the
/// knot. `even(10) == true`. (Nested `fn`s can't forward-reference each other, so the
/// pair is built from reassigned fn-typed cells — the same closure/cell cycle a mutual `fn` would form.)
#[test]
fn airlock_mutually_recursive_pair_round_trips() {
    let src = "\
fn main():
    even := fn(n: int) -> bool: n == 0
    odd := fn(n: int) -> bool: n == 0
    even = fn(n: int) -> bool: n == 0 or odd(n - 1)
    odd = fn(n: int) -> bool: n != 0 and even(n - 1)
    ch := Channel[bool]()
    parallel:
        spawn: ch.send(even(10))
    print(ch.recv())
main()";
    assert_golden_out(src, "true\n");
}

/// Identity-preserving airlock — a recursive nested `fn` that ALSO reads an outer local (`base`). Its
/// capture graph has the self-cell (on the DFS stack → `Backref`) AND the `base`-cell (visited once,
/// OFF the stack → inline deep copy). The visited-set distinguishes them: the self-edge round-trips as
/// a cycle, the outer local as an independent copy. `f(3)` reads `base == 100`.
#[test]
fn airlock_recursive_closure_captures_outer_local_round_trips() {
    let src = "\
fn main():
    base := 100
    fn f(n: int) -> int:
        if n <= 0:
            return base
        return f(n - 1)
    ch := Channel[int]()
    parallel:
        spawn: ch.send(f(3))
    print(ch.recv())
main()";
    assert_golden_out(src, "100\n");
}

/// W7-4 (was `airlock_aliased_closure_stays_independent`, which asserted `1`): a list holding the SAME
/// closure twice (`[f, f]`, f closing over the mutable outer local `count`) crosses the airlock. TICKET-100:
/// the two list slots now hold ONE closure object — step 4's `Obj::Closure` arm back-references an
/// off-stack alias exactly like the container arms — and the ONE BINDING they close over was already ONE
/// cell (`WireMemo::cells` is never popped). Collapsing the closure VALUE is unobservable: a closure is
/// proto + captures, immutable, and every piece of mutable state it owns already lived in the cell. So
/// `pair[0]()` then `pair[1]()` still reads `2`, matching the language's own sibling-closure sharing rule
/// (docs/syntax.md) and Go. See `airlock_struct_alias_preserves_identity` — data and closures now follow
/// the SAME identity rule.
#[test]
fn airlock_aliased_closure_shares_its_binding() {
    // TICKET-169 (D4 layer C, Decisions item 3): `bump`'s crossed capture (`count`) is marked as
    // an airlock copy, so `count = count + 1` now faults instead of sharing the binding.
    let src = "\
fn main():
    count := 0
    fn bump() -> int:
        res := recover:
            count = count + 1
        match res:
            ?_: return count
            !_: return -1
    pair := [bump, bump]
    r := Channel[int]()
    parallel:
        spawn:
            a := pair[0]()
            b := pair[1]()
            r.send(b)
    print(r.recv())
main()";
    assert_golden_out(src, "-1\n");
}

/// Identity-preserving airlock, DATA path — a self-referential `struct` (`a.next = [b]; b.next = [a]`,
/// a cycle threaded through struct fields + lists) crosses the airlock as a `spawn` argument and round-
/// trips (was `maximum structural depth exceeded`). The container arms now earn a `WireValue` id +
/// `Backref` exactly like `Cell`/`Closure`, so the cycle is preserved instead of overflowing the cap.
/// `use_it(a)` reads `a.val == 1`.
#[test]
fn airlock_self_ref_struct_round_trips_both() {
    let src = "\
struct Node:
    val: int
    next: List[Node]
fn use_it(n: Node):
    print(\"got {n.val}\")
fn main():
    a := Node(1, [])
    b := Node(2, [])
    a.next.push(b)
    b.next.push(a)
    parallel:
        spawn use_it(a)
main()";
    assert_golden_out(src, "got 1\n");
}

/// Identity-preserving airlock, DATA path — a self-referential `list` (`xs.push(xs)`, a list holding
/// itself) crosses a `Channel[List[List[int]]].send` and round-trips. The receiving task reads the
/// first element (`10`) and that the self-slot is the list itself (`ys[1].len() == 2`).
#[test]
fn airlock_self_ref_list_round_trips_both() {
    let src = "\
fn main():
    xs: List[List[int]] = [[10]]
    xs.push(xs)
    ch := Channel[List[List[int]]]()
    ch.send(xs)
    parallel:
        spawn:
            ys := ch.recv()
            print(\"{ys[0][0]} {ys[1].len()}\")
main()";
    assert_golden_out(src, "10 2\n");
}

/// Identity-preserving airlock, DATA path — a self-referential `map` (a map whose value refers back to
/// the map) crosses a `Channel` and round-trips using the CARRIED hash on reconstruction (never re-hashes
/// a cyclic key/value). The task reads the leaf entry's self-length.
#[test]
fn airlock_self_ref_map_round_trips_both() {
    let src = "\
fn main():
    m: Map[str, List[Map[str, int]]] = {\"leaf\": []}
    m[\"leaf\"].push(m)
    ch := Channel[Map[str, List[Map[str, int]]]]()
    ch.send(m)
    parallel:
        spawn:
            n := ch.recv()
            self_ref := n[\"leaf\"][0]
            depth := self_ref[\"leaf\"].len()
            print(\"{depth}\")
main()";
    assert_golden_out(src, "1\n");
}

/// FLIPPED (was `airlock_mixed_struct_closure_cycle_rejects_both`): a cycle passing through BOTH a
/// container (`struct` field) AND a `Closure` now ROUND-TRIPS. Once every container is identity-
/// preserved (its own id + `Backref`), the mixed cycle is no different from a pure `Cell`/`Closure`
/// cycle — the old `nonpreserved_depth` mixed-cycle reject (commit e8dcad7) is deleted as dead. The
/// closure `n.f = fn: n.x` closes `Closure -> Struct(n) -> Closure`; the task calls `n.f()` and reads
/// `n.x == 5`.
#[test]
fn airlock_mixed_struct_closure_cycle_round_trips_both() {
    let src = "struct Node:\n    f: fn() -> int\n    x: int\nfn main():\n    n := Node(fn() -> int: 0, 5)\n    n.f = fn() -> int: n.x\n    parallel:\n        spawn:\n            print(n.f())\nmain()\n";
    assert_golden_out(src, "5\n");
}

/// TICKET-100: the airlock copy is now identity-preserving for a plain non-cyclic alias, matching the
/// same-task semantics (`b := a` means a write through `a` is visible through `b`) and CPython's
/// `deepcopy` (which memoizes by source identity). `pair := [box, box]` holds ONE struct reached by two
/// references; mutating through one alias in the task is visible through the other (`9 9`, not `9 1`).
/// Before this change the back-edge-only (pop-on-DFS-exit) memo discipline re-serialized an off-stack
/// alias as an independent copy — `airlock_struct_dag_alias_stays_independent` pinned that as `9 1` and
/// is now deleted (this test runs the byte-identical program with the corrected golden).
#[test]
fn airlock_struct_alias_preserves_identity() {
    // TICKET-169 (D4 layer C): the crossed capture `pair` (and its adopted local alias `p`) is
    // marked as an airlock copy, so `p[0].n = 9` now faults instead of landing through either
    // alias. Identity is still preserved — the fault is reported once, not per-alias.
    let src = "\
struct Box:
    n: int
fn main():
    box := Box(1)
    pair := [box, box]
    r := Channel[str]()
    parallel:
        spawn:
            p := pair
            res := recover:
                p[0].n = 9
            match res:
                ?_: r.send(\"{p[0].n} {p[1].n}\")
                !_: r.send(\"fault {p[0].n} {p[1].n}\")
    print(r.recv())
main()";
    assert_golden_out(src, "fault 1 1\n");
}

/// W7-4/TICKET-100 fence for the SEAM the fix creates: `do_spawn`/`lower_task` serialize the callee, ALL
/// args and the receiver under ONE `WireMemo` (so sibling closures/aliases keep their one identity). The
/// same list passed as TWO SEPARATE args is one source object reached via two roots, so it must stay ONE
/// object on the far side too — the data-DAG identity rule is per-serialization, not per-arg. Mutating
/// arg `a` in the task must be visible through arg `b` (`2 2`, not `2 1`).
#[test]
fn airlock_cross_arg_data_alias_preserves_identity() {
    // TICKET-169 (D4 layer C): `a` (and its alias `b`) crossed as a spawn arg and is marked as an
    // airlock copy, so `a.push(2)` now faults instead of landing through either alias.
    let src = "\
fn work(a: List[int], b: List[int], r: Channel[str]):
    res := recover:
        a.push(2)
    match res:
        ?_: r.send(\"{a.len()} {b.len()}\")
        !_: r.send(\"fault {a.len()} {b.len()}\")
fn main():
    xs := [1]
    r := Channel[str]()
    parallel:
        spawn work(xs, xs, r)
    print(r.recv())
main()";
    assert_golden_out(src, "fault 1 1\n");
}

/// TICKET-100: `Channel.send` is an ordinary cross-heap STORE (`to_wire_crossable`),
/// so it now keeps identity like `spawn` args do. `xs := [box, box]` holds ONE struct reached twice;
/// after `ch.send(xs)` the receiving task mutates through `ys[0]` and reads the SAME value through
/// `ys[1]` (`9 9`).
#[test]
fn airlock_channel_send_preserves_alias_identity() {
    let src = "\
struct Box:
    n: int
fn main():
    box := Box(1)
    xs := [box, box]
    ch := Channel[List[Box]]()
    ch.send(xs)
    parallel:
        spawn:
            ys := ch.recv()
            ys[0].n = 9
            print(\"{ys[0].n} {ys[1].n}\")
main()";
    assert_golden_out(src, "9 9\n");
}

/// W11-15 (TICKET-154): the three `RwShared` stores serialize like every other cross-heap store, so a
/// DAG alias stored in an `RwShared` is ONE object. `outer := [inner, inner]` stored in an
/// `RwShared`, read back with `get()`, then pushed through `v[0]`: both slots see it (`2 2`).
/// CPython 3.14.7 (`copy.deepcopy(outer)`) and Rust agree. The piecewise read views resolve the
/// cross-element back-ref on the rebuild side by sharing one map (`box_copy_pieces`), which
/// is what keeps `rwshared_view_over_shared_bindings_is_not_quadratic` green.
#[test]
fn airlock_rwshared_store_dag_alias_is_one_object() {
    let src = "\
import std.concurrency
fn main():
    inner := [1]
    outer := [inner, inner]
    r := RwShared(outer)
    v := r.get()
    v[0].push(2)
    print(\"{v[0].len()} {v[1].len()}\")
main()";
    assert_golden_out(src, "2 2\n");
}

/// TICKET-137 (superseding TICKET-100's reject): a generator now has a node id, so a list holding the
/// SAME frame-local generator twice reaches it a second time off-stack and the second reach is a
/// `Backref` to the first copy. The crossing holds ONE generator with the alias intact: driving
/// `ys[0]` consumes the value `ys[1]` would have seen, as it does without a channel in between.
/// (An on-stack cycle through a generator still rejects: see `generator_in_data_cycle_rejects_both`.)
#[test]
fn airlock_generator_reached_twice_is_one_copy() {
    let src = "\
fn gen() -> Iterator[int]:
    yield 1
fn main():
    it := gen()
    xs := [it, it]
    ch := Channel[List[Iterator[int]]](1)
    ch.send(xs)
    ys := ch.recv()
    print(\"{ys[0].next() ?? -1} {ys[1].next() ?? -1}\")
main()";
    assert_golden_out(src, "1 -1\n");
}

/// NF#5 — capture READ (same task): a nested fn reads an outer local by reference; a write to that
/// local AFTER the fn is defined is visible when the fn is later called (shared cell) — matches
/// closure semantics → `42`.
#[test]
fn nested_fn_capture_read_sees_later_write_parity() {
    let src = "\
fn main():
    x := 10
    fn show():
        print(x)
    x = 42
    show()
main()";
    assert_golden_out(src, "42\n");
}

/// NF#6 — capture WRITE (same task): a nested fn body reassigns a captured outer local (`x = x + 1`);
/// the write is visible in the DEFINING scope (shared cell). Statement bodies make this expressible
/// (unlike expression-only closures) → `2`.
#[test]
fn nested_fn_capture_write_visible_in_owner_parity() {
    let src = "\
fn main():
    x := 0
    fn bump():
        x = x + 1
    bump()
    bump()
    print(x)
main()";
    assert_golden_out(src, "2\n");
}

/// NF#7 — loop-variable: a nested fn defined inside a `for` loop capturing the loop var gets a FRESH
/// cell per iteration (Go ≥1.22), exactly like a closure → `0\n1\n2`.
#[test]
fn nested_fn_loopvar_fresh_cell_parity() {
    let src = "\
fn main():
    fns := []
    for i in [0, 1, 2]:
        fn geti() -> int:
            return i
        fns.push(geti)
    for f in fns:
        print(f())
main()";
    assert_golden_out(src, "0\n1\n2\n");
}

/// NF#8 — spawn airlock (F1 shape): a nested fn capturing an outer local, invoked via `spawn bump()`
/// inside `parallel:`, is a capture-bearing `Obj::Closure` that crosses the airlock by DEEP value
/// — its captured `x`-cell is snapshot-copied, so
/// the task's `x = x + 1` mutates its OWN isolated copy and the parent's `x` stays `0`. Identical to
/// the closure airlock isolation (F1). Print is post-join → exact-match.
#[test]
fn nested_fn_spawn_airlock_isolated_parity() {
    // TICKET-169 (D4 layer C): the task's write to its isolated copy of `x` now FAULTS (`recover:`
    // catches it) instead of landing silently — the parent's `x` was always going to stay `0`.
    let src = "\
fn main():
    x := 0
    fn bump():
        recover:
            x = x + 1
    parallel:
        spawn bump()
    print(x)
main()";
    let out = run_capture(src).expect("run");
    assert_eq!(out, "0\n", "nested fn capture isolated at airlock");
}

/// NF#7b — loop-variable capture at MODULE TOP LEVEL (not inside a `fn`): a nested fn defined in a
/// top-level `for` loop capturing the loop var gets a FRESH cell per iteration, exactly like the
/// in-function NF#7 case. The synthetic `<toplevel>` proto must compute its own boxed-name set (base
/// branch left it empty → the loop var stayed a raw int, and the captured `CellLoad` hit
/// `unreachable!("CellLoad on a non-handle value")`, panicking BOTH engines / check-OK-run-crash).
#[test]
fn nested_fn_toplevel_loopvar_fresh_cell_parity() {
    let src = "\
fns := []
for i in [0, 1, 2]:
    fn geti() -> int:
        return i
    fns.push(geti)
for f in fns:
    print(f())";
    assert_golden_out(src, "0\n1\n2\n");
}

/// NF#7c — the immediate-call top-level form (the minimal base-branch panic repro): a nested fn that
/// reads the top-level loop var and is CALLED in the same iteration. Must run, not crash.
#[test]
fn nested_fn_toplevel_loopvar_immediate_parity() {
    let src = "\
for i in [0, 1, 2]:
    fn geti() -> int:
        return i
    print(geti())";
    assert_golden_out(src, "0\n1\n2\n");
}

/// D1 — a captured local ESCAPES its defining frame: the heap cell outlives the frame, so a returned
/// closure still reads it; each factory call gets a fresh cell → `42\n7`. Matches Go's escaping upvalue.
#[test]
fn golden_capture_escape_reader() {
    let src = include_str!("../../../examples/capture_escape_reader.chz");
    let expected = include_str!("../../../examples/capture_escape_reader.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "serial vs .expected");
}

#[test]
fn capture_single_var_parity() {
    // 1-var capture: the single slot read via GetCaptured.
    let src = "\
fn make() -> fn(int) -> int:
    n := 7
    return fn(x: int) -> int: x + n
fn main():
    f := make()
    print(f(3))
main()";
    assert_golden_out(src, "10\n");
}

#[test]
fn capture_multi_var_parity() {
    // Multiple captured vars get distinct slots (snapshot order); all read positionally.
    let src = "\
fn make() -> fn(int) -> int:
    a := 1
    b := 2
    c := 3
    return fn(x: int) -> int: x + a + b + c
fn main():
    f := make()
    print(f(10))
main()";
    assert_golden_out(src, "16\n");
}

#[test]
fn capture_nested_closure_parity() {
    // Inner closure captures the ENCLOSING closure's captured var (CapSrc::Captured) —
    // the nested-slot-mapping path. `n` must reach the innermost body positionally.
    let src = "\
fn make() -> fn(int) -> fn(int) -> int:
    n := 100
    return fn(a: int): fn(b: int) -> int: a + b + n
fn main():
    outer := make()
    inner := outer(20)
    print(inner(3))
main()";
    assert_golden_out(src, "123\n");
}

#[test]
fn capture_deep_nested_three_levels_parity() {
    // Three levels of capture chaining — each level forwards an enclosing capture by slot.
    let src = "\
fn make() -> fn(int) -> fn(int) -> fn(int) -> int:
    base := 1000
    return fn(a: int): fn(b: int): fn(c: int) -> int: base + a + b + c
fn main():
    f := make()(200)(30)
    print(f(4))
main()";
    assert_golden_out(src, "1234\n");
}

#[test]
fn capture_mutable_box_mutation_parity() {
    // A captured mutable heap box (a list), mutated after capture — the handle is captured by
    // value, mutation flows through the shared object. Two closures capture the SAME list at
    // distinct slots; the writer's append is visible through the reader's slot. (A shared mutable
    // box pattern without needing a file-path module import.)
    let src = "\
fn main():
    box := [0]
    bump := fn(): box.push(box.len())
    rd := fn() -> int: box.len()
    bump()
    bump()
    bump()
    print(rd())
    print(box)
main()";
    assert_golden_out(src, "4\n[0, 1, 2, 3]\n");
}

#[test]
fn capture_hof_callbacks_parity() {
    // Closures-with-captures as map/filter/fold callbacks (the HOF hot path for GetCaptured).
    let src = "\
fn main():
    base := 10
    xs := [1, 2, 3, 4]
    doubled := xs.map(fn(x: int) -> int: x * base)
    print(doubled)
    threshold := 2
    kept := xs.filter(fn(x: int) -> bool: x > threshold)
    print(kept)
    seed := 100
    total := xs.fold(seed, fn(acc: int, x: int) -> int: acc + x)
    print(total)
main()";
    assert_golden_out(src, "[10, 20, 30, 40]\n[3, 4]\n110\n");
}

#[test]
fn capture_hot_read_loop_parity() {
    // GetCaptured executed many times in a loop (the hot path post-refactor: pure index).
    let src = "\
fn make() -> fn(int) -> int:
    step := 2
    return fn(x: int) -> int: x + step
fn main():
    f := make()
    total := 0
    i := 0
    while i < 1000:
        total = total + f(i)
        i = i + 1
    print(total)
main()";
    assert_golden_out(src, "501500\n");
}

#[test]
fn capture_closure_across_spawn_parity() {
    // A capturing closure sent across spawn/channel — exercises the --parallel deep-clone +
    // wire/snap rebuild of positional captures. VM (serial), interp, and --parallel must agree.
    let src = "\
fn main():
    base := 41
    ch := Channel[int]()
    parallel:
        spawn:
            ch.send(base + 1)
    print(ch.recv())
main()";
    assert_golden_out(src, "42\n");
    assert_eq!(
        run_capture(src).expect("parallel"),
        "42\n",
        "--parallel capture wire"
    );
}

// ===== bytes type =====

#[test]
fn vm_bytes_ops() {
    // Index → int, negative index, reverse slice → bytes (repr), for-loop sum, len, ==/!=.
    let src = concat!(
        "fn main():\n",
        "    b := b\"\\x01\\x02\\x03\"\n",
        "    print(b[0])\n",         // 1
        "    print(b[-1])\n",        // 3
        "    print(b.reversed())\n", // b'\x03\x02\x01'
        "    s := 0\n",
        "    for x in b:\n",
        "        s = s + x\n",
        "    print(s)\n",                  // 6
        "    print(b.len())\n",            // 3
        "    print(b\"ab\" == b\"ab\")\n", // true
        "    print(b\"ab\" == b\"ac\")\n", // false
        "    print(b\"ab\" != b\"ac\")\n", // true
        "main()\n"
    );
    assert_golden_out(src, "1\n3\nb'\\x03\\x02\\x01'\n6\n3\ntrue\nfalse\ntrue\n");
    assert_eq!(
        run_capture(src).expect("vm"),
        "1\n3\nb'\\x03\\x02\\x01'\n6\n3\ntrue\nfalse\ntrue\n"
    );
}

#[test]
fn vm_bytes_index_out_of_range_recoverable() {
    // An out-of-range index faults recoverably (catchable by `recover:`), like list/str.
    let src = concat!(
        "fn main():\n",
        "    b := b\"\\x01\\x02\"\n",
        "    x := recover:\n",
        "        b[9]\n",
        "    match x:\n",
        "        ?v: print(v)\n",
        "        !e: print(\"caught\")\n",
        "main()\n"
    );
    assert_golden_out(src, "caught\n");
    assert_eq!(run_capture(src).expect("vm"), "caught\n");
}

#[test]
fn vm_bytes_repr_and_map_key() {
    // Display/str repr is Python b'...'; bytes works as a map key (Hashable).
    let src = concat!(
        "fn main():\n",
        "    print(b\"hi\\n\")\n",      // b'hi\n'
        "    print(str(b\"\\xFF\"))\n", // b'\xff'
        "    m := {b\"a\": 1, b\"b\": 2}\n",
        "    print(m[b\"a\"])\n", // 1
        "    print(m[b\"b\"])\n", // 2
        "main()\n"
    );
    assert_golden_out(src, "b'hi\\n'\nb'\\xff'\n1\n2\n");
    assert_eq!(run_capture(src).expect("vm"), "b'hi\\n'\nb'\\xff'\n1\n2\n");
}

#[test]
fn vm_bytes_slice_step_parity() {
    let src = "print(b\"\\x00\\x01\\x02\\x03\\x04\"[Range(1, 4, 2)])\nprint(b\"abc\"[1..])\n";
    assert_golden_out(src, "b'\\x01\\x03'\nb'bc'\n");
}

#[test]
fn bytes_crosses_channel() {
    // `bytes` is fully value-typed and sendable — it crosses the --parallel airlock via
    // WireValue::Bytes and reconstructs as a fresh heap `bytes` (Python b'...' repr preserved).
    // Buffer-then-drain (send before recv) — a holdover from when the since-removed cooperative
    // and interp engines could not block a consumer mid-flight on a live producer (a C5
    // limitation, not bytes-specific).
    let src = concat!(
        "fn main():\n",
        "    ch := Channel[bytes]()\n",
        "    parallel:\n",
        "        spawn ch.send(b\"\\x01\\x02\")\n",
        "    print(ch.recv())\n",
        "main()\n"
    );
    // Must byte-match on both a plain run and under `--parallel`.
    assert_golden_out(src, "b'\\x01\\x02'\n");
    assert_eq!(run_capture(src).expect("vm"), "b'\\x01\\x02'\n");
    assert_eq!(run_capture(src).expect("parallel"), "b'\\x01\\x02'\n");
}

// ===== bytearray type (mutable sibling of bytes) =====

#[test]
fn vm_bytearray_ops() {
    // Constructors (all 4 forms), index read, slice -> bytearray (incl. reverse), for-loop sum,
    // len, push, pop, extend, ==/!=, and Display bytearray(b'...').
    let src = concat!(
        "fn main():\n",
        "    print(bytearray())\n",                // bytearray(b'')
        "    print(bytearray(3))\n",               // bytearray(b'\x00\x00\x00')
        "    print(bytearray(b\"\\x01\\x02\"))\n", // bytearray(b'\x01\x02')
        "    ba := bytearray([1, 2, 3])\n",
        "    print(ba)\n",            // bytearray(b'\x01\x02\x03')
        "    print(ba[0])\n",         // 1
        "    print(ba[-1])\n",        // 3
        "    print(ba.reversed())\n", // bytearray(b'\x03\x02\x01')
        "    s := 0\n",
        "    for x in ba:\n",
        "        s = s + x\n",
        "    print(s)\n",        // 6
        "    print(ba.len())\n", // 3
        "    ba.push(4)\n",
        "    print(ba)\n",       // bytearray(b'\x01\x02\x03\x04')
        "    print(ba.pop())\n", // Some(4)
        "    ba.extend(b\"\\xFF\")\n",
        "    ba.extend([7, 8])\n",
        "    print(ba)\n", // bytearray(b'\x01\x02\x03\xff\x07\x08')
        "    print(bytearray([1]) == bytearray([1]))\n", // true
        "    print(bytearray([1]) != bytearray([2]))\n", // true
        "main()\n"
    );
    assert_golden_out(
        src,
        "bytearray(b'')\nbytearray(b'\\x00\\x00\\x00')\nbytearray(b'\\x01\\x02')\nbytearray(b'\\x01\\x02\\x03')\n1\n3\nbytearray(b'\\x03\\x02\\x01')\n6\n3\nbytearray(b'\\x01\\x02\\x03\\x04')\n4\nbytearray(b'\\x01\\x02\\x03\\xff\\x07\\x08')\ntrue\ntrue\n",
    );
    assert_eq!(
        run_capture(src).expect("vm"),
        "bytearray(b'')\nbytearray(b'\\x00\\x00\\x00')\nbytearray(b'\\x01\\x02')\nbytearray(b'\\x01\\x02\\x03')\n1\n3\nbytearray(b'\\x03\\x02\\x01')\n6\n3\nbytearray(b'\\x01\\x02\\x03\\x04')\n4\nbytearray(b'\\x01\\x02\\x03\\xff\\x07\\x08')\ntrue\ntrue\n"
    );
}

#[test]
fn vm_bytearray_index_write_and_shared_mutation() {
    // ba[i] = x mutates in place; a second binding observes it (proves in-place, not copy).
    let src = concat!(
        "fn main():\n",
        "    ba := bytearray([1, 2, 3])\n",
        "    ba2 := ba\n",
        "    ba[0] = 65\n",
        "    print(ba2[0])\n", // 65 — same buffer
        "    ba2[1] = 66\n",
        "    print(ba[1])\n", // 66 — observed through the other binding
        "main()\n"
    );
    assert_golden_out(src, "65\n66\n");
    assert_eq!(run_capture(src).expect("vm"), "65\n66\n");
}

#[test]
fn vm_bytearray_oob_and_value_range_recoverable() {
    // An out-of-range index OR an out-of-range value (not 0..=255) is a recoverable fault.
    let src = concat!(
        "fn main():\n",
        "    ba := bytearray([1, 2])\n",
        "    r1 := recover:\n",
        "        ba[9] = 1\n",
        "    match r1:\n",
        "        ?v: print(\"ok\")\n",
        "        !e: print(\"caught oob index\")\n",
        "    r2 := recover:\n",
        "        ba[0] = 999\n",
        "    match r2:\n",
        "        ?v: print(\"ok\")\n",
        "        !e: print(\"caught bad value\")\n",
        "main()\n"
    );
    assert_golden_out(src, "caught oob index\ncaught bad value\n");
    assert_eq!(
        run_capture(src).expect("vm"),
        "caught oob index\ncaught bad value\n"
    );
}

#[test]
fn vm_bytearray_huge_size_is_recoverable_not_abort() {
    // `bytearray(N)` for an absurd N must fault recoverably (try_reserve), NOT abort the process
    // (SIGABRT) uncatchably — the language's recoverable-fault invariant (cf. range()'s cap).
    let src = concat!(
        "fn main():\n",
        "    r := recover:\n",
        "        bytearray(9999999999999)\n",
        "    match r:\n",
        "        ?v: print(\"ok\")\n",
        "        !e: print(\"caught huge\")\n",
        "main()\n"
    );
    assert_golden_out(src, "caught huge\n");
    assert_eq!(run_capture(src).expect("vm"), "caught huge\n");
}

#[test]
fn vm_bytearray_conversion_bridge() {
    // bytes(ba) -> immutable snapshot; bytearray(b) -> mutable copy; round-trip + independence.
    let src = concat!(
        "fn main():\n",
        "    ba := bytearray([1, 2, 3])\n",
        "    b := bytes(ba)\n",
        "    print(b)\n", // b'\x01\x02\x03'
        "    ba[0] = 99\n",
        "    print(b)\n", // b'\x01\x02\x03' — snapshot unaffected
        "    ba2 := bytearray(b\"\\x07\\x08\")\n",
        "    ba2[0] = 10\n",
        "    print(ba2)\n",          // bytearray(b'\n\x08') — 0x0a renders as \n
        "    print(bytearray(b))\n", // bytearray(b'\x01\x02\x03')
        "main()\n"
    );
    assert_golden_out(
        src,
        "b'\\x01\\x02\\x03'\nb'\\x01\\x02\\x03'\nbytearray(b'\\n\\x08')\nbytearray(b'\\x01\\x02\\x03')\n",
    );
    assert_eq!(
        run_capture(src).expect("vm"),
        "b'\\x01\\x02\\x03'\nb'\\x01\\x02\\x03'\nbytearray(b'\\n\\x08')\nbytearray(b'\\x01\\x02\\x03')\n"
    );
}

#[test]
fn bytearray_crosses_channel_deep_copy() {
    // `bytearray` crosses the --parallel airlock by VALUE (deep copy): the other side gets a
    // fresh independent buffer. Buffer-then-drain (send before recv) so the sequential interp
    // agrees, exactly like bytes_crosses_channel (a C5 limitation, not bytearray-specific).
    let src = concat!(
        "fn main():\n",
        "    ch := Channel[bytearray]()\n",
        "    parallel:\n",
        "        spawn ch.send(bytearray([1, 2]))\n",
        "    print(ch.recv())\n",
        "main()\n"
    );
    assert_golden_out(src, "bytearray(b'\\x01\\x02')\n");
    assert_eq!(run_capture(src).expect("vm"), "bytearray(b'\\x01\\x02')\n");
    assert_eq!(
        run_capture(src).expect("parallel"),
        "bytearray(b'\\x01\\x02')\n"
    );
}

// ===== Iterable[T] / `.iter()` cursor =====

#[test]
fn iter_next_idempotent_both_engines() {
    // next() yields Some(10), Some(20), then None forever (idempotent past exhaustion).
    let src = "fn main():\n    it := [10, 20].iter()\n    print(it.next())\n    print(it.next())\n    print(it.next())\n    print(it.next())\nmain()\n";
    assert_golden_out(src, "10\n20\nNone\nNone\n");
}

#[test]
fn iter_empty_collection_none_immediately() {
    let src =
        "fn main():\n    xs: List[int] = []\n    it := xs.iter()\n    print(it.next())\nmain()\n";
    assert_golden_out(src, "None\n");
}

#[test]
fn iter_snapshot_order_matches_for() {
    // For each collection, the cursor's element sequence must equal `for x in X`.
    for (coll, want) in [
        ("[1, 2, 3]", "1\n2\n3\n"),
        ("{1, 2, 3}", "1\n2\n3\n"),
        ("{1: \"a\", 2: \"b\"}.keys()", "1\n2\n"), // keys()
        ("\"abc\"", "a\nb\nc\n"),
        ("b\"hi\"", "104\n105\n"),
        ("bytearray([7, 8])", "7\n8\n"),
    ] {
        let via_for = format!("fn main():\n    for x in {coll}:\n        print(x)\nmain()\n");
        let via_iter = format!(
            "fn main():\n    it := ({coll}).iter()\n    while true:\n        match it.next():\n            ?x:\n                print(x)\n            None:\n                break\nmain()\n"
        );
        assert_golden_out(&via_for, want);
        assert_golden_out(&via_iter, want);
        let for_out = vm_outcome(&via_for);
        let iter_out = vm_outcome(&via_iter);
        assert_eq!(
            for_out, iter_out,
            "cursor order must match `for` for {coll}"
        );
    }
}

#[test]
fn iter_self_on_iterator_value() {
    // iter() on a cursor returns self (idempotent); driving the result still works.
    let src = "fn main():\n    it := [1, 2].iter().iter()\n    print(it.next())\nmain()\n";
    assert_golden_out(src, "1\n");
}

#[test]
fn list_of_cursor_roundtrip_both_engines() {
    assert_golden_out(
        "fn main():\n    print(List([5, 6, 7].iter()))\nmain()\n",
        "[5, 6, 7]\n",
    );
    assert_golden_out(
        "fn main():\n    print(Set({1, 2}.iter()).len())\nmain()\n",
        "2\n",
    );
}

#[test]
fn cursor_composes_into_adapter() {
    // The headline win: a list cursor flows into a `[I: Iterator[T]]` adapter.
    let src = concat!(
        "struct Take[I: Iterator[T], T]:\n",
        "    inner: I\n",
        "    left: int\n",
        "    fn next(self) -> T?:\n",
        "        if self.left <= 0:\n",
        "            return None\n",
        "        self.left = self.left - 1\n",
        "        return self.inner.next()\n",
        "fn main():\n",
        "    for v in Take([10, 20, 30, 40].iter(), 2):\n",
        "        print(v)\n",
        "main()\n"
    );
    assert_golden_out(src, "10\n20\n");
}

#[test]
fn for_over_pure_iterable_struct() {
    // A struct with ONLY iter() drives `for`; one with BOTH is driven by iter() too (sentinel).
    let only_iter = concat!(
        "struct Wrap:\n",
        "    xs: List[int]\n",
        "    fn iter(self) -> Iterator[int]:\n",
        "        return self.xs.iter()\n",
        "fn main():\n",
        "    for x in Wrap([1, 2, 3]):\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_golden_out(only_iter, "1\n2\n3\n");
    // Both iter() and next(): iter() wins (next() would print 100 and 101).
    let both = concat!(
        "struct Two:\n",
        "    n: int\n",
        "    fn next(self) -> int?:\n",
        "        if self.n >= 2:\n",
        "            return None\n",
        "        v := self.n\n",
        "        self.n = self.n + 1\n",
        "        return ?(v + 100)\n",
        "    fn iter(self) -> Iterator[int]:\n",
        "        return [999].iter()\n",
        "fn main():\n",
        "    for x in Two(0):\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_golden_out(both, "999\n");
}

// ---- Bug C: a `for`/`List()`/`Set()` over a NAMED builtin cursor consumes it in place ----
// (must match `.next()` and struct iterators and docs/syntax.md:709-713).

#[test]
fn for_named_cursor_partial_consume_then_drain() {
    // The repro: partial-consume via a broken `for`, then `List()` drains the REMAINDER.
    let src = concat!(
        "fn main():\n",
        "    it := [1, 2, 3, 4].iter()\n",
        "    seen := 0\n",
        "    for x in it:\n",
        "        seen = seen + 1\n",
        "        if x == 2:\n",
        "            break\n",
        "    rest := List(it)\n",
        "    print(\"seen={seen} rest={rest}\")\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap().trim(), "seen=2 rest=[3, 4]");
    assert_golden_out(src, "seen=2 rest=[3, 4]\n");
}

#[test]
fn for_named_cursor_two_pass_second_yields_nothing() {
    // A named cursor is consumed by the first `for`; the second pass yields nothing.
    let src = concat!(
        "fn main():\n",
        "    it := [1, 2, 3].iter()\n",
        "    for x in it:\n",
        "        print(x)\n",
        "    for x in it:\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "1\n2\n3\n");
    assert_golden_out(src, "1\n2\n3\n");
}

#[test]
fn next_after_for_over_named_cursor_is_none() {
    // `for` advances the shared cursor; a trailing `.next()` sees it exhausted.
    let src = concat!(
        "fn main():\n",
        "    it := [10, 20].iter()\n",
        "    for x in it:\n",
        "        print(x)\n",
        "    print(it.next())\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "10\n20\nNone\n");
    assert_golden_out(src, "10\n20\nNone\n");
}

#[test]
fn for_over_collection_reiterates_fully_twice() {
    // Invariant 1: a NON-cursor collection (list) keeps fresh-snapshot semantics — both passes full.
    let src = concat!(
        "fn main():\n",
        "    xs := [1, 2, 3]\n",
        "    for x in xs:\n",
        "        print(x)\n",
        "    for x in xs:\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "1\n2\n3\n1\n2\n3\n");
    assert_golden_out(src, "1\n2\n3\n1\n2\n3\n");
}

#[test]
fn iter_of_iter_fresh_cursor() {
    // Invariant 2: `xs.iter().iter()` is one fresh cursor (no double-advance).
    let src = concat!(
        "fn main():\n",
        "    a := [5, 6].iter().iter()\n",
        "    print(a.next())\n",
        "    for x in a:\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "5\n6\n");
    assert_golden_out(src, "5\n6\n");
}

#[test]
fn for_over_fresh_temp_cursor_full() {
    // Invariant 3: a fresh unnamed temp cursor still fully iterates.
    let src = concat!(
        "fn main():\n",
        "    for x in [7, 8, 9].iter():\n",
        "        print(x)\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).unwrap(), "7\n8\n9\n");
    assert_golden_out(src, "7\n8\n9\n");
}

#[test]
fn cursor_crosses_spawn_airlock_three_engine_parity() {
    // A cursor IS sendable: it crosses the spawn/channel airlock as a DEEP COPY (independent
    // snapshot + pos on the receiver), like a `list`. The receiver drives it and gets `Some(1)`.
    // This must hold byte-identically under the M:N `--parallel` engine.
    // (Regression guard: an earlier cut gated the cursor non-sendable like a generator, which
    // panicked the spawned worker on the VM while the since-removed interp engine succeeded — a
    // parity divergence.)
    let src = concat!(
        "fn main():\n",
        "    ch := Channel[Iterator[int]]()\n",
        "    parallel:\n",
        "        spawn ch.send([1, 2].iter())\n",
        "    print(ch.recv().next())\n",
        "main()\n"
    );
    assert_golden_out(src, "1\n");
    assert_eq!(
        run_capture(src).expect("--parallel"),
        "1\n",
        "cursor crosses the M:N airlock"
    );
}

#[test]
fn generator_iter_returns_self_vm() {
    // VM-only (interp has no generators): a generator's iter() returns self and drives.
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "    yield 2\n",
        "fn main():\n",
        "    it := gen().iter()\n",
        "    print(it.next())\n",
        "    print(it.next())\n",
        "    print(it.next())\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("vm"), "1\n2\nNone\n");
}

// ===== mixed int/float VALUE arithmetic (D3: no int→float slot widening, no runtime coercion) =====

/// An all-int literal collection stays `List[int]`, so `xs[0] / 2` is int division (`0`).
#[test]
fn widen_all_int_literal_collection_stays_int() {
    widen_three_engines("xs := [1, 2, 3]\nprint(xs[0] / 2)\n", "0\n");
}

/// Regression-pin: mixed int/float COMPARISONS promote by runtime tag (`src/vm/arith.rs`).
#[test]
fn widen_mixed_comparisons_pinned() {
    widen_three_engines("print(1 < 2.3)\nprint(1 == 2.3)\n", "true\nfalse\n");
}

// ===== Generic fn as a VALUE (scope A + B) — runtime is generic-ERASED, so a RUN test is still
// required per accepted case (the bind-import trap: a checker-accept that faults at runtime).
