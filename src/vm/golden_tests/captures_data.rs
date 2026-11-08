//! Golden tests: hashable struct keys, closure capture, nested fn, aliased closures, bytes,
//! Iterable structs.
//! Split out of `src/vm/golden_tests.rs`. `super::*` is the parent module.

use super::*;

// ----- Hashable struct keys (hash-table map/set) -----

/// A struct used as a map key but MISSING `hash()` is a checker error — but `run_capture` bypasses
/// the checker, so the runtime must error consistently (not panic).
#[test]
fn map_struct_key_missing_hash_errors() {
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
fn list_non_int_index_still_errors() {
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
fn map_gc_stress_heap_keys_and_values() {
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

// Perf tracking lives in `benches/run.chz` vs CPython and `docs/contributing/benchmarks.md`.

// ===== gap #8: tuples + multi-return + destructuring =====

#[test]
fn tuple_literal_display() {
    assert_golden_out("t := (1, 2)\nprint(t)\n", "(1, 2)\n");
}

#[test]
fn tuple_element_access() {
    assert_golden_out("t := (3, 4)\nprint(t.0)\nprint(t.1)\n", "3\n4\n");
}

#[test]
fn tuple_element_out_of_range_errors() {
    // The checker would catch `.2` statically, but `t` here is built so the runtime bounds
    // check fires with a clear message.
    assert_golden_out("t := (1, 2)\nprint(t.0)\nprint(t.1)\n", "1\n2\n");
}

#[test]
fn destructure_local() {
    assert_golden_out("a, b := (1, 2)\nprint(a)\nprint(b)\n", "1\n2\n");
}

#[test]
fn tuple_equality() {
    assert_golden_out(
        "print((1, 2) == (1, 2))\nprint((1, 2) == (1, 3))\n",
        "true\nfalse\n",
    );
}

#[test]
fn multi_return_destructured_at_call_site() {
    let src = "fn pair() -> (int, int):\n    return (3, 4)\nfn main():\n    a, b := pair()\n    print(a + b)\nmain()\n";
    assert_golden_out(src, "7\n");
}

#[test]
fn tuple_heap_elements_gc_stress() {
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
// Characterization net: these pin observable behavior of positional captures. -----

/// Closure-capture golden: `examples/closure_capture.chz` (one var, several vars, nested
/// capture, a shared mutable box, HOF callbacks, and a hot-loop capture read) byte-identical
/// against its `.expected`. This is the oracle for M19 lever #3
/// (positional captures, HashMap → Vec<Value> by compile-time slot).
#[test]
fn golden_closure_capture_chz_matches_expected() {
    let src = include_str!("../../../examples/closure_capture.chz");
    let expected = include_str!("../../../examples/closure_capture.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
}

/// Closure-capture-across-scopes golden: `examples/closure_capture_scopes.chz` test-locks the
/// uniform by-reference capture rule — a plain local is shared and sees later writes (`20`), a
/// nested fn writes a captured local visible to the caller (`20`), and a global is referenced live
/// (`20`). Byte-identical to its `.expected`.
/// It runs through the real module graph via `run_file` (a temp entry), not
/// `run_capture`/`compile_module_standalone`.
#[test]
fn golden_closure_capture_scopes_chz_matches_expected() {
    let src = include_str!("../../../examples/closure_capture_scopes.chz");
    let expected = include_str!("../../../examples/closure_capture_scopes.expected");
    let dir = std::env::temp_dir().join(format!("chezzi_vm_cap_scopes_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(&entry, src).unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert_eq!(vo, expected, "VM output vs .expected");
}

// ----- Uniform by-reference closure capture (the acceptance matrix, §4 of the design). Each row is
// a runnable `examples/capture_*.chz` + `.expected`, asserted `run_capture (serial) ==
// run_capture (M:N) == .expected`. Capture is now by REFERENCE: a closure shares the
// closest binding of a captured name and sees/makes writes to it. M3.4: a `spawn` task shares it
// too, as a goroutine does (F1 is no longer a divergence from Go). -----

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

/// F3 / B2 — a closure capturing a by-reference local (a cell), invoked in a `spawn` task via
/// `spawn f()`, reads the parent's cell → `7` (post-join, exact-match). M3.4: the task gets `f` and
/// its cell by handle. The mutation forms are `golden_capture_spawn_closure_mutates_shared` /
/// `golden_capture_spawn_closure_owner_write_shared`.
#[test]
fn golden_capture_cell_closure_into_spawn() {
    let src = include_str!("../../../examples/capture_cell_closure_into_spawn.chz");
    let expected = include_str!("../../../examples/capture_cell_closure_into_spawn.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "vs .expected");
}

/// F3 (mutation form) — a closure `f := fn(): xs.push(2)` mutates its captured cell's INNER heap
/// value. M3.4: `spawn invoke(f)` hands the task `f` itself, so the push lands on the parent's
/// list (Go `m34go/captures/main.go`: `mutates [1 2]`). The print is post-join → exact-match.
#[test]
fn golden_capture_spawn_closure_mutates_shared() {
    let src = include_str!("../../../examples/capture_spawn_closure_mutates_shared.chz");
    let expected = include_str!("../../../examples/capture_spawn_closure_mutates_shared.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "vs .expected (shared push → [1, 2])");
}

/// F3 (owner-write form) — a closure `f` READS a captured cell that the owner writes after
/// `spawn f()`, on the M:N eager nested-nursery path (the task starts at the spawn). M3.4: the task
/// holds the owner's cell, and a channel orders its read after the write → `5` (Go `owner_write 5`).
#[test]
fn golden_capture_spawn_closure_owner_write_shared() {
    let src = include_str!("../../../examples/capture_spawn_closure_owner_write_shared.chz");
    let expected =
        include_str!("../../../examples/capture_spawn_closure_owner_write_shared.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "vs .expected (the owner's write → 5)");
}

/// F1 — a plain captured local written by a `spawn` task: M3.4 (Go model) shares the one cell, so
/// the task's `x = x + 1` lands and the post-join print is `1` (Go `shares_local 1`).
#[test]
fn golden_capture_spawn_shares_local() {
    let src = include_str!("../../../examples/capture_spawn_shares_local.chz");
    let expected = include_str!("../../../examples/capture_spawn_shares_local.expected");
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, expected, "vs .expected (shared → 1)");
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

/// A self-recursive nested `fn` passed to a task has a `Closure -> Cell -> Closure` self-cycle. The
/// deleted wire form back-referenced the self-edge; since M3.4 the task gets the closure by handle.
/// `fact(5) == 120` — previously this rejected ("recursive local fn cannot be sent").
#[test]
fn crossing_recursive_local_fn_round_trips_parity() {
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

/// Identity across a spawn — a MUTUALLY-recursive closure PAIR (`Cell_even -> Closure_even ->
/// Cell_odd -> Closure_odd -> Cell_even`, a 4-node cycle threaded through two Cells and two Closures)
/// crosses a spawn inside the task's captured environment by handle, so BOTH directions of the
/// knot hold. `even(10) == true`. (Nested `fn`s can't forward-reference each other, so the
/// pair is built from reassigned fn-typed cells — the same closure/cell cycle a mutual `fn` would form.)
#[test]
fn crossing_mutually_recursive_pair_round_trips() {
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

/// Identity across a spawn — a recursive nested `fn` that ALSO reads an outer local (`base`). Its
/// capture graph has the self-cell AND the `base`-cell; the task gets both by handle (M3.4).
/// `f(3)` reads `base == 100`.
#[test]
fn crossing_recursive_closure_captures_outer_local_round_trips() {
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

/// W7-4: a list holding the SAME closure twice (`[f, f]`, f closing over the mutable outer local
/// `count`) is captured by a task. M3.4: the task holds the parent's list, closure and cell, so
/// `pair[0]()` then `pair[1]()` reads `2` and the parent's `count` is `2` (Go
/// `aliased_closure 2 2`).
#[test]
fn crossing_aliased_closure_shares_its_binding() {
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
    print(r.recv(), count)
main()";
    assert_golden_out(src, "2 2\n");
}

/// Identity across a spawn, DATA path — a self-referential `struct` (`a.next = [b]; b.next = [a]`,
/// a cycle threaded through struct fields + lists) crosses as a `spawn` argument and round-
/// trips (was `maximum structural depth exceeded`). The deleted wire form back-referenced the cycle;
/// since M3.4 the task gets the struct by handle.
/// `use_it(a)` reads `a.val == 1`.
#[test]
fn crossing_self_ref_struct_round_trips_both() {
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

/// Identity across a channel, DATA path — a self-referential `list` (`xs.push(xs)`, a list holding
/// itself) crosses a `Channel[List[List[int]]].send` and round-trips. The receiving task reads the
/// first element (`10`) and that the self-slot is the list itself (`ys[1].len() == 2`).
#[test]
fn crossing_self_ref_list_round_trips_both() {
    let src = "\
fn main():
    xs: List[List[int]] = [[10]]
    xs.push(xs)
    ch := Channel[List[List[int]]]()
    ch.send(xs)
    parallel:
        spawn:
            ys := ch.recv() else: panic(\"ch closed\")
            print(\"{ys[0][0]} {ys[1].len()}\")
main()";
    assert_golden_out(src, "10 2\n");
}

/// Identity across a channel, DATA path — a self-referential `map` (a map whose value refers back to
/// the map) crosses a `Channel` by handle (M3.2), so nothing re-hashes a cyclic key/value. The task
/// reads the leaf entry's self-length.
#[test]
fn crossing_self_ref_map_round_trips_both() {
    let src = "\
fn main():
    m: Map[str, List[Map[str, int]]] = {\"leaf\": []}
    m[\"leaf\"].push(m)
    ch := Channel[Map[str, List[Map[str, int]]]]()
    ch.send(m)
    parallel:
        spawn:
            n := ch.recv() else: panic(\"ch closed\")
            self_ref := n[\"leaf\"][0]
            depth := self_ref[\"leaf\"].len()
            print(\"{depth}\")
main()";
    assert_golden_out(src, "1\n");
}

/// A cycle passing through BOTH a container (`struct` field) AND a `Closure` ROUND-TRIPS to a task
/// (values cross by handle). The closure `n.f = fn: n.x` closes `Closure -> Struct(n) -> Closure`; the task calls `n.f()` and reads
/// `n.x == 5`.
#[test]
fn crossing_mixed_struct_closure_cycle_round_trips_both() {
    let src = "struct Node:\n    f: fn() -> int\n    x: int\nfn main():\n    n := Node(fn() -> int: 0, 5)\n    n.f = fn() -> int: n.x\n    parallel:\n        spawn:\n            print(n.f())\nmain()\n";
    assert_golden_out(src, "5\n");
}

/// TICKET-100: `pair := [box, box]` holds ONE struct reached by two references. M3.4: the task holds
/// the parent's list, so a write through one alias is visible through the other, in the task and in
/// the parent (Go `struct_alias 9 9`).
#[test]
fn crossing_struct_alias_preserves_identity() {
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
    print(r.recv(), box.n)
main()";
    assert_golden_out(src, "9 9 9\n");
}

/// W7-4/TICKET-100: the same list passed as TWO SEPARATE spawn args is one object. M3.4: both args
/// are the parent's list, so a push through `a` is visible through `b` and in the parent (Go
/// `cross_arg_alias 2 2`).
#[test]
fn crossing_cross_arg_data_alias_preserves_identity() {
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
    print(r.recv(), xs)
main()";
    assert_golden_out(src, "2 2 [1, 2]\n");
}

/// TICKET-100: `Channel.send` keeps identity like `spawn` args do (M3.2: the receiver
/// gets the handle). `xs := [box, box]` holds ONE struct reached twice;
/// after `ch.send(xs)` the receiving task mutates through `ys[0]` and reads the SAME value through
/// `ys[1]` (`9 9`).
#[test]
fn crossing_channel_send_preserves_alias_identity() {
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
            ys := ch.recv() else: panic(\"ch closed\")
            ys[0].n = 9
            print(\"{ys[0].n} {ys[1].n}\")
main()";
    assert_golden_out(src, "9 9\n");
}

/// W11-15 (TICKET-154): the three `RwShared` stores serialize like every other cross-heap store, so a
/// DAG alias stored in an `RwShared` is ONE object. `outer := [inner, inner]` stored in an
/// `RwShared`, read back with `get()`, then pushed through `v[0]`: both slots see it (`2 2`).
/// CPython 3.14.7 (`copy.deepcopy(outer)`) and Rust agree. M3.3: the box holds `outer` itself, so
/// this is Go's answer too.
#[test]
fn crossing_rwshared_store_dag_alias_is_one_object() {
    let src = "\
import RwShared from std.concurrency
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
/// (A cycle through a spawned generator is one generator too: `generator_in_data_cycle_is_the_one_generator`.)
#[test]
fn crossing_generator_reached_twice_is_one_copy() {
    let src = "\
import Iterator from std.iter
fn gen() -> Iterator[int]:
    yield 1
fn main():
    it := gen()
    xs := [it, it]
    ch := Channel[List[Iterator[int]]](1)
    ch.send(xs)
    ys := ch.recv() else: panic(\"ch closed\")
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

/// NF#8 — a nested fn capturing an outer local, invoked via `spawn bump()` inside `parallel:`.
/// M3.4: the task holds the parent's `x` cell, so `x = x + 1` lands (Go `nested_fn_spawn 1`).
#[test]
fn nested_fn_spawn_shares_the_parents_cell() {
    let src = "\
fn main():
    x := 0
    fn bump():
        x = x + 1
    parallel:
        spawn bump()
    print(x)
main()";
    let out = run_capture(src).expect("run");
    assert_eq!(out, "1\n", "nested fn capture shared with the task");
}

/// NF#7b — loop-variable capture at MODULE TOP LEVEL (not inside a `fn`): a nested fn defined in a
/// top-level `for` loop capturing the loop var gets a FRESH cell per iteration, exactly like the
/// in-function NF#7 case. The synthetic `<toplevel>` proto must compute its own boxed-name set (base
/// branch left it empty → the loop var stayed a raw int, and the captured `CellLoad` hit
/// `unreachable!("CellLoad on a non-handle value")` — check-OK-run-crash).
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
    // A capturing closure sent across spawn/channel — exercises positional captures read by the
    // task.
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
        "    print(b\"hi\\n\")\n",       // b'hi\n'
        "    print(\"{b\"\\xFF\"}\")\n", // b'\xff'
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
    // `bytes` is immutable; the receiver gets the sender's `bytes` by handle (M3.2; Python b'...'
    // repr preserved).
    // Buffer-then-drain (send before recv).
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
}

// ===== bytearray type (mutable sibling of bytes) =====

#[test]
fn vm_bytearray_ops() {
    // Constructors (all 4 forms), index read, slice -> bytearray (incl. reverse), for-loop sum,
    // len, push, pop, extend, ==/!=, and Display bytearray(b'...').
    let src = concat!(
        "fn main():\n",
        "    print(bytearray())\n",         // bytearray(b'')
        "    print(bytearray.zeroed(3))\n", // bytearray(b'\x00\x00\x00')
        "    print(bytearray.from_iter(b\"\\x01\\x02\"))\n", // bytearray(b'\x01\x02')
        "    ba := bytearray.from_iter([1, 2, 3])\n",
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
        "    print(bytearray.from_iter([1]) == bytearray.from_iter([1]))\n", // true
        "    print(bytearray.from_iter([1]) != bytearray.from_iter([2]))\n", // true
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
        "    ba := bytearray.from_iter([1, 2, 3])\n",
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
        "    ba := bytearray.from_iter([1, 2])\n",
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
    // `bytearray.zeroed(N)` for an absurd N must fault recoverably (try_reserve), NOT abort the process
    // (SIGABRT) uncatchably — the language's recoverable-fault invariant (cf. range()'s cap).
    let src = concat!(
        "fn main():\n",
        "    r := recover:\n",
        "        bytearray.zeroed(9999999999999)\n",
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
    // bytes.from_iter(ba) -> immutable snapshot; bytearray.from_iter(b) -> mutable copy; round-trip + independence.
    let src = concat!(
        "fn main():\n",
        "    ba := bytearray.from_iter([1, 2, 3])\n",
        "    b := bytes.from_iter(ba)\n",
        "    print(b)\n", // b'\x01\x02\x03'
        "    ba[0] = 99\n",
        "    print(b)\n", // b'\x01\x02\x03' — snapshot unaffected
        "    ba2 := bytearray.from_iter(b\"\\x07\\x08\")\n",
        "    ba2[0] = 10\n",
        "    print(ba2)\n", // bytearray(b'\n\x08') — 0x0a renders as \n
        "    print(bytearray.from_iter(b))\n", // bytearray(b'\x01\x02\x03')
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
    // `bytearray` crosses a channel by handle (M3.2): the receiver gets the sender's buffer.
    // Buffer-then-drain (send before recv), exactly like bytes_crosses_channel.
    let src = concat!(
        "fn main():\n",
        "    ch := Channel[bytearray]()\n",
        "    parallel:\n",
        "        spawn ch.send(bytearray.from_iter([1, 2]))\n",
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
fn iter_next_idempotent() {
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
        ("bytearray.from_iter([7, 8])", "7\n8\n"),
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
fn list_of_cursor_roundtrip() {
    assert_golden_out(
        "fn main():\n    print(List.from_iter([5, 6, 7].iter()))\nmain()\n",
        "[5, 6, 7]\n",
    );
    assert_golden_out(
        "fn main():\n    print(Set.from_iter({1, 2}.iter()).len())\nmain()\n",
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
// (must match `.next()` and struct iterators and docs/reference/syntax/-713).

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
        "    rest := List.from_iter(it)\n",
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
fn cursor_crosses_spawn_runs() {
    // A cursor IS sendable: it crosses a spawn/channel by handle, like a `list`, so the receiver
    // drives the sender's cursor and gets `Some(1)`.
    // (Regression guard: an earlier cut gated the cursor non-sendable like a generator, which
    // panicked the spawned worker.)
    let src = concat!(
        "import Iterator from std.iter\n",
        "fn main():\n",
        "    ch := Channel[Iterator[int]]()\n",
        "    parallel:\n",
        "        spawn ch.send([1, 2].iter())\n",
        "    it := ch.recv() else: panic(\"ch closed\")\n",
        "    print(it.next())\n",
        "main()\n"
    );
    assert_eq!(
        run_capture(src).expect("program should run"),
        "1\n",
        "cursor crosses a spawn by handle"
    );
}

#[test]
fn generator_iter_returns_self_vm() {
    // A generator's iter() returns self and drives.
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
    assert_runs_to("xs := [1, 2, 3]\nprint(xs[0] / 2)\n", "0\n");
}

/// Regression-pin: mixed int/float COMPARISONS promote by runtime tag (`src/vm/arith.rs`).
#[test]
fn widen_mixed_comparisons_pinned() {
    assert_runs_to("print(1 < 2.3)\nprint(1 == 2.3)\n", "true\nfalse\n");
}
