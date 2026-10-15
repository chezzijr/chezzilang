//! VM tests: host args, implicit nursery, fault reclaim.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

/// Build a VM `Map[str, str]` value with the given pairs (insertion order preserved), so the
/// host-side map readers can be unit-tested without compiling a program.
fn build_str_map(vm: &mut Vm, pairs: &[(&str, &str)]) -> Value {
    let span = Span::RUNTIME;
    let mut map = MapData::default();
    for (k, v) in pairs {
        let kv = vm.alloc_str((*k).to_string());
        let vv = vm.alloc_str((*v).to_string());
        let hk = vm.hash_value(kv, span).unwrap();
        map.push(hk, kv, vv);
    }
    Value::obj(vm.heap.alloc(Obj::Map(map)))
}

/// `OffloadHost::arg_str_map` serves the pre-extracted `NativeArg::Map` pairs back (so an
/// offloaded `request()` reads its headers off-thread); a non-map arg errors with `arg_type`.
#[test]
fn offload_host_arg_str_map_roundtrips() {
    use crate::native::Host;
    let mut host = OffloadHost {
        args: vec![
            crate::native::NativeArg::Map(vec![("X-Custom".into(), "value".into())]),
            crate::native::NativeArg::Str("not-a-map".into()),
        ],
    };
    assert_eq!(
        host.arg_str_map(0).unwrap(),
        vec![("X-Custom".into(), "value".into())]
    );
    assert!(
        host.arg_str_map(1).is_err(),
        "a non-map NativeArg must error"
    );
    assert!(host.arg_str_map(9).is_err(), "a missing arg must error");
}

/// `extract_native_args` snapshots a `Map[str, str]` Value into `NativeArg::Map` (insertion
/// order) so `request()` can offload; a non-str-valued map reverts to `None` (run inline).
#[test]
fn extract_native_args_snapshots_str_map() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let m = build_str_map(&mut vm, &[("a", "1"), ("b", "2")]);
    let got = vm.extract_native_args(&[m]).expect("str/str map extracts");
    assert_eq!(
        got,
        vec![crate::native::NativeArg::Map(vec![
            ("a".into(), "1".into()),
            ("b".into(), "2".into()),
        ])],
        "pairs preserved in insertion order"
    );
    // A map with a non-str value (here an int) is not snapshottable → None (safe inline fallback).
    let span = Span::RUNTIME;
    let mut bad = MapData::default();
    let kv = vm.alloc_str("k".to_string());
    let hk = vm.hash_value(kv, span).unwrap();
    bad.push(hk, kv, Value::int(7));
    let bad_map = Value::obj(vm.heap.alloc(Obj::Map(bad)));
    assert_eq!(vm.extract_native_args(&[bad_map]), None);
}

/// `VmHost::arg_str_map` reads a live heap map in insertion order; a non-map arg errors.
#[test]
fn vm_host_arg_str_map_reads_live_map() {
    use crate::native::Host;
    let mut vm = Vm::new(Arc::new(empty_program()));
    let m = build_str_map(&mut vm, &[("one", "1"), ("two", "2")]);
    let not_map = Value::int(3);
    let mut host = VmHost {
        vm: &mut vm,
        args: vec![m, not_map],
    };
    assert_eq!(
        host.arg_str_map(0).unwrap(),
        vec![("one".into(), "1".into()), ("two".into(), "2".into())]
    );
    assert!(host.arg_str_map(1).is_err(), "a non-map arg must error");
}

/// `OffloadHost::arg_str_list` serves the pre-extracted `NativeArg::List` argv back (so an
/// offloaded `run_args` reads its arguments off-thread); a non-list arg errors with `arg_type`.
#[test]
fn offload_host_arg_str_list_roundtrips() {
    use crate::native::Host;
    let mut host = OffloadHost {
        args: vec![
            crate::native::NativeArg::List(vec!["a".into(), "b".into()]),
            crate::native::NativeArg::Str("not-a-list".into()),
        ],
    };
    assert_eq!(
        host.arg_str_list(0).unwrap(),
        vec!["a".to_string(), "b".to_string()]
    );
    assert!(
        host.arg_str_list(1).is_err(),
        "a non-list NativeArg must error"
    );
    assert!(host.arg_str_list(9).is_err(), "a missing arg must error");
}

/// `extract_native_args` snapshots a `List[str]` Value into `NativeArg::List` (order preserved)
/// so `run_args` can offload; a list with a non-str element reverts to `None` (run inline).
#[test]
fn extract_native_args_snapshots_str_list() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let a = vm.alloc_str("echo".to_string());
    let b = vm.alloc_str("hi".to_string());
    let list = Value::obj(vm.heap.alloc(Obj::List(vec![a, b])));
    let got = vm.extract_native_args(&[list]).expect("str list extracts");
    assert_eq!(
        got,
        vec![crate::native::NativeArg::List(vec![
            "echo".into(),
            "hi".into()
        ])]
    );
    // A list with a non-str element is not snapshottable → None (safe inline fallback).
    let s = vm.alloc_str("x".to_string());
    let bad = Value::obj(vm.heap.alloc(Obj::List(vec![s, Value::int(7)])));
    assert_eq!(vm.extract_native_args(&[bad]), None);
}

/// `VmHost::arg_str_list` reads a live heap `List[str]` in order; a non-list / non-str arg errors.
#[test]
fn vm_host_arg_str_list_reads_live_list() {
    use crate::native::Host;
    let mut vm = Vm::new(Arc::new(empty_program()));
    let a = vm.alloc_str("one".to_string());
    let b = vm.alloc_str("two".to_string());
    let list = Value::obj(vm.heap.alloc(Obj::List(vec![a, b])));
    let s = vm.alloc_str("z".to_string());
    let bad = Value::obj(vm.heap.alloc(Obj::List(vec![s, Value::int(3)])));
    let mut host = VmHost {
        vm: &mut vm,
        args: vec![list, bad, Value::int(9)],
    };
    assert_eq!(
        host.arg_str_list(0).unwrap(),
        vec!["one".to_string(), "two".to_string()]
    );
    assert!(
        host.arg_str_list(1).is_err(),
        "a non-str element must error"
    );
    assert!(host.arg_str_list(2).is_err(), "a non-list arg must error");
}

/// M-C implicit nurseries: a bare `spawn` at function scope (no explicit `parallel:`) joins at
/// the function's end — inline statements after the spawn run first, then the spawned body.
#[test]
fn implicit_nursery_basic_vm() {
    let src = "fn w():\n    print(\"w\")\nfn main():\n    print(\"a\")\n    spawn w()\n    print(\"b\")\nmain()\n";
    assert_eq!(run(src), "a\nb\nw\n");
    assert_eq!(run_capture(src).expect("parallel"), "a\nb\nw\n");
}

/// M-C: `return <value>` is a JOIN point — pending spawned tasks run to completion, THEN the
/// value returns. This is the regression guard for the cancel→join inversion.
#[test]
fn implicit_nursery_return_joins_vm() {
    let src = "fn w(n: int):\n    print(\"w{n}\")\nfn f() -> int:\n    spawn w(1)\n    spawn w(2)\n    print(\"x\")\n    return 0\nfn main():\n    print(f())\nmain()\n";
    assert_eq!(run(src), "x\nw1\nw2\n0\n");
    assert_eq!(run_capture(src).expect("parallel"), "x\nw1\nw2\n0\n");
}

/// M-C: the module top level is an implicit nursery that joins at program exit.
#[test]
fn implicit_nursery_toplevel_vm() {
    let src = "fn w():\n    print(\"w\")\nprint(\"end\")\nspawn w()\n";
    assert_eq!(run(src), "end\nw\n");
    assert_eq!(run_capture(src).expect("parallel"), "end\nw\n");
}

/// M-C: spawned tasks JOIN before the frame's `defer`s run (tasks complete, then cleanup).
#[test]
fn implicit_nursery_defer_orders_tasks_then_defers() {
    let src = "fn w():\n    print(\"task\")\nfn cleanup():\n    print(\"cleanup\")\nfn main():\n    defer cleanup()\n    spawn w()\n    print(\"body\")\nmain()\n";
    assert_mc_parity(src, "body\ntask\ncleanup\n");
}

/// A spawned task that FAULTS (panic propagating to the nursery join)
/// must preserve the stdout it buffered BEFORE the fault, at its task-order slot, on the default
/// M:N engine. Pre-fix the M:N engine dropped the
/// task's partial output (the `Fault` outcome carried no buffered output); it now flushes the
/// terminal (lowest-index propagating) fault's buffer before the fault propagates to the outer
/// `recover:`.
///
/// The nursery here has EXACTLY ONE task, deliberately. That is the only configuration in which
/// fault-output is DETERMINISTIC: the M:N engine runs siblings concurrently and buffers/flushes
/// per-task — so as soon as a nursery has a second output-producing sibling, whether that sibling
/// reaches `Done` (output kept) or observes the faulter's cancel-trip first and becomes `Cancelled`
/// (output dropped) is a genuine scheduler race the buffer-and-flush model cannot reconcile
/// deterministically. So a multi-task-with-fault case is intentionally
/// NOT asserted as byte-identical (it would flake); see the residual-race note in `reduce_task_slots`.
#[test]
fn parallel_faulting_task_flushes_partial_output_3engine() {
    let src = "fn bad():\n    print(\"SOLO-PARTIAL\")\n    panic(\"boom\")\n\
                   fn main():\n\
                   \x20   r := recover:\n\
                   \x20       parallel:\n\
                   \x20           spawn bad()\n\
                   \x20   match r:\n\
                   \x20       ?v: print(\"ok\")\n\
                   \x20       !e: print(\"caught {e.message()}\")\n\
                   main()\n";
    assert_mc_parity(src, "SOLO-PARTIAL\ncaught boom\n");
}

/// Deadlock-abort output-flush: when a `parallel:` nursery is aborted by the M:N
/// scheduler's deadlock detector, a still-PARKED task's ALREADY-BUFFERED stdout must be
/// preserved at its task-order slot (matching what the since-removed serial engine did). Pre-fix
/// `flag_deadlock` wrote each parked
/// fiber's `Fault` slot with an EMPTY buffer (`out: String::new()`), so the parked consumer's
/// three buffered lines were silently discarded on M:N (the since-removed serial engine printed
/// them live, so it kept them).
///
/// This repro is order-DETERMINISTIC: the consumer is the SOLE printer. The producer's buffered
/// `send(1)` deterministically satisfies the consumer's first `recv()` (→ `1`), so the consumer
/// always prints exactly the three lines before it parks forever on the second `recv()` and the
/// deadlock detector fires. It must fault with `DEADLOCK_MSG` and emit the three lines.
/// Looped to catch any scheduler-interleaving flakiness (there must be none — content is fixed).
#[test]
fn parallel_nursery_deadlock_flushes_parked_stdout_2engine() {
    let src = r#"fn producer(ch: Channel[int]):
    ch.send(1)
fn consumer(ch: Channel[int]):
    print("LINE-A")
    print("got {ch.recv()}")
    print("blocking now")
    x := ch.recv()
    print("never")
fn main():
    ch := Channel[int]()
    parallel:
        spawn producer(ch)
        spawn consumer(ch)
main()
"#;
    for _ in 0..50 {
        assert_fault_parity(src, "LINE-A\ngot 1\nblocking now\n");
    }
}

/// Deadlock-abort MULTI-PARKED output-flush: when a `parallel:` nursery deadlocks with TWO
/// OR MORE parked fibers, EVERY parked fiber's already-buffered stdout must be flushed at its
/// task-order slot (matching what the since-removed serial engine did, which printed those lines
/// live). Pre-fix `reduce_task_slots`
/// flushed only the LOWEST-index propagating fault's buffer (the `first_fault.is_none()` guard), so
/// a HIGHER-index parked printer whose LOWER-index sibling parks silently had its output silently
/// dropped on M:N (the silent lower-index fiber's empty buffer won `first_fault`).
///
/// This repro is order-DETERMINISTIC: `silent` (task_index 0) prints NOTHING, so `printer`
/// (task_index 1) is the sole printer and its single line is fixed. Both fibers park forever on an
/// empty channel no sibling can fill → the deadlock detector fires. It must fault with
/// `DEADLOCK_MSG` and emit the single line. Looped to catch scheduler-interleaving flakiness.
#[test]
fn parallel_nursery_deadlock_multiparked_flushes_higher_index_2engine() {
    let src = r#"fn silent(ch: Channel[int]):
    x := ch.recv()
fn printer(ch: Channel[int]):
    print("HI-FROM-PRINTER")
    x := ch.recv()
fn main():
    ch := Channel[int]()
    parallel:
        spawn silent(ch)
        spawn printer(ch)
main()
"#;
    for _ in 0..50 {
        assert_fault_parity(src, "HI-FROM-PRINTER\n");
    }
}

/// Deadlock-abort THREE-PARKED multi-printer SET: two printers with DISJOINT single lines +
/// one silent fiber all park and deadlock. Must emit the same SET of lines
/// {SET-LINE-1, SET-LINE-2}. Asserted order-INSENSITIVELY (`assert_same_lines`): with two fibers
/// each printing before they park the interleaving is genuinely nondeterministic under M:N, so an
/// exact-order assert would flake — the disjoint single lines keep the SET
/// fixed. Looped to catch flakiness.
#[test]
fn parallel_nursery_deadlock_multiparked_multiprinter_set_3parked() {
    let src = r#"fn silent(ch: Channel[int]):
    x := ch.recv()
fn p1(ch: Channel[int]):
    print("SET-LINE-1")
    x := ch.recv()
fn p2(ch: Channel[int]):
    print("SET-LINE-2")
    x := ch.recv()
fn main():
    ch := Channel[int]()
    parallel:
        spawn silent(ch)
        spawn p1(ch)
        spawn p2(ch)
main()
"#;
    for _ in 0..50 {
        assert_fault_same_lines(src, "SET-LINE-1\nSET-LINE-2\n");
    }
}

/// Phase 5a-containers REGRESSION GUARD: the List/Map/Set method-surface migration to file-backed
/// `native struct` decls in std/prelude.chz is CHECKER-ONLY — the literals/ctors still lower to the
/// native build opcodes and runtime method dispatch is by name (untouched). This drives a
/// representative call of EVERY builtin List/Map/Set method (append/pop/contains/index_of/concat/
/// extend/sum/reverse + residual map/filter/fold/sort; Map has/get/keys/values/remove/merge/update/
/// len; Set add/has/remove/union/intersection/difference/len) and asserts byte-identical output
/// against a literal golden. A diverged harvested sig or a
/// runtime regression would change this output.
#[test]
fn container_methods_3engine_parity() {
    let src = include_str!("../../../examples/container_methods.chz");
    let expected = include_str!("../../../examples/container_methods.expected");
    assert_mc_parity(src, expected);
}

/// Phase 6 BEHAVIOR-PRESERVING GUARD: the List HOFs `map`/`filter`/`fold`/`sort_by`/`sort_by_key`
/// are now file-backed `native fn` decls (routed through the generic solver's closure-return
/// loop-back) instead of the bespoke `infer_list_hof` arm. That migration is CHECKER-ONLY — runtime
/// dispatch stays name-keyed and type-erased. This drives typed + UNANNOTATED closures, a chained
/// map→filter→fold, a nested map, and in-place sort_by/sort_by_key, asserting byte-identical output
/// against a literal golden.
#[test]
fn list_hof_3engine_parity() {
    let src = "fn main():\n\
            \x20   a := [1, 2, 3].map(fn(x: int) -> int: x * 2)\n\
            \x20   print(a)\n\
            \x20   b := [1, 2, 3].map(fn(x): x + 1)\n\
            \x20   print(b)\n\
            \x20   c := [1, 2, 3, 4].filter(fn(x): x % 2 == 0)\n\
            \x20   print(c)\n\
            \x20   s := [1, 2, 3].fold(0, fn(a, x): a + x)\n\
            \x20   print(s)\n\
            \x20   chained := [1, 2, 3].map(fn(x): x * 2).filter(fn(x): x > 2).fold(0, fn(a, x): a + x)\n\
            \x20   print(chained)\n\
            \x20   nested := [1, 2].map(fn(x): [x, x])\n\
            \x20   print(nested)\n\
            \x20   ss := [3, 1, 2]\n\
            \x20   ss.sort_by(fn(a, b): a - b)\n\
            \x20   print(ss)\n\
            \x20   sk := [3, 1, 2]\n\
            \x20   sk.sort_by_key(fn(x): -x)\n\
            \x20   print(sk)\n\
            main()\n";
    let expected = "[2, 4, 6]\n[2, 3, 4]\n[2, 4]\n6\n10\n[[1, 1], [2, 2]]\n[1, 2, 3]\n[3, 2, 1]\n";
    assert_mc_parity(src, expected);
}

/// Phase 5c-protocols BEHAVIOR-PRESERVING GUARD: all 18 reserved-protocol SHAPES are now file-backed
/// in std/prelude.chz, but conformance (`satisfies`/`iter_elem`) + operator binding stay Rust-wired
/// and untouched. This drives int/float INTRINSIC arithmetic, a user 4-op struct under `+ - * /` AND
/// through `[T: Arithmetic]`, `[T: Comparable]` max over a Comparable struct, a user `Iterator` struct
/// in a `for`, builtin Index/Slice, and a user IndexSet struct (`[]` get + set) — asserting
/// byte-identical output against a literal golden. A
/// diverged protocol shape or a conformance/operator regression would change this output.
#[test]
fn protocols_5c_3engine_parity() {
    let src = include_str!("../../../examples/protocols_5c.chz");
    let expected = include_str!("../../../examples/protocols_5c.expected");
    assert_mc_parity(src, expected);
}

/// Phase 4c-concurrency REGRESSION GUARD: the std.concurrency migration to a file-backed
/// std/concurrency.chz is CHECKER-ONLY — the ctors still lower to `Op::NewShared`/etc by name and
/// runtime dispatch is untouched. This drives all four primitives (Shared set/update, RwShared
/// write/read closure-recovery, Atomic add/cas, Executor submit/shutdown) PLUS a `parallel:` nursery
/// that increments a `Shared` counter, and asserts byte-identical output against a literal golden.
/// Green before AND after the migration.
#[test]
fn concurrency_file_backed_three_engine() {
    let src = "import std.concurrency\n\
                   fn bump(s: Shared[int]):\n    s.update(fn(x): x + 1)\n\
                   fn job(id: int):\n    print(\"exec {id}\")\n\
                   fn main():\n\
                   \x20   s := Shared(0)\n\
                   \x20   s.set(10)\n\
                   \x20   parallel:\n        spawn bump(s)\n        spawn bump(s)\n        spawn bump(s)\n\
                   \x20   print(s.get())\n\
                   \x20   r := RwShared(5)\n\
                   \x20   r.write(fn(x): x * 2)\n\
                   \x20   print(r.read(fn(x): x + 1))\n\
                   \x20   a := Atomic(0)\n\
                   \x20   a.add(7)\n\
                   \x20   print(a.load())\n\
                   \x20   print(a.cas(7, 8))\n\
                   \x20   print(a.load())\n\
                   \x20   ex := Executor()\n\
                   \x20   ex.submit(fn(): job(1))\n\
                   \x20   ex.submit(fn(): job(2))\n\
                   \x20   ex.shutdown()\n\
                   \x20   print(\"done\")\n\
                   main()\n";
    assert_mc_parity(src, "13\n11\n7\ntrue\n8\nexec 1\nexec 2\ndone\n");
}

/// M-C: a `?` early-return is a JOIN point — pending tasks run before the error propagates.
#[test]
fn implicit_nursery_try_joins_before_propagating() {
    let src = "fn w():\n    print(\"task ran\")\nfn g() -> int!:\n    return !\"inner\"\nfn f() -> int!:\n    spawn w()\n    x := g()?\n    print(\"unreached\")\n    return ?x\nfn main():\n    r := recover:\n        f()?\n        0\n    print(\"done\")\nmain()\n";
    assert_mc_parity(src, "task ran\ndone\n");
}

/// M-C function-boundary rule: a task spawned in a callee joins at the callee's end, not the
/// caller's `parallel:` dedent — it cannot outlive the function that spawned it.
#[test]
fn implicit_nursery_respects_function_boundary() {
    let src = "fn task(label: str):\n    print(label)\nfn helper():\n    spawn task(\"helper-task\")\n    print(\"helper body\")\nfn main():\n    parallel:\n        spawn helper()\n    print(\"main after parallel\")\nmain()\n";
    assert_mc_parity(src, "helper body\nhelper-task\nmain after parallel\n");
}

/// M-C: nested functions each have their own implicit nursery — no task leaks across a call.
#[test]
fn implicit_nursery_nested_functions() {
    let src = "fn leaf(id: int):\n    print(\"leaf {id}\")\nfn inner():\n    spawn leaf(1)\n    spawn leaf(2)\n    print(\"inner body\")\nfn main():\n    spawn leaf(3)\n    inner()\n    print(\"main body\")\nmain()\n";
    assert_mc_parity(src, "inner body\nleaf 1\nleaf 2\nmain body\nleaf 3\n");
}

/// M-C regression (review-panel BUG): a `?` early-return from a body with a bare `spawn` must
/// surface the USER's `Err(...)`, not the internal `? propagation` sentinel. The interp join loop
/// previously let the spawned task's `finish_frame` clear the in-flight `?` value.
#[test]
fn implicit_nursery_try_preserves_error_value() {
    let src = "fn w():\n    print(\"task ran\")\nfn g() -> int!:\n    return !\"boom-value\"\nfn f() -> int!:\n    spawn w()\n    x := g()?\n    return ?x\nfn main():\n    r := recover:\n        f()?\n        99\n    print(\"after: {r}\")\nmain()\n";
    assert_mc_parity(src, "task ran\nafter: !boom-value\n");
}

/// M-C regression (review-panel BUG): a bare `spawn` inside a `defer:` block is legal — the
/// deferred block runs in its own frame with its own implicit nursery, joined when the block ends.
/// The VM previously omitted the nursery for deferred-block protos and hit the runtime guard.
#[test]
fn implicit_nursery_spawn_in_defer_block() {
    let src = "fn work(n: int):\n    print(n)\nfn main():\n    defer:\n        spawn work(1)\n    print(\"body\")\nmain()\n";
    assert_mc_parity(src, "body\n1\n");
}

/// M-C: a genuine body fault caught by `recover:` cancels the implicit nursery's tasks and the
/// recover continues — identical to an explicit `parallel:` escape, on all engines.
///
/// §2c1 — the spawned body is SILENT and no report line is expected. The task starts at its
/// `spawn` now, so "it did not run" is a race rather than a fact, and the
/// `"{n} pending task(s) cancelled…"` line is deleted. What is invariant — that an escape cannot be
/// outlived by a started task — is pinned by `an_escaped_nursery_cancels_a_started_task`.
#[test]
fn implicit_nursery_fault_cancels_pending_tasks() {
    let src = "fn w():\n    0\nfn f():\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    r := recover:\n        f()\n        0\n    print(\"recovered\")\nmain()\n";
    assert_mc_parity(src, "recovered\n");
}

/// Like [`assert_fault_parity`] but asserts the order-INSENSITIVE SET of stdout lines matches
/// `expected_lines` (`assert_same_lines`), for a genuinely-racing multi-printer deadlock where the
/// interleaving is nondeterministic under M:N. The run must fault. Goes through the
/// `(stdout, result)` harness (NOT `run_capture`, which drops stdout on `Err`) so the parked
/// buffers are observable.
#[cfg(test)]
fn assert_fault_same_lines(src: &str, expected_lines: &str) {
    let (vm_out, vm_res) = run_program(src);
    assert!(vm_res.is_err(), "VM expected to fault, got {vm_out:?}");
    assert_same_lines(&vm_out, expected_lines);
}

/// Parity gap fix (T1), §2c1 rewrite: an UNCAUGHT body fault reclaims the function's implicit
/// nursery SILENTLY — nothing reaches stdout on either engine, and both fault.
#[test]
fn uncaught_fault_reclaims_implicit_nursery_silently() {
    let src = "fn w():\n    0\nfn boom():\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    boom()\nmain()\n";
    assert_fault_parity(src, "");
}

/// Parity gap fix (T2), §2c1 rewrite: the explicit-`parallel:` form of the same — silent reclaim.
#[test]
fn uncaught_fault_reclaims_explicit_parallel_silently() {
    let src = "fn w():\n    0\nfn main():\n    parallel:\n        spawn w()\n        x := [1]\n        print(x[9])\nmain()\n";
    assert_fault_parity(src, "");
}

/// T3, §2c1 rewrite: TWO stacked implicit nurseries are BOTH reclaimed as the fault unwinds —
/// innermost first, each silently, and the fault still propagates. The old assertion (two report
/// lines, not one combined `2 pending`) went with the report; what survives is that a second,
/// outer nursery on the unwind path neither leaks nor hangs the teardown.
#[test]
fn uncaught_fault_reclaims_each_nursery_separately() {
    let src = "fn w(tag: str):\n    0\nfn boom():\n    spawn w(\"boom\")\n    x := [1]\n    print(x[9])\nfn main():\n    spawn w(\"main\")\n    boom()\nmain()\n";
    assert_fault_parity(src, "");
}

/// T4 guard, §2c1 rewrite: a top-level bare `spawn` followed by an uncaught TOP-LEVEL fault stays
/// silent and does not hang. The spawned body is silent deliberately — under eager
/// start the module nursery's task IS live when the fault lands, so whether it got a slice is a
/// race; what must hold is that the fault propagates and the run terminates.
#[test]
fn uncaught_toplevel_fault_reclaims_module_nursery_silently() {
    let src = "fn w():\n    0\nspawn w()\nx := [1]\nprint(x[9])\n";
    assert_fault_parity(src, "");
}

/// §2c1 rewrite: the recover-CAUGHT twin — two stacked nurseries reclaimed as the fault unwinds,
/// then the recover continues.
#[test]
fn recover_caught_fault_reclaims_each_nursery_separately() {
    let src = "fn w(tag: str):\n    0\nfn boom():\n    spawn w(\"boom\")\n    x := [1]\n    print(x[9])\nfn outer():\n    spawn w(\"outer\")\n    boom()\nfn main():\n    r := recover:\n        outer()\n        0\n    print(\"recovered\")\nmain()\n";
    assert_mc_parity(src, "recovered\n");
}

/// §2c1 rewrite: the faulting frame's `defer` still runs, exactly once, after its nursery is
/// reclaimed. (The report this test was originally written to ORDER against the defer is deleted;
/// the defer half is the part that was ever a behaviour.)
#[test]
fn uncaught_fault_runs_frame_defers_after_reclaim() {
    let src = "fn w():\n    0\nfn cleanup():\n    print(\"cleanup\")\nfn boom():\n    defer cleanup()\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    boom()\nmain()\n";
    assert_fault_parity(src, "cleanup\n");
}

/// The recover-CAUGHT twin: defer, then the recover continues.
#[test]
fn recover_caught_fault_runs_frame_defers_after_reclaim() {
    let src = "fn w():\n    0\nfn cleanup():\n    print(\"cleanup\")\nfn boom():\n    defer cleanup()\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    r := recover:\n        boom()\n        0\n    print(\"recovered\")\nmain()\n";
    assert_mc_parity(src, "cleanup\nrecovered\n");
}

/// Multi-frame interleave: each unwound frame reclaims its nursery and runs its own `defer` before
/// the next (outer) frame — innermost-first. Guards the per-frame interleave in `unwind_deferred`
/// against batching regressions. (§2c1: the report half of the old `report, cleanup, report,
/// cleanup` sequence is gone; the per-frame defer ORDER is what this ever guarded.)
#[test]
fn uncaught_fault_interleaves_reclaim_and_defer_per_frame() {
    let src = "fn w(t: str):\n    0\nfn cl(t: str):\n    print(\"cleanup {t}\")\nfn boom():\n    defer cl(\"boom\")\n    spawn w(\"boom\")\n    x := [1]\n    print(x[9])\nfn outer():\n    defer cl(\"outer\")\n    spawn w(\"outer\")\n    boom()\nfn main():\n    outer()\nmain()\n";
    assert_fault_parity(src, "cleanup boom\ncleanup outer\n");
}

/// M19 SSO — the production `alloc_str` path stores short strings inline (no `Box` heap alloc)
/// and spills longer ones to the heap. This guards the wiring of `ChzStr` into the VM's hot
/// string-construction funnel; `chzstr.rs` unit tests cover the selection logic itself.
#[test]
fn vm_alloc_str_inlines_short_spills_long() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let short = vm.alloc_str("item-499999".to_string()); // 11 bytes ≤ INLINE_CAP
    let long = vm.alloc_str("x".repeat(crate::vm::chzstr::INLINE_CAP + 1)); // > INLINE_CAP
    let inline = matches!(vm.heap.get(short.as_obj().unwrap()), Obj::Str(s) if s.is_inline());
    let heap = matches!(vm.heap.get(long.as_obj().unwrap()), Obj::Str(s) if !s.is_inline());
    assert!(inline, "short string should be stored inline");
    assert!(heap, "long string should spill to the heap");
}
