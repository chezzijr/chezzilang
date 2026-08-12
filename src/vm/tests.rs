// Extracted from vm/mod.rs (test module). `super::` == the `vm` module.
use super::*;

/// Run a program to completion, returning its stdout (panics on runtime error).
fn run(src: &str) -> String {
    run_capture(src).unwrap_or_else(|e| panic!("unexpected runtime error: {e}"))
}

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
/// Identical on the cooperative default and the `--parallel` engine.
#[test]
fn implicit_nursery_basic_vm() {
    let src = "fn w():\n    print(\"w\")\nfn main():\n    print(\"a\")\n    spawn w()\n    print(\"b\")\nmain()\n";
    assert_eq!(run(src), "a\nb\nw\n");
    assert_eq!(run_capture_parallel(src).expect("parallel"), "a\nb\nw\n");
}

/// M-C: `return <value>` is a JOIN point — pending spawned tasks run to completion, THEN the
/// value returns. No cancel-report. This is the regression guard for the cancel→join inversion.
#[test]
fn implicit_nursery_return_joins_vm() {
    let src = "fn w(n: int):\n    print(\"w{n}\")\nfn f() -> int:\n    spawn w(1)\n    spawn w(2)\n    print(\"x\")\n    return 0\nfn main():\n    print(f())\nmain()\n";
    assert_eq!(run(src), "x\nw1\nw2\n0\n");
    assert_eq!(
        run_capture_parallel(src).expect("parallel"),
        "x\nw1\nw2\n0\n"
    );
}

/// M-C: the module top level is an implicit nursery that joins at program exit.
#[test]
fn implicit_nursery_toplevel_vm() {
    let src = "fn w():\n    print(\"w\")\nprint(\"end\")\nspawn w()\n";
    assert_eq!(run(src), "end\nw\n");
    assert_eq!(run_capture_parallel(src).expect("parallel"), "end\nw\n");
}

/// Assert a program yields `expected` on both VM engines (cooperative + M:N `--parallel`) — the
/// parity bar.
#[cfg(test)]
fn assert_mc_parity(src: &str, expected: &str) {
    assert_eq!(run(src), expected, "cooperative VM");
    assert_eq!(
        run_capture_parallel(src).expect("M:N"),
        expected,
        "M:N engine"
    );
}

/// M-C: spawned tasks JOIN before the frame's `defer`s run (tasks complete, then cleanup).
#[test]
fn implicit_nursery_defer_orders_tasks_then_defers() {
    let src = "fn w():\n    print(\"task\")\nfn cleanup():\n    print(\"cleanup\")\nfn main():\n    defer cleanup()\n    spawn w()\n    print(\"body\")\nmain()\n";
    assert_mc_parity(src, "body\ntask\ncleanup\n");
}

/// Fault-output-flush parity: a spawned task that FAULTS (panic propagating to the nursery join)
/// must preserve the stdout it buffered BEFORE the fault, at its task-order slot, on the default
/// M:N engine — matching the cooperative/interp oracle. Pre-fix the M:N engine dropped the
/// task's partial output (the `Fault` outcome carried no buffered output); it now flushes the
/// terminal (lowest-index propagating) fault's buffer before the fault propagates to the outer
/// `recover:`.
///
/// The nursery here has EXACTLY ONE task, deliberately. That is the only configuration in which
/// fault-output is DETERMINISTIC across engines: serial runs tasks in spawn order and stops at
/// the first fault, but the M:N engine runs siblings concurrently and buffers/flushes per-task —
/// so as soon as a nursery has a second output-producing sibling, whether that sibling reaches
/// `Done` (output kept) or observes the faulter's cancel-trip first and becomes `Cancelled`
/// (output dropped) is a genuine scheduler race the buffer-and-flush model cannot reconcile with
/// serial's strict stop-at-first-fault order. So a multi-task-with-fault case is intentionally
/// NOT asserted as parity (it would flake); see the residual-race note in `reduce_task_slots`.
#[test]
fn parallel_faulting_task_flushes_partial_output_3engine() {
    let src = "fn bad():\n    print(\"SOLO-PARTIAL\")\n    panic(\"boom\")\n\
                   fn main():\n\
                   \x20   r := recover:\n\
                   \x20       parallel:\n\
                   \x20           spawn bad()\n\
                   \x20   match r:\n\
                   \x20       Ok(v): print(\"ok\")\n\
                   \x20       Err(e): print(\"caught {e.message()}\")\n\
                   main()\n";
    assert_mc_parity(src, "SOLO-PARTIAL\ncaught boom\n");
}

/// Deadlock-abort output-flush parity: when a `parallel:` nursery is aborted by the M:N
/// scheduler's deadlock detector, a still-PARKED task's ALREADY-BUFFERED stdout must be
/// preserved at its task-order slot — matching serial. Pre-fix `flag_deadlock` wrote each parked
/// fiber's `Fault` slot with an EMPTY buffer (`out: String::new()`), so the parked consumer's
/// three buffered lines were silently discarded on M:N (serial prints them live, so it kept them).
///
/// This repro is order-DETERMINISTIC: the consumer is the SOLE printer. The producer's buffered
/// `send(1)` deterministically satisfies the consumer's first `recv()` (→ `1`), so the consumer
/// always prints exactly the three lines before it parks forever on the second `recv()` and the
/// deadlock detector fires. Both engines fault with `DEADLOCK_MSG`; both must emit the three lines.
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

/// Deadlock-abort MULTI-PARKED output-flush parity: when a `parallel:` nursery deadlocks with TWO
/// OR MORE parked fibers, EVERY parked fiber's already-buffered stdout must be flushed at its
/// task-order slot — matching serial, which printed those lines live. Pre-fix `reduce_task_slots`
/// flushed only the LOWEST-index propagating fault's buffer (the `first_fault.is_none()` guard), so
/// a HIGHER-index parked printer whose LOWER-index sibling parks silently had its output silently
/// dropped on M:N (the silent lower-index fiber's empty buffer won `first_fault`).
///
/// This repro is order-DETERMINISTIC: `silent` (task_index 0) prints NOTHING, so `printer`
/// (task_index 1) is the sole printer and its single line is fixed. Both fibers park forever on an
/// empty channel no sibling can fill → the deadlock detector fires. Both engines fault with
/// `DEADLOCK_MSG`; both must emit the single line. Looped to catch scheduler-interleaving flakiness.
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

/// Deadlock-abort THREE-PARKED multi-printer SET parity: two printers with DISJOINT single lines +
/// one silent fiber all park and deadlock. Both engines must emit the same SET of lines
/// {SET-LINE-1, SET-LINE-2}. Asserted order-INSENSITIVELY (`assert_same_lines`): with two fibers
/// each printing before they park the interleaving is genuinely nondeterministic under M:N, so an
/// exact-order assert would flake (mn-parity discipline) — the disjoint single lines keep the SET
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
        assert_fault_same_lines(src);
    }
}

/// Phase 5a-containers REGRESSION GUARD: the List/Map/Set method-surface migration to file-backed
/// `native struct` decls in std/prelude.chz is CHECKER-ONLY — the literals/ctors still lower to the
/// native build opcodes and runtime method dispatch is by name (untouched). This drives a
/// representative call of EVERY builtin List/Map/Set method (append/pop/contains/index_of/concat/
/// extend/sum/reverse + residual map/filter/fold/sort; Map has/get/keys/values/remove/merge/update/
/// len; Set add/has/remove/union/intersection/difference/len) and asserts byte-identical output on all
/// three engines (cooperative VM / frozen interp / `--parallel`). A diverged harvested sig or a
/// runtime regression would change this output.
#[test]
fn container_methods_3engine_parity() {
    let src = include_str!("../../examples/container_methods.chz");
    let expected = include_str!("../../examples/container_methods.expected");
    assert_mc_parity(src, expected);
}

/// Phase 6 BEHAVIOR-PRESERVING GUARD: the List HOFs `map`/`filter`/`fold`/`sort_by`/`sort_by_key`
/// are now file-backed `native fn` decls (routed through the generic solver's closure-return
/// loop-back) instead of the bespoke `infer_list_hof` arm. That migration is CHECKER-ONLY — runtime
/// dispatch stays name-keyed and type-erased. This drives typed + UNANNOTATED closures, a chained
/// map→filter→fold, a nested map, and in-place sort_by/sort_by_key, asserting byte-identical output
/// on all three engines (cooperative VM / frozen interp / `--parallel`).
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
/// byte-identical output on all three engines (cooperative VM / frozen interp / `--parallel`). A
/// diverged protocol shape or a conformance/operator regression would change this output.
#[test]
fn protocols_5c_3engine_parity() {
    let src = include_str!("../../examples/protocols_5c.chz");
    let expected = include_str!("../../examples/protocols_5c.expected");
    assert_mc_parity(src, expected);
}

/// Phase 4c-concurrency REGRESSION GUARD: the std.concurrency migration to a file-backed
/// std/concurrency.chz is CHECKER-ONLY — the ctors still lower to `Op::NewShared`/etc by name and
/// runtime dispatch is untouched. This drives all four primitives (Shared set/update, RwShared
/// write/read closure-recovery, Atomic add/cas, Executor submit/shutdown) PLUS a `parallel:` nursery
/// that increments a `Shared` counter, and asserts byte-identical output on all three engines
/// (cooperative VM, frozen interp, --parallel). Green before AND after the migration.
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
    let src = "fn w():\n    print(\"task ran\")\nfn g() -> int!:\n    return Err(\"inner\")\nfn f() -> int!:\n    spawn w()\n    x := g()?\n    print(\"unreached\")\n    return Ok(x)\nfn main():\n    r := recover:\n        f()?\n        0\n    print(\"done\")\nmain()\n";
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
    let src = "fn w():\n    print(\"task ran\")\nfn g() -> int!:\n    return Err(\"boom-value\")\nfn f() -> int!:\n    spawn w()\n    x := g()?\n    return Ok(x)\nfn main():\n    r := recover:\n        f()?\n        99\n    print(\"after: {r}\")\nmain()\n";
    assert_mc_parity(src, "task ran\nafter: Err('boom-value')\n");
}

/// M-C regression (review-panel BUG): a bare `spawn` inside a `defer:` block is legal — the
/// deferred block runs in its own frame with its own implicit nursery, joined when the block ends.
/// The VM previously omitted the nursery for deferred-block protos and hit the runtime guard.
#[test]
fn implicit_nursery_spawn_in_defer_block() {
    let src = "fn work(n: int):\n    print(n)\nfn main():\n    defer:\n        spawn work(1)\n    print(\"body\")\nmain()\n";
    assert_mc_parity(src, "body\n1\n");
}

/// M-C: a genuine body fault caught by `recover:` cancels-and-reports the implicit nursery's
/// unstarted tasks (they do NOT run) — identical to an explicit `parallel:` escape, on all engines.
#[test]
fn implicit_nursery_fault_cancels_pending_tasks() {
    let src = "fn w():\n    print(\"should not run\")\nfn f():\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    r := recover:\n        f()\n        0\n    print(\"recovered\")\nmain()\n";
    assert_mc_parity(
        src,
        "1 pending task(s) cancelled on early exit from parallel:\nrecovered\n",
    );
}

/// Assert an UNCAUGHT fault yields identical stdout on the cooperative VM and the frozen interp,
/// and that both actually faulted. `run_capture` drops stdout on `Err`, so go through the
/// `(stdout, result)` harness directly. This is the cancel-report parity bar for uncaught faults.
#[cfg(test)]
fn assert_fault_parity(src: &str, expected_out: &str) {
    let (vm_out, vm_res) = run_program(src);
    assert!(vm_res.is_err(), "VM expected to fault, got {vm_out:?}");
    assert_eq!(vm_out, expected_out, "cooperative VM stdout");
    let (it_out, it_res) = run_program_parallel(src);
    assert!(it_res.is_err(), "interp expected to fault, got {it_out:?}");
    assert_eq!(it_out, expected_out, "interp stdout");
    assert_eq!(vm_out, it_out, "VM/interp cancel-report divergence");
}

/// Like [`assert_fault_parity`] but asserts the order-INSENSITIVE SET of stdout lines matches
/// (`assert_same_lines`), for a genuinely-racing multi-printer deadlock where the interleaving is
/// nondeterministic under M:N. Both engines must fault. Goes through the `(stdout, result)` harness
/// (NOT `run_capture_parallel`, which drops stdout on `Err`) so the parked buffers are observable.
#[cfg(test)]
fn assert_fault_same_lines(src: &str) {
    let (vm_out, vm_res) = run_program(src);
    assert!(vm_res.is_err(), "VM expected to fault, got {vm_out:?}");
    let (mn_out, mn_res) = run_program_parallel(src);
    assert!(mn_res.is_err(), "M:N expected to fault, got {mn_out:?}");
    assert_same_lines(&vm_out, &mn_out);
}

/// Parity gap fix (T1): an UNCAUGHT body fault with one un-run task on the function's implicit
/// nursery reports the cancellation on stdout — previously only the interp printed it.
#[test]
fn uncaught_fault_reports_implicit_nursery() {
    let src = "fn w():\n    print(\"should not run\")\nfn boom():\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    boom()\nmain()\n";
    assert_fault_parity(
        src,
        "1 pending task(s) cancelled on early exit from parallel:\n",
    );
}

/// Parity gap fix (T2): an UNCAUGHT fault inside an explicit `parallel:` block reports its
/// un-run task on stdout (the pre-M-C form of the same gap).
#[test]
fn uncaught_fault_reports_explicit_parallel() {
    let src = "fn w():\n    print(\"should not run\")\nfn main():\n    parallel:\n        spawn w()\n        x := [1]\n        print(x[9])\nmain()\n";
    assert_fault_parity(
        src,
        "1 pending task(s) cancelled on early exit from parallel:\n",
    );
}

/// Parity gap fix (T3): TWO stacked implicit nurseries each with a pending task report
/// PER-NURSERY (two lines, innermost first) — matching the interp's per-frame reporting, not one
/// combined line. Guards the `drain_escaped_nursery` sum→per-line change.
#[test]
fn uncaught_fault_reports_each_nursery_separately() {
    let src = "fn w(tag: str):\n    print(\"ran {tag}\")\nfn boom():\n    spawn w(\"boom\")\n    x := [1]\n    print(x[9])\nfn main():\n    spawn w(\"main\")\n    boom()\nmain()\n";
    let line = "1 pending task(s) cancelled on early exit from parallel:\n";
    assert_fault_parity(src, &format!("{line}{line}"));
}

/// Parity gap fix (T4 guard): a top-level bare `spawn` followed by an uncaught TOP-LEVEL fault
/// stays SILENT on both engines — the module nursery is not reported (it joins only at clean
/// exit). The fix must preserve this (don't drain the toplevel frame's own implicit nursery).
#[test]
fn uncaught_toplevel_fault_does_not_report_module_nursery() {
    let src = "fn w():\n    print(\"ran top\")\nspawn w()\nx := [1]\nprint(x[9])\n";
    assert_fault_parity(src, "");
}

/// Parity gap fix: a recover-CAUGHT fault unwinding two stacked nurseries also reports
/// PER-NURSERY (two lines), then the recover continues — previously the VM combined them into
/// one `2 pending` line while the interp emitted two.
#[test]
fn recover_caught_fault_reports_each_nursery_separately() {
    let src = "fn w(tag: str):\n    print(\"ran {tag}\")\nfn boom():\n    spawn w(\"boom\")\n    x := [1]\n    print(x[9])\nfn outer():\n    spawn w(\"outer\")\n    boom()\nfn main():\n    r := recover:\n        outer()\n        0\n    print(\"recovered\")\nmain()\n";
    let line = "1 pending task(s) cancelled on early exit from parallel:\n";
    assert_mc_parity(src, &format!("{line}{line}recovered\n"));
}

/// Parity gap fix (review-panel BUG, ordering): the cancel-report is emitted BEFORE the faulting
/// frame's `defer`s run — matching the interp (`leave_implicit_nursery` reports, then
/// `finish_frame` runs defers). The VM previously ran defers in `unwind_deferred` FIRST and only
/// reported afterward (`cleanup` then report — a divergence the no-defer tests above missed).
#[test]
fn uncaught_fault_reports_before_frame_defers() {
    let src = "fn w():\n    print(\"task\")\nfn cleanup():\n    print(\"cleanup\")\nfn boom():\n    defer cleanup()\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    boom()\nmain()\n";
    assert_fault_parity(
        src,
        "1 pending task(s) cancelled on early exit from parallel:\ncleanup\n",
    );
}

/// Same report-before-defer ordering on the recover-CAUGHT path, then the recover continues.
#[test]
fn recover_caught_fault_reports_before_frame_defers() {
    let src = "fn w():\n    print(\"task\")\nfn cleanup():\n    print(\"cleanup\")\nfn boom():\n    defer cleanup()\n    spawn w()\n    x := [1]\n    print(x[9])\nfn main():\n    r := recover:\n        boom()\n        0\n    print(\"recovered\")\nmain()\n";
    assert_mc_parity(
        src,
        "1 pending task(s) cancelled on early exit from parallel:\ncleanup\nrecovered\n",
    );
}

/// Multi-frame interleave: each unwound frame reports its nursery, THEN runs its defer, before
/// the next (outer) frame — innermost-first (`report boom, cleanup boom, report outer, cleanup
/// outer`). Guards the per-frame interleave in `unwind_deferred` against batching regressions.
#[test]
fn uncaught_fault_interleaves_report_and_defer_per_frame() {
    let src = "fn w(t: str):\n    print(\"task {t}\")\nfn cl(t: str):\n    print(\"cleanup {t}\")\nfn boom():\n    defer cl(\"boom\")\n    spawn w(\"boom\")\n    x := [1]\n    print(x[9])\nfn outer():\n    defer cl(\"outer\")\n    spawn w(\"outer\")\n    boom()\nfn main():\n    outer()\nmain()\n";
    let line = "1 pending task(s) cancelled on early exit from parallel:\n";
    assert_fault_parity(src, &format!("{line}cleanup boom\n{line}cleanup outer\n"));
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

/// Run a program expected to fail; return the runtime error message.
fn run_err(src: &str) -> String {
    match run_capture(src) {
        Ok(out) => panic!("expected a runtime error, got output: {out:?}"),
        Err(e) => e.message,
    }
}

// ---- gaps.md §2 wave-1: List value/ergonomics methods ----

/// `remove_at` on a true out-of-range index faults byte-identically on both engines.
#[test]
fn remove_at_oob_faults_both_engines() {
    let src = "xs := [1, 2, 3]\nprint(\"before\")\nxs.remove_at(9)\n";
    assert_fault_parity(src, "before\n");
    assert_eq!(run_err(src), "index 9 out of bounds (len 3)");
}

/// `min`/`max` on an empty list faults byte-identically on both engines.
#[test]
fn min_max_empty_faults() {
    let src = "xs: List[int] = []\nprint(\"before\")\nxs.min()\n";
    assert_fault_parity(src, "before\n");
    assert_eq!(run_err(src), "min() of empty list");
    let src2 = "xs: List[int] = []\nprint(\"before\")\nxs.max()\n";
    assert_fault_parity(src2, "before\n");
    assert_eq!(run_err(src2), "max() of empty list");
}

/// `min_by`/`max_by` take a key extractor and return the ELEMENT with the extremal key (first-seen
/// tie); empty faults on both engines.
#[test]
fn list_min_max_by_parity() {
    let src = "struct P:\n    k: int\n    tag: str\n\
               xs := [P(2, \"a\"), P(1, \"b\"), P(1, \"c\"), P(3, \"d\")]\n\
               fn key(p: P) -> int:\n    return p.k\n\
               lo := xs.min_by(key)\n\
               print(lo.tag)\n\
               hi := xs.max_by(key)\n\
               print(hi.tag)\n";
    assert_mc_parity(src, "b\nd\n");
}

#[test]
fn min_max_by_empty_faults() {
    let src = "xs: List[int] = []\nfn key(x: int) -> int:\n    return x\nprint(\"before\")\nxs.min_by(key)\n";
    assert_fault_parity(src, "before\n");
    assert_eq!(run_err(src), "min_by() of empty list");
}

/// `min`/`max` scan a SNAPSHOT: a Comparable-struct element's `compare` that SHRINKS the receiver
/// mid-scan must not index past the live list's new length (no OOB panic), matching `sort`/`min_by`.
/// Regression for `list_reduce_extreme` re-indexing the live source after user code mutated it.
#[test]
fn min_max_shrinking_comparator_no_panic() {
    // `compare` shrinks the module-global list it is invoked on (the receiver) each comparison; the
    // snapshot scan still visits all three original elements → min by x = 1, on both engines.
    let min_src = "struct Point:\n    x: int\n    fn compare(self, other: Point) -> int:\n        pts.remove_at(0)\n        return self.x - other.x\n    fn eq(self, other: Point) -> bool:\n        return self.x == other.x\n\
               pts: List[Point] = [Point(3), Point(1), Point(2)]\n\
               print(pts.min().x)\n";
    assert_mc_parity(min_src, "1\n");
    // Same for `max` (same `list_reduce_extreme` scan, is_max flipped) → max by x = 3.
    let max_src = "struct Q:\n    x: int\n    fn compare(self, other: Q) -> int:\n        qs.remove_at(0)\n        return self.x - other.x\n    fn eq(self, other: Q) -> bool:\n        return self.x == other.x\n\
               qs: List[Q] = [Q(3), Q(1), Q(2)]\n\
               print(qs.max().x)\n";
    assert_mc_parity(max_src, "3\n");
}

// ---- gaps.md §2 wave-2: List iter-ergonomics (unique/dedup/chunk/windows/take_while/drop_while/count/position) ----

/// `chunk`/`windows` with `n <= 0` fault byte-identically on both engines with a clear message.
#[test]
fn list_chunk_windows_bad_n_faults() {
    let c = "xs := [1, 2]\nprint(\"before\")\nxs.chunk(0)\n";
    assert_fault_parity(c, "before\n");
    assert_eq!(run_err(c), "chunk size must be positive, got 0");
    let w = "xs := [1, 2]\nprint(\"before\")\nxs.windows(-1)\n";
    assert_fault_parity(w, "before\n");
    assert_eq!(run_err(w), "window size must be positive, got -1");
}

/// The 4 predicate methods scan a SNAPSHOT: a predicate that SHRINKS the receiver mid-scan must not
/// index past the live list's new length (no OOB panic), matching `map`/`filter`/`min`. The predicate
/// sees the full original-length snapshot.
#[test]
fn list_predicate_shrinking_no_panic() {
    // count: pred pops the receiver on the first element; snapshot still visits all 4 → all x>0 → 4.
    let c = "xs := [1, 2, 3, 4]\nfn f(x: int) -> bool:\n    if x == 1:\n        xs.pop()\n    return x > 0\nprint(xs.count(f))\n";
    assert_mc_parity(c, "4\n");
    // position: pred shrinks the receiver; snapshot scan finds index 2 without OOB.
    let p = "xs := [1, 2, 3, 4]\nfn f(x: int) -> bool:\n    xs.pop()\n    return x == 3\nfn showpos(o: Option[int]) -> int:\n    match o:\n        Some(i): return i\n        None: return -1\nprint(showpos(xs.position(f)))\n";
    assert_mc_parity(p, "2\n");
    // take_while: pred shrinks the receiver; snapshot scan is unaffected.
    let t = "xs := [1, 2, 3, 4]\nfn f(x: int) -> bool:\n    xs.pop()\n    return x < 3\nprint(xs.take_while(f))\n";
    assert_mc_parity(t, "[1, 2]\n");
}

// ---- list HOF: callback that shrinks the receiver (snapshot semantics) ----

/// `map` iterates a SNAPSHOT of the receiver's elements at call time: a callback that pops the
/// receiver mid-iteration must not perturb the iteration sequence (no OOB panic), matching the
/// interpreter, comprehensions, and Python `map`. Regression for the OOB at list_hof's index.
#[test]
fn map_shrinking_callback_no_panic() {
    let src = "xs := [1, 2, 3, 4, 5]\nfn f(x: int) -> int:\n    xs.pop()\n    return x * 2\nys := xs.map(f)\nprint(ys)\n";
    assert_eq!(run(src), "[2, 4, 6, 8, 10]\n");
}

/// `filter` over a snapshot: a predicate that shrinks the receiver still tests every original
/// element. Original 1..5 → evens kept → `[2, 4]`.
#[test]
fn filter_shrinking_callback_no_panic() {
    let src = "xs := [1, 2, 3, 4, 5]\nfn p(x: int) -> bool:\n    xs.pop()\n    return x % 2 == 0\nprint(xs.filter(p))\n";
    assert_eq!(run(src), "[2, 4]\n");
}

/// `fold` over a snapshot: a callback that shrinks the receiver still folds all original
/// elements. Sum of 1..5 = 15.
#[test]
fn fold_shrinking_callback_no_panic() {
    let src = "xs := [1, 2, 3, 4, 5]\nfn g(acc: int, x: int) -> int:\n    xs.pop()\n    return acc + x\nprint(xs.fold(0, g))\n";
    assert_eq!(run(src), "15\n");
}

// ---- assert (Phase A) ----

#[test]
fn assert_true_does_not_fault() {
    assert_eq!(run("assert true\nprint(\"ok\")\n"), "ok\n");
    assert_eq!(run("assert 1 == 1, \"never\"\nprint(\"ok\")\n"), "ok\n");
}

#[test]
fn assert_false_faults_with_custom_msg_and_line() {
    // The assert is on line 2; the fault carries that line and the FORMATTED custom message
    // (`assertion failed: <msg>`), giving the failure context Python's `assert` does.
    let err = run_capture("print(\"a\")\nassert false, \"boom\"\n").unwrap_err();
    assert_eq!(err.message, "assertion failed: boom");
    assert_eq!(err.span.line, 2);
}

#[test]
fn assert_true_with_msg_does_not_evaluate_msg() {
    // A passing assert must NOT evaluate its message expression (laziness): the program runs on.
    assert_eq!(run("assert true, \"never\"\nprint(\"ok\")\n"), "ok\n");
}

#[test]
fn assert_false_default_message() {
    let err = run_capture("assert false\n").unwrap_err();
    assert_eq!(err.message, "assertion failed");
    assert_eq!(err.span.line, 1);
}

// ---- M19 Phase 3: ConstStr interning + per-char alloc (correctness guards) ----

#[test]
fn interned_literal_repeated_pushes_render_identically() {
    // The same literal op pushed many times must render identically — interning must not change
    // the observed value (no identity operator exists, so aliasing is invisible).
    assert_eq!(
        run("i := 0\nwhile i < 3:\n    print(\"hi\")\n    i = i + 1\n"),
        "hi\nhi\nhi\n"
    );
}

#[test]
fn interned_fstring_literal_parts_in_loop() {
    // Interpolation literal chunks (`n=` / `!`) are ConstStr pushes repeated per iteration.
    assert_eq!(
        run("i := 0\nwhile i < 3:\n    print(\"n={i}!\")\n    i = i + 1\n"),
        "n=0!\nn=1!\nn=2!\n"
    );
}

#[test]
fn interned_literal_as_map_key_repeated() {
    // A literal reused as a map key: aliasing must preserve structural (by-content) map lookup.
    assert_eq!(
        run("m := {}\ni := 0\nwhile i < 3:\n    m[\"k\"] = i\n    i = i + 1\nprint(m[\"k\"])\n"),
        "2\n"
    );
}

#[test]
fn interned_strings_survive_gc_stress() {
    // Proves interned ConstStr objects are GC-rooted: collect-before-every-instruction must not
    // sweep a cached literal out from under a later push of the same op.
    let src = "i := 0\nout := \"\"\nwhile i < 50:\n    out = out + \"x\"\n    i = i + 1\nprint(out.len())\n";
    assert_eq!(run_capture_stress(src), run(src));
    assert_eq!(run(src), "50\n");
}

#[test]
fn per_char_sites_render_unchanged() {
    // `for c in str`, string indexing, `chars()`, and `chr()` all build 1-char strs via the
    // single-allocation helper — output must stay byte-identical (same UTF-8).
    assert_eq!(
        run("for c in \"héllo\":\n    print(c)\n"),
        "h\né\nl\nl\no\n"
    );
    assert_eq!(run("s := \"héllo\"\nprint(s[1])\n"), "é\n");
    assert_eq!(
        run("for c in \"abc\".chars():\n    print(c)\n"),
        "a\nb\nc\n"
    );
    assert_eq!(run("print(chr(233))\n"), "é\n");
}

// ---- M19: FxHash map/set index hasher (correctness guards) ----
// The map/set `index` (cached-hash → positions) and `str_intern` swap SipHash for a cheap FxHash.
// The hasher only picks buckets; `values_equal` confirms every probe, so behavior must not change.

#[test]
fn fxhash_map_int_keys_insert_lookup_remove() {
    // Int keys hash straight to f64 bits, so this exercises only the index BuildHasher. Insert,
    // read, remove (rebuilds the index), then re-insert — all must agree with the interpreter.
    let src = "m := {}\ni := 0\nwhile i < 50:\n    m[i] = i * 2\n    i = i + 1\n\
                   m.remove(10)\nm.remove(20)\nm[10] = 999\n\
                   acc := 0\nfor k in m:\n    acc = acc + m[k]\nprint(acc)\nprint(m.len())\n";
    // sum(2i, i in 0..50) = 2450; drop 20→-40, drop10 then re-add 10→999: 2450-20-40+999 = 3389
    assert_eq!(run_parity(src), "3389\n49\n");
}

#[test]
fn fxhash_map_str_keys() {
    // String keys hash by content (DefaultHasher, unchanged) then route through the index
    // BuildHasher (changed). Repeated-key updates must still land in the same entry.
    let src = concat!(
        "counts := {}\n",
        "for w in [\"a\", \"b\", \"a\", \"c\", \"a\", \"b\"]:\n",
        "    if counts.has(w):\n",
        "        counts[w] = counts[w] + 1\n",
        "    else:\n",
        "        counts[w] = 1\n",
        "for k in counts:\n",
        "    print(\"{k}={counts[k]}\")\n",
    );
    assert_eq!(run_parity(src), "a=3\nb=2\nc=1\n");
}

#[test]
fn fxhash_constant_hash_collision_still_resolves() {
    // A struct key whose hash() is constant forces every key into ONE index bucket. The probe
    // must still find the right entry via structural ==, regardless of the bucket hasher.
    let src = "struct K:\n    v: int\n    fn hash(self) -> int:\n        return 7\n\
                   m := {}\ni := 0\nwhile i < 30:\n    m[K(i)] = i\n    i = i + 1\n\
                   print(m[K(7)])\nprint(m[K(29)])\nprint(m.has(K(30)))\nprint(m.len())\n";
    assert_eq!(run_parity(src), "7\n29\nfalse\n30\n");
}

#[test]
fn dense_index_collision_upgrade_parity() {
    // Two DISTINCT struct keys whose hash() is the same constant land in ONE index bucket. The
    // dense index must UPGRADE that bucket from a single inline position to hold BOTH positions
    // (the `Many` path) so each key reads back distinctly, and an absent third constant-hash key
    // still misses. A One-only / single-slot index would drop the second key and diverge.
    let src = "struct K:\n    v: int\n    fn hash(self) -> int:\n        return 42\n\
                   m := {}\nm[K(1)] = \"one\"\nm[K(2)] = \"two\"\n\
                   print(m[K(1)])\nprint(m[K(2)])\nprint(m.has(K(3)))\nprint(m.len())\n";
    assert_eq!(run_parity(src), "one\ntwo\nfalse\n2\n");
}

#[test]
fn fxhash_set_dedup_and_ops() {
    // Set dedup + union/intersection/difference over the index hasher.
    let src = "a := Set([1, 2, 3, 2, 1])\nb := Set([3, 4, 5])\n\
                   print(a.len())\nprint(a.union(b).len())\nprint(a.intersection(b).len())\nprint(a.difference(b).len())\n";
    assert_eq!(run_parity(src), "3\n5\n1\n2\n");
}

// ---- M19 Tier-2: index-access specialization (behavior-preserving guards) ----
// The Int-key fast path in `get_index`/`set_index` (skips the rooting that protects a struct
// key's re-entrant hash) and the inline `GetIndex`/`SetIndex` dispatch are VM-only speedups, so
// every result + error string must stay byte-identical to the frozen interpreter. `idx_parity`
// compares the full `Result` outcome (stdout OR error message). These pin the contract BEFORE the
// change and stay green AFTER.
fn idx_parity(src: &str) {
    let vm = run_capture(src).map_err(|e| e.to_string());
    let interp = run_capture_parallel(src).map_err(|e| e.to_string());
    assert_eq!(
        vm, interp,
        "vm/interp divergence (index specialization must be behavior-preserving):\n{src}"
    );
}

#[test]
fn idxspec_int_map_get_hit_and_miss() {
    // Int-key map read: a present key returns its value; an absent key faults "key not found".
    idx_parity("m := {1: 10, 2: 20}\nprint(m[1])\nprint(m[2])\n");
    idx_parity("m := {1: 10}\nprint(m[99])\n"); // miss → "key not found", same on both engines
}

#[test]
fn idxspec_int_map_set_overwrite_and_insert() {
    // Int-key map write: overwrite an existing entry, insert a new one; len + reads agree.
    idx_parity("m := {1: 10}\nm[1] = 11\nm[2] = 20\nprint(m[1])\nprint(m[2])\nprint(m.len())\n");
}

#[test]
fn idxspec_int_list_get_set_in_bounds() {
    idx_parity("xs := [5, 6, 7]\nprint(xs[0])\nxs[2] = 99\nprint(xs[2])\n");
}

#[test]
fn idxspec_list_out_of_bounds_message_exact() {
    // Both get and set must surface the exact same bounds message through the fast path's fallback.
    idx_parity("xs := [1, 2, 3]\nprint(xs[5])\n");
    idx_parity("xs := [1, 2, 3]\nxs[5] = 0\n");
    idx_parity("xs := [1, 2, 3]\nprint(xs[-1])\n"); // negative → out of bounds, not a panic
}

#[test]
fn idxspec_non_int_map_keys_via_fallback() {
    // Str + bool keys must NOT take the Int fast path — they route through the unchanged general
    // match (content/scalar hash). Output + a str-key miss message stay identical.
    idx_parity("m := {\"a\": 1, \"b\": 2}\nprint(m[\"a\"])\nprint(m[\"b\"])\n");
    idx_parity("m := {true: 1, false: 0}\nprint(m[false])\nprint(m[true])\n");
    idx_parity("m := {\"a\": 1}\nprint(m[\"z\"])\n"); // str miss → "key not found"
}

#[test]
fn idxspec_struct_index_protocol_via_fallback() {
    // THE TRAP: an Int key on a struct receiver must dispatch the `index`/`set_index` protocol,
    // NOT the List/Map Int fast path. The receiver kind (Struct) gates the fast path, not the key.
    let src = "struct Buf:\n    xs: List[int]\n    fn index(self, k: int) -> int:\n        return self.xs[k]\n    fn set_index(self, k: int, v: int):\n        self.xs[k] = v\n\
                   b := Buf([10, 20, 30])\nprint(b[0])\nb[1] = 99\nprint(b[1])\n";
    idx_parity(src);
}

#[test]
fn idxspec_int_float_key_collision_resolves() {
    // Int(3) and Float(3.0) hash identically (3.0.to_bits()) and are values_equal. The fast path
    // shortcuts only the HASH, never the candidates+values_equal probe, so a Float key inserted as
    // 3.0 is found by m[3] and vice-versa — exactly the interpreter's behavior.
    idx_parity(
        "m := {}\nm[3] = \"int\"\nprint(m[3.0])\nm[3.0] = \"float\"\nprint(m[3])\nprint(m.len())\n",
    );
}

// ---- M19 Phase 4: struct-field inline cache (correctness guards) ----

/// Run on the VM and the frozen interpreter; assert byte-identical stdout (the M19 parity bar),
/// and return the shared output. The field IC is a VM-only speedup, so any divergence is a bug.
fn run_parity(src: &str) -> String {
    let vm = run_capture(src).expect("vm run");
    let interp = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm, interp,
        "vm/interp divergence (field IC must be behavior-preserving)"
    );
    vm
}

#[test]
fn nan_format_spec_sign_parity() {
    // A negative-signed NaN (0.0/0.0) must render `NaN` (not `-NaN`) through the format-spec
    // path on BOTH engines, matching the bare stringify path. Infinities keep their sign.
    let src = "n := 0.0 / 0.0\n\
                   print(\"{n:.2f}\")\nprint(\"{n:f}\")\nprint(\"{n:e}\")\n\
                   ninf := -1.0 / 0.0\nprint(\"{ninf:.2f}\")\n\
                   pinf := 1.0 / 0.0\nprint(\"{pinf:.2f}\")\n";
    assert_eq!(run_parity(src), "NaN\nNaN\nNaN\n-inf\ninf\n");
}

#[test]
fn neg_int_pattern_runtime_parity() {
    // A negative int literal pattern matches the negative value and nothing else; a
    // negative-bounded range pattern is half-open (`-10 <= v < -5`). VM == interp.
    let src = "fn classify(x: int) -> str:\n\
                   \x20   match x:\n\
                   \x20       -3: return \"neg3\"\n\
                   \x20       -10..-5: return \"lo\"\n\
                   \x20       _: return \"other\"\n\
                   print(classify(-3))\n\
                   print(classify(-7))\n\
                   print(classify(-5))\n\
                   print(classify(-10))\n\
                   print(classify(3))\n";
    assert_eq!(run_parity(src), "neg3\nlo\nother\nlo\nother\n");
}

#[test]
fn neg_pattern_with_guard_and_or_parity() {
    // Negatives compose with guards and or-patterns; both engines agree.
    let src = "fn f(x: int, flag: bool) -> str:\n\
                   \x20   match x:\n\
                   \x20       -3 if flag: return \"g\"\n\
                   \x20       -1 | -2: return \"or\"\n\
                   \x20       _: return \"_\"\n\
                   print(f(-3, true))\n\
                   print(f(-3, false))\n\
                   print(f(-1, false))\n\
                   print(f(-2, true))\n\
                   print(f(0, true))\n";
    assert_eq!(run_parity(src), "g\n_\nor\nor\n_\n");
}

#[test]
fn ic_deep_field_read() {
    // Read the LAST field of a 6-field struct in a loop: exercises the IC hit path past five
    // would-be name-probes. Cached idx must point at `f` every iteration.
    let src = "struct S:\n    a: int\n    b: int\n    c: int\n    d: int\n    e: int\n    f: int\n\
                   s := S(1, 2, 3, 4, 5, 6)\n\
                   i := 0\nacc := 0\nwhile i < 5:\n    acc = acc + s.f\n    i = i + 1\nprint(acc)\n";
    assert_eq!(run_parity(src), "30\n");
}

#[test]
fn ic_field_write_then_read() {
    // SetField IC: mutate `x` (plain) and `y` (compound) in a loop, then read both back. The
    // write cache and the read cache must agree on the field index.
    let src = "struct P:\n    x: int\n    y: int\n\
                   p := P(0, 0)\n\
                   i := 0\nwhile i < 4:\n    p.x = p.x + 2\n    p.y += 3\n    i = i + 1\n\
                   print(p.x)\nprint(p.y)\n";
    assert_eq!(run_parity(src), "8\n12\n");
}

#[test]
fn ic_distinct_layouts() {
    // Two structs whose shared field names sit at DIFFERENT indices (A{x,y} vs B{y,x}), read at
    // their own sites in one loop. A bug that confused per-site IC cells (bad id allocation, or a
    // hit that skipped the name re-verify) would return a wrong field; the verify keeps it sound.
    let src = "struct A:\n    x: int\n    y: int\nstruct B:\n    y: int\n    x: int\n\
                   a := A(1, 2)\nb := B(3, 4)\n\
                   i := 0\ns := 0\nwhile i < 3:\n    s = s + a.x + a.y + b.x + b.y\n    i = i + 1\nprint(s)\n";
    // per iter: a.x=1 a.y=2 b.x=4 b.y=3 => 10; *3 = 30
    assert_eq!(run_parity(src), "30\n");
}

#[test]
fn ic_self_field_method() {
    // `self.field` reads inside a method called in a loop — the hot OO path the IC targets.
    let src = "struct Counter:\n    n: int\n\n    fn get(self) -> int:\n        return self.n\n\
                   c := Counter(7)\n\
                   i := 0\nacc := 0\nwhile i < 5:\n    acc = acc + c.get()\n    i = i + 1\nprint(acc)\n";
    assert_eq!(run_parity(src), "35\n");
}

#[test]
fn ic_struct_under_parallel_engine() {
    // The IC lives on each worker `Vm` too; field-heavy code run on the real-thread engine must
    // produce the same output as the cooperative engine (the caches are per-Vm, self-verifying).
    let src = "struct Pt:\n    x: int\n    y: int\n\n    fn sum(self) -> int:\n        return self.x + self.y\n\
                   p := Pt(3, 4)\n\
                   acc := 0\ni := 0\nwhile i < 100:\n    acc = acc + p.sum()\n    p.x = p.x + 1\n    i = i + 1\n\
                   print(acc)\nprint(p.x)\n";
    assert_eq!(run_capture_parallel(src).expect("parallel"), run(src));
}

#[test]
fn ic_gc_stress_fields() {
    // Field reads under collect-before-every-instruction: cached indices stay valid because GC
    // never reorders a struct's `fields` Vec (and the IC holds indices, not GcRefs).
    let src = "struct V:\n    a: int\n    b: int\n    c: int\n\
                   v := V(10, 20, 30)\n\
                   i := 0\nacc := 0\nwhile i < 30:\n    acc = acc + v.a + v.b + v.c\n    i = i + 1\nprint(acc)\n";
    assert_eq!(run_capture_stress(src), run(src));
    assert_eq!(run_parity(src), "1800\n");
}

// ---- M19 Phase 5b: struct type-id guard on the field IC (correctness guards) ----
// The IC hit now guards on a numeric `tid` (== struct layout identity) instead of re-verifying
// the field name string. Soundness rests on: every distinct layout has a distinct `tid`, and the
// empty/sentinel `tid` never matches. These lock that behavior is unchanged.

#[test]
fn typeid_guard_distinct_layouts_keep_distinct_values() {
    // Same field names on two types at SWAPPED indices, read in a hot loop. With the tid guard,
    // each per-type site caches (tid, idx); a guard that ignored type identity (or stamped a
    // shared tid) would read the wrong slot. Values asserted, not just a sum, to pin the layout.
    let src = concat!(
        "struct A:\n    v: int\n    w: int\n",
        "struct B:\n    w: int\n    v: int\n",
        "a := A(1, 2)\n",
        "b := B(3, 4)\n",
        "i := 0\nout := 0\n",
        "while i < 4:\n    out = out + a.v * 1000 + a.w * 100 + b.v * 10 + b.w\n    i = i + 1\n",
        // per iter: a.v=1,a.w=2,b.v=4,b.w=3 -> 1000+200+40+3 = 1243 ; *4 = 4972
        "print(out)\n",
    );
    assert_eq!(run_parity(src), "4972\n");
}

#[test]
fn typeid_guard_struct_round_trips_through_channel() {
    // A struct sent across a Channel is serialized (to_wire) and rebuilt (from_wire) in the
    // receiver; from_wire must stamp a `tid` so the receiver's field IC stays sound. VM-only
    // (channels don't run under the frozen interp), so assert the VM output directly.
    let src = concat!(
        "struct Pt:\n    x: int\n    y: int\n",
        "fn worker(ch: Channel[Pt]):\n    p := ch.recv()\n    print(\"{p.x} {p.y}\")\n",
        "fn sender(ch: Channel[Pt]):\n    ch.send(Pt(10, 20))\n",
        "fn main():\n    ch := Channel[Pt]()\n    parallel:\n        spawn worker(ch)\n        spawn sender(ch)\n",
        "main()\n",
    );
    assert_eq!(run(src), "10 20\n");
}

// ---- M19 Tier-2: adaptive opcode quickening (PEP 659), v1 — binops — correctness guards ----
// The un-fused generic binop arms (`Add..GtEq` reached by stack operands; `Eq`/`NotEq` always)
// specialize to an int/int fast path behind a per-`Vm`, per-site (proto,ip) deopt guard. The
// side table holds only state bytes (no `GcRef`), so it is heap-independent — never swapped in
// `swap_ctx`, like `field_ic`/`method_ic`. Behaviour is byte-identical to the generic path; the
// interpreter is untouched, so two-engine parity holds by construction. These guard the gotchas.

#[test]
fn free_test_fn_is_tagged_and_recorded() {
    // A free `test fn` is recorded in `program.tests` and its proto is tagged `is_test`; an
    // ordinary `fn` is neither.
    let src = "test fn t():\n    assert true\nfn helper():\n    return\n";
    let module = parser::parse(lexer::tokenize(src).unwrap()).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    assert_eq!(program.tests.len(), 1, "exactly one free test recorded");
    let (name, pid) = &program.tests[0];
    assert_eq!(name, "t");
    assert!(
        program.protos[*pid].is_test,
        "the test proto is tagged is_test"
    );
    // The `helper` proto is not a test.
    assert!(
        program.protos.iter().filter(|p| p.is_test).count() == 1,
        "only the test fn is tagged"
    );
}

#[test]
fn suite_is_discovered_with_thunk_and_methods() {
    // A struct with a `test fn` method is a suite with a `__new_` thunk + recorded test methods.
    let src = "struct S:\n    n: int = 7\n    test fn a(self):\n        assert self.n == 7\n    fn before_each(self):\n        return\n";
    let module = parser::parse(lexer::tokenize(src).unwrap()).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    assert_eq!(program.suites.len(), 1, "one suite discovered");
    let s = &program.suites[0];
    assert_eq!(s.name, "S");
    assert_eq!(
        s.tests.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
        vec!["a"]
    );
    assert!(
        s.hooks.contains_key("before_each"),
        "before_each hook recorded"
    );
    // The thunk proto exists and the struct records its test methods. ROOT REDESIGN — the struct
    // table is keyed by the qualified IDENTITY KEY (`<main>::S` on the standalone path).
    assert!(s.new_thunk < program.protos.len());
    assert_eq!(
        program.structs["<main>::S"].test_methods,
        vec!["a".to_string()]
    );
}

#[test]
fn widen_suite_float_field_coerced() {
    // A `float` suite field with an int default stores a genuine f64 — the suite-construction
    // thunk emits `Op::CoerceFloat` (it bypasses `compile_ctor_args`). Regression for the
    // prosecutor charge "Int in a float slot via the suite thunk" (suites are VM-only, so no
    // two-engine parity test covers this path).
    let src = "struct SuiteF:\n    v: float = 3\n    test fn t(self):\n        assert self.v / 2 == 1.5\n";
    let module = parser::parse(lexer::tokenize(src).unwrap()).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    let thunk = program.suites[0].new_thunk;
    let mut vm = Vm::new(Arc::new(program));
    vm.init_for_tests().unwrap();
    let inst = vm.build_suite_instance(thunk).unwrap();
    let Some(h) = inst.as_obj() else {
        panic!("suite instance is not an object");
    };
    let f0 = {
        let Obj::Struct { fields, .. } = vm.heap.get(h) else {
            panic!("suite instance is not a struct");
        };
        fields[0]
    };
    assert!(
        f0.is_float(),
        "float suite field must store a boxed float, not Int(3)"
    );
    assert_eq!(
        vm.float_of(f0),
        3.0,
        "float suite field must store f64(3.0)"
    );
}

#[test]
fn quicken_table_presized_and_based() {
    // White-box wiring: the per-`Vm` quicken side table has one state byte per program
    // instruction, and `quicken_base` is the prefix sum of per-proto code lengths so a site is
    // `quicken_base[pid] + ip` (mirrors `field_ic_sites`/`field_ic` presizing).
    let src = "i := 0\ntotal := 0\nwhile i < 5:\n    total = total + i * i\n    i = i + 1\nprint(total)\n";
    let tokens = lexer::tokenize(src).unwrap();
    let module = parser::parse(tokens).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    let vm = Vm::new(Arc::new(program.clone()));
    let total: usize = program.protos.iter().map(|p| p.code.len()).sum();
    assert_eq!(vm.quicken.len(), total, "one quicken cell per instruction");
    assert_eq!(
        vm.quicken_base.len(),
        program.protos.len(),
        "one base per proto"
    );
    // prefix-sum invariant: base[0]==0, base[k+1]==base[k]+len(proto[k])
    let mut acc = 0u32;
    for (pid, p) in program.protos.iter().enumerate() {
        assert_eq!(
            vm.quicken_base[pid], acc,
            "base[{pid}] is the running prefix sum"
        );
        acc += p.code.len() as u32;
    }
    // all cells start Cold (0)
    assert!(vm.quicken.iter().all(|&b| b == 0), "every site starts Cold");
}

#[test]
fn quicken_eq_uses_exact_i64_semantics() {
    // The quickened int `Eq`/`NotEq` fast path compares EXACT i64 (`x == y`), matching
    // `values_equal_guarded`'s exact `(Int,Int)` arm — Python parity. 2^53 vs 2^53+1 are DISTINCT
    // ints (they'd collide only under the old lossy `as_f64` compare), so `==` is FALSE and `!=` is
    // TRUE. Run it in a hot loop so the site warms past Cold into the specialized Int state — this
    // guards that the specialized path stays exact, not just the generic one.
    let src = "i := 0\nhits := 0\nwhile i < 3:\n    if 9007199254740992 == 9007199254740993:\n        hits = hits + 1\n    i = i + 1\nprint(hits)\n";
    assert_eq!(run_parity(src), "0\n");
    let src2 = "i := 0\nmiss := 0\nwhile i < 3:\n    if 9007199254740992 != 9007199254740993:\n        miss = miss + 1\n    i = i + 1\nprint(miss)\n";
    assert_eq!(run_parity(src2), "3\n");
}

#[test]
fn quicken_eq_small_ints_exact() {
    // Small ints (within f64 exact range) compare normally; loop warms the site to Int state.
    let src = "i := 0\nc := 0\nwhile i < 6:\n    if i == 3:\n        c = c + 100\n    if i != 3:\n        c = c + 1\n    i = i + 1\nprint(c)\n";
    // i==3 once (+100), i!=3 five times (+5) = 105
    assert_eq!(run_parity(src), "105\n");
}

#[test]
fn quicken_deopt_int_then_float_then_str() {
    // A single generic `+` site reached first with ints (warms to Int), then floats, then strings
    // (string concat) — each must deopt cleanly to the generic path and stay correct. The `+` of
    // two CALL results is a stack-operand Add (un-fused), exactly the quickening target.
    let src = "fn add2[T](a: T, b: T) -> T:\n    return a + b\n\
                   print(add2(add2(2, 3), add2(4, 5)))\n\
                   print(add2(1.5, 2.5))\n\
                   print(add2(\"ab\", \"cd\"))\n";
    // 5+9=14 ; 1.5+2.5=4.0 ; abcd
    assert_eq!(run_parity(src), "14\n4.0\nabcd\n");
}

#[test]
fn quicken_stack_arith_and_compare_int_fast_path() {
    // Stack-operand arith + ordered compare on ints (the un-fused generic arms). `(a*b) - (c+d)`
    // pushes intermediate results then operates on them — not a `local⊕local`/`local⊕const`
    // window, so it never fuses to a superinstruction and rides the quickened path instead.
    let src = "fn f(a: int, b: int, c: int, d: int) -> int:\n    return (a * b) - (c + d)\n\
                   i := 0\nacc := 0\nwhile i < 4:\n    if f(i + 2, i + 3, i, 1) > 0:\n        acc = acc + f(i + 2, i + 3, i, 1)\n    i = i + 1\nprint(acc)\n";
    // f = (i+2)(i+3) - (i+1): i=0:6-1=5; i=1:12-2=10; i=2:20-3=17; i=3:30-4=26 ; all >0 => 58
    assert_eq!(run_parity(src), "58\n");
}

#[test]
fn quicken_overflow_and_divzero_errors_match_generic() {
    // The quickened int fast path reuses `fast_int_bin`, so overflow / div-by-zero must raise the
    // SAME error as the generic `arith` path. Warm the site, then trip it.
    let dz = "i := 0\nwhile i < 1:\n    print(10 / (i - i))\n    i = i + 1\n";
    let err = run_capture(dz).unwrap_err().to_string();
    assert!(err.contains("division by zero"), "got: {err}");
    let mz = "i := 0\nwhile i < 1:\n    print(10 % (i - i))\n    i = i + 1\n";
    let err2 = run_capture(mz).unwrap_err().to_string();
    assert!(err2.contains("modulo by zero"), "got: {err2}");
}

// ---- M19 Phase 6: method-call inline cache (+ flatten) — correctness guards ----
// `Op::CallMethod` on a struct caches `(tid → proto, module_idx)` per call site, mirroring the
// field IC: a hit on a matching `tid` skips the `program.structs` clone + the name-keyed
// `def.methods` probe. The cell holds no `GcRef` (proto id + module_idx are heap-independent), so
// it is invisible to GC / snapshots / `swap_ctx` — sound across cooperative fibers and `--parallel`.

#[test]
fn method_ic_sites_allocated_and_vm_presized() {
    // White-box wiring: a program with struct-method calls allocates ≥1 method-IC site, and the
    // VM pre-sizes its per-`Vm` `method_ic` vector to match (mirrors `field_ic_sites`/`field_ic`).
    let src = "struct C:\n    n: int\n\n    fn g(self) -> int:\n        return self.n\nc := C(5)\nprint(c.g())\n";
    let tokens = lexer::tokenize(src).unwrap();
    let module = parser::parse(tokens).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    assert!(
        program.method_ic_sites >= 1,
        "expected ≥1 method-IC site, got {}",
        program.method_ic_sites
    );
    let vm = Vm::new(Arc::new(program.clone()));
    assert_eq!(vm.method_ic.len(), program.method_ic_sites as usize);
}

#[test]
fn method_ic_monomorphic_hot_loop() {
    // A struct method called in a hot loop — the IC hit path. The method DISPATCH (not a field
    // read) is what the method IC caches; the cached proto must be re-used every iteration.
    let src = "struct Acc:\n    n: int\n\n    fn add(self, k: int) -> int:\n        return self.n + k\n\
                   a := Acc(10)\ni := 0\nout := 0\nwhile i < 5:\n    out = out + a.add(i)\n    i = i + 1\nprint(out)\n";
    // per iter: 10 + i -> 10,11,12,13,14 = 60
    assert_eq!(run_parity(src), "60\n");
}

#[test]
fn method_ic_polymorphic_one_site_via_protocol() {
    // A protocol-bounded generic fn has ONE `CallMethod` site (type-erased body) reached by two
    // distinct struct types. A method-IC hit that ignored type identity would dispatch a stale
    // proto (Sq.area on a Rect); the `tid` guard forces a re-resolve on the type switch.
    let src = "protocol Shape:\n    fn area(self) -> int\n\
                   struct Sq:\n    s: int\n\n    fn area(self) -> int:\n        return self.s * self.s\n\
                   struct Rect:\n    w: int\n    h: int\n\n    fn area(self) -> int:\n        return self.w * self.h\n\
                   fn describe[S: Shape](x: S) -> int:\n    return x.area()\n\
                   i := 0\nout := 0\nwhile i < 4:\n    out = out + describe(Sq(3)) + describe(Rect(2, 5))\n    i = i + 1\nprint(out)\n";
    // per iter: 9 + 10 = 19 ; *4 = 76
    assert_eq!(run_parity(src), "76\n");
}

#[test]
fn method_ic_under_parallel_engine() {
    // The method IC lives on each worker `Vm`; method-heavy code on the real-thread engine must
    // match the cooperative engine (caches are per-Vm, tid-guarded, self-verifying).
    let src = "struct Pt:\n    x: int\n    y: int\n\n    fn sum(self) -> int:\n        return self.x + self.y\n\
                   p := Pt(3, 4)\nacc := 0\ni := 0\nwhile i < 100:\n    acc = acc + p.sum()\n    p.x = p.x + 1\n    i = i + 1\n\
                   print(acc)\nprint(p.x)\n";
    assert_eq!(run_capture_parallel(src).expect("parallel"), run(src));
}

#[test]
fn method_ic_gc_stress() {
    // Method dispatch under collect-before-every-instruction: the cached proto/module_idx stay
    // valid because they hold no GcRef and GC never reorders a struct's identity.
    let src = "struct Box:\n    v: int\n\n    fn doubled(self) -> int:\n        return self.v * 2\n\
                   b := Box(21)\ni := 0\nacc := 0\nwhile i < 30:\n    acc = acc + b.doubled()\n    i = i + 1\nprint(acc)\n";
    assert_eq!(run_capture_stress(src), run(src));
    assert_eq!(run_parity(src), "1260\n");
}

#[test]
fn method_ic_function_typed_field_not_cached() {
    // `recv.f(args)` where `f` is a function-typed FIELD (not a method) must keep dispatching via
    // `invoke_value` — the method IC must never cache it as a method proto.
    let src = "struct H:\n    op: fn(int) -> int\n\
                   double := fn(x: int) -> int: x * 2\nh := H(double)\n\
                   i := 0\nout := 0\nwhile i < 3:\n    out = out + h.op(i + 1)\n    i = i + 1\nprint(out)\n";
    // per iter: (i+1)*2 -> 2,4,6 = 12
    assert_eq!(run_parity(src), "12\n");
}

#[test]
fn method_ic_struct_method_shadowing_hof_name() {
    // The IC fast path sits BEFORE the list-HOF / core-type guards in `do_method_call`. A struct
    // whose own method is named `map` (a built-in list HOF name) must dispatch the STRUCT method,
    // never the list HOF — the `Obj::Struct` tid guard makes the collision impossible, this pins it.
    let src = "struct Grid:\n    n: int\n\n    fn map(self, k: int) -> int:\n        return self.n + k\n\
                   g := Grid(100)\ni := 0\nacc := 0\nwhile i < 3:\n    acc = acc + g.map(i)\n    i = i + 1\nprint(acc)\n";
    // 100+0 + 100+1 + 100+2 = 303
    assert_eq!(run_parity(src), "303\n");
}

#[test]
fn method_ic_flattened_method_with_defer_in_loop() {
    // A flattened method's `do_return` must drain the frame's `defer`s on the IC-hit path, every
    // iteration, AFTER the return value is captured (Go order) — pinned across repeated hits.
    let src = "fn note(id: int):\n    print(\"d{id}\")\n\
                   struct Logger:\n    id: int\n\n    fn work(self, n: int) -> int:\n        defer note(self.id)\n        return n * 2\n\
                   l := Logger(7)\ni := 0\nacc := 0\nwhile i < 3:\n    acc = acc + l.work(i)\n    i = i + 1\nprint(acc)\n";
    // each call: prints d7 (defer), returns n*2 -> 0,2,4 = 6
    assert_eq!(run_parity(src), "d7\nd7\nd7\n6\n");
}

#[test]
fn method_ic_uncaught_fault_on_hit_path() {
    // Warm the IC with a good call, then fault on a cached hit. The flattened/cached path must
    // produce the SAME uncaught-fault behavior (message + that the program errors) as a fresh
    // resolve — the frozen interp is the oracle (run_err asserts the VM error; parity via interp).
    let src = "struct Bomb:\n    n: int\n\n    fn blow(self, d: int) -> int:\n        return self.n / d\n\
                   b := Bomb(10)\nprint(b.blow(2))\nprint(b.blow(0))\n";
    let vm_err = run_err(src);
    let interp_err = match run_capture_parallel(src) {
        Ok(o) => panic!("expected interp error, got {o:?}"),
        Err(e) => e.message,
    };
    assert_eq!(
        vm_err, interp_err,
        "VM/interp must agree on the IC-hit-path fault message"
    );
    assert!(
        vm_err.contains("zero") || vm_err.contains("division"),
        "got: {vm_err}"
    );
}

#[test]
fn method_ic_survives_fiber_park_under_parallel() {
    // The per-`Vm` `method_ic` must stay intact across a `swap_ctx` (a fiber parks on `recv`, another
    // runs, the parked fiber resumes and makes a CACHED method call). The central liveness claim:
    // the cell holds no `GcRef`, so a context swap can't invalidate it. VM == `--parallel`.
    let src = "struct Acc:\n    base: int\n\n    fn fold_in(self, k: int) -> int:\n        return self.base + k\n\
                   fn consumer(ch: Channel[int]):\n    a := Acc(1000)\n    total := 0\n    i := 0\n    while i < 4:\n        v := ch.recv()\n        total = total + a.fold_in(v)\n        i = i + 1\n    print(total)\n\
                   fn producer(ch: Channel[int]):\n    i := 0\n    while i < 4:\n        ch.send(i)\n        i = i + 1\n\
                   fn main():\n    ch := Channel[int]()\n    parallel:\n        spawn consumer(ch)\n        spawn producer(ch)\nmain()\n";
    // total = 4*1000 + (0+1+2+3) = 4006
    assert_eq!(run_capture_parallel(src).expect("parallel"), run(src));
    assert_eq!(run(src), "4006\n");
}

// ---- M19 — N-way polymorphic method-call IC (CALLMETHOD ADAPTIVE) correctness guards ----
// The single `MethodIcCell` per site is widened to a small N-way poly cache: a megamorphic site
// (a `List[Shape]` walked at one `.area()` call) must HIT a way for each distinct receiver `tid`
// — never thrash the monomorphic refill, never dispatch a wrong body. Each way is tid+arity
// re-guarded on every hit (a way can never enter a frame with the wrong slot count or body). A
// 5th+ distinct tid sets a one-way sticky-generic bit so the site stops probing the ways and goes
// straight to the (clone-free) slow path, mirroring the binop quickening's `Q_GENERIC`.

#[test]
fn mega_dispatch_correctness_parity() {
    // A `Shape` protocol with `.area()` on FOUR distinct struct types (distinct layouts/tids).
    // A heterogeneous `List[Shape]` is walked at ONE call site, repeatedly (each type hit many
    // times). The right body must dispatch per type across repeated calls; VM == interp.
    let src = "protocol Shape:\n    fn area(self) -> int\n\
                   struct Sq:\n    s: int\n\n    fn area(self) -> int:\n        return self.s * self.s\n\
                   struct Rect:\n    w: int\n    h: int\n\n    fn area(self) -> int:\n        return self.w * self.h\n\
                   struct Tri:\n    b: int\n    hh: int\n\n    fn area(self) -> int:\n        return self.b * self.hh / 2\n\
                   struct Circ:\n    r: int\n\n    fn area(self) -> int:\n        return self.r + self.r\n\
                   fn total(shapes: List[Shape]) -> int:\n    acc := 0\n    for s in shapes:\n        acc = acc + s.area()\n    return acc\n\
                   shapes := []\nshapes.push(Sq(3))\nshapes.push(Rect(2, 4))\nshapes.push(Tri(4, 5))\nshapes.push(Circ(3))\n\
                   out := 0\ni := 0\nwhile i < 8:\n    out = out + total(shapes)\n    i = i + 1\nprint(out)\n";
    // per pass: 9 + 8 + 10 + 6 = 33 ; *8 = 264
    assert_eq!(run_parity(src), "264\n");
}

#[test]
fn poly_ic_all_ways_distinct_bodies() {
    // Four distinct tids each map to a DIFFERENT body returning a tid-distinguishing constant.
    // Every way fills, then each type is re-called: each must still return its OWN body's value,
    // not a stale neighbour way's. A broken way-match (compare only way[0].tid) returns way-0's
    // body for the 2nd/3rd/4th types → wrong sum → this fails.
    let src = "protocol Tag:\n    fn id(self) -> int\n\
                   struct A:\n    n: int\n\n    fn id(self) -> int:\n        return 1\n\
                   struct B:\n    n: int\n\n    fn id(self) -> int:\n        return 2\n\
                   struct C:\n    n: int\n\n    fn id(self) -> int:\n        return 4\n\
                   struct D:\n    n: int\n\n    fn id(self) -> int:\n        return 8\n\
                   fn sum(xs: List[Tag]) -> int:\n    acc := 0\n    for x in xs:\n        acc = acc + x.id()\n    return acc\n\
                   xs := []\nxs.push(A(0))\nxs.push(B(0))\nxs.push(C(0))\nxs.push(D(0))\n\
                   out := 0\ni := 0\nwhile i < 5:\n    out = out + sum(xs)\n    i = i + 1\nprint(out)\n";
    // per pass 1+2+4+8 = 15 ; *5 = 75 (a stale-way bug would smear the bits)
    assert_eq!(run_parity(src), "75\n");
}

#[test]
fn poly_ic_overflow_goes_sticky_generic() {
    // FIVE+ distinct struct types at ONE site overflow the N(=4)-way cache. The 5th+ must resolve
    // via the slow path (sticky-generic), never a wrong way. All five bodies must still dispatch
    // correctly across repeated calls; VM == interp.
    let src = "protocol Tag:\n    fn id(self) -> int\n\
                   struct A:\n    n: int\n\n    fn id(self) -> int:\n        return 1\n\
                   struct B:\n    n: int\n\n    fn id(self) -> int:\n        return 10\n\
                   struct C:\n    n: int\n\n    fn id(self) -> int:\n        return 100\n\
                   struct D:\n    n: int\n\n    fn id(self) -> int:\n        return 1000\n\
                   struct E:\n    n: int\n\n    fn id(self) -> int:\n        return 10000\n\
                   struct F:\n    n: int\n\n    fn id(self) -> int:\n        return 100000\n\
                   fn sum(xs: List[Tag]) -> int:\n    acc := 0\n    for x in xs:\n        acc = acc + x.id()\n    return acc\n\
                   xs := []\nxs.push(A(0))\nxs.push(B(0))\nxs.push(C(0))\nxs.push(D(0))\nxs.push(E(0))\nxs.push(F(0))\n\
                   out := 0\ni := 0\nwhile i < 6:\n    out = out + sum(xs)\n    i = i + 1\nprint(out)\n";
    // per pass 111111 ; *6 = 666666
    assert_eq!(run_parity(src), "666666\n");
}

#[test]
fn poly_ic_site_latches_sticky_on_5th_type() {
    // White-box: after a 5-type megamorphic site runs, the site's 4 ways are all occupied AND the
    // `sticky` latch is set (so further calls skip way-probing — the anti-thrash guarantee). Pins
    // the deopt mechanism directly, not just the output. A monomorphic site stays non-sticky.
    let src = "protocol Tag:\n    fn id(self) -> int\n\
                   struct A:\n    n: int\n\n    fn id(self) -> int:\n        return 1\n\
                   struct B:\n    n: int\n\n    fn id(self) -> int:\n        return 2\n\
                   struct C:\n    n: int\n\n    fn id(self) -> int:\n        return 3\n\
                   struct D:\n    n: int\n\n    fn id(self) -> int:\n        return 4\n\
                   struct E:\n    n: int\n\n    fn id(self) -> int:\n        return 5\n\
                   fn sum(xs: List[Tag]) -> int:\n    acc := 0\n    for x in xs:\n        acc = acc + x.id()\n    return acc\n\
                   xs := []\nxs.push(A(0))\nxs.push(B(0))\nxs.push(C(0))\nxs.push(D(0))\nxs.push(E(0))\n\
                   print(sum(xs))\n";
    let tokens = lexer::tokenize(src).unwrap();
    let module = parser::parse(tokens).unwrap();
    let program = crate::compiler::compile_module_standalone(&module).unwrap();
    let mut vm = Vm::new(Arc::new(program));
    vm.run().expect("run");
    assert_eq!(vm.out, b"15\n");
    // Exactly one method-call site (the `x.id()` in `sum`), and it must have gone sticky with all
    // 4 ways filled by the first four distinct tids.
    let sticky_sites = vm.method_ic.iter().filter(|s| s.sticky).count();
    assert_eq!(
        sticky_sites, 1,
        "expected the megamorphic `id` site to latch sticky"
    );
    let sticky = vm.method_ic.iter().find(|s| s.sticky).unwrap();
    assert!(
        sticky.ways.iter().all(|w| w.tid != TID_NONE),
        "all 4 ways should be occupied before sticky"
    );
}

#[test]
fn structdef_clone_free_slow_path_parity() {
    // A megamorphic site (>4 types → sticky-generic slow path) PLUS a function-typed FIELD call
    // `recv.f(args)` (the fields-fallback arm that previously read the cloned Obj). Removing the
    // per-miss StructDef clone must not break either. VM == interp.
    let src = "protocol Tag:\n    fn id(self) -> int\n\
                   struct A:\n    n: int\n\n    fn id(self) -> int:\n        return 1\n\
                   struct B:\n    n: int\n\n    fn id(self) -> int:\n        return 2\n\
                   struct C:\n    n: int\n\n    fn id(self) -> int:\n        return 3\n\
                   struct D:\n    n: int\n\n    fn id(self) -> int:\n        return 4\n\
                   struct E:\n    n: int\n\n    fn id(self) -> int:\n        return 5\n\
                   struct H:\n    op: fn(int) -> int\n\
                   fn sum(xs: List[Tag]) -> int:\n    acc := 0\n    for x in xs:\n        acc = acc + x.id()\n    return acc\n\
                   xs := []\nxs.push(A(0))\nxs.push(B(0))\nxs.push(C(0))\nxs.push(D(0))\nxs.push(E(0))\n\
                   double := fn(x: int) -> int: x * 2\nh := H(double)\n\
                   out := 0\ni := 0\nwhile i < 3:\n    out = out + sum(xs) + h.op(i)\n    i = i + 1\nprint(out)\n";
    // per pass sum=15 ; h.op(i)=2*i -> i=0:15+0, i=1:15+2, i=2:15+4 => 45+6 = 51
    assert_eq!(run_parity(src), "51\n");
}

#[test]
fn inlined_hot_ops_path_matches_step() {
    // M19 Phase 7 — `run_until` dispatches the hottest ops (GetLocal/SetLocal, the superinstrs,
    // Jump/JumpIfFalse, Call/Return) inline and delegates the tail to `step`. This hammers every
    // inlined op in one program (locals + `a+b`/`a+const`/`i+=1` superinstrs + a conditional +
    // a call + a return) and pins the inline path == the frozen interp (which has no such split).
    let src = "fn f(a: int, b: int) -> int:\n    return a + b\n\
                   i := 0\nacc := 0\n\
                   while i < 20:\n    x := i * 2\n    if x % 3 == 0:\n        acc = acc + f(x, i)\n    else:\n        acc = acc + 1\n    i = i + 1\nprint(acc)\n";
    // x=0,3*?: i=0 x=0 x%3==0 acc+=f(0,0)=0; i=1 x=2 no acc+=1; i=2 x=4 no +1; i=3 x=6 yes +f(6,3)=9;
    // i=4 x=8 no +1; i=5 x=10 no +1; i=6 x=12 yes +f(12,6)=18; ... let the engines agree on the value.
    let out = run_parity(src);
    assert_eq!(out, run_capture_parallel(src).expect("interp"));
    assert!(!out.is_empty());
}

#[test]
fn method_call_flatten_deep_recursion_on_small_stack() {
    // Phase 6b: a recursive struct method must not consume host stack (frames live in the heap
    // `frames` Vec, executed by the running `run_until` — not a per-call Rust recursion). Survives
    // a host stack far below production `VM_STACK_BYTES`, like the plain-call flatten guarantee.
    let src = "struct R:\n    base: int\n\n    fn down(self, n: int) -> int:\n        if n == 0:\n            return self.base\n        return self.down(n - 1)\n\
                   r := R(99)\nprint(r.down(8000))\n";
    assert_eq!(
        run_capture_on_stack(src, 256 * 1024).expect("deep method recursion on small stack"),
        "99\n"
    );
}

#[test]
fn calls_preserve_arg_order_nesting_and_result_slot() {
    // P1 characterization: locks call semantics before the in-place-args refactor. The bugs an
    // in-place fast path could introduce are stack-position errors — wrong arg order, a stale
    // callee slot left under the result, or a misplaced return value in a larger expression.
    // Non-commutative op catches arg-order swaps; the nested/expression forms catch slot drift.
    assert_eq!(
        run("fn sub(a: int, b: int) -> int:\n    return a - b\nprint(sub(10, 3))\n"),
        "7\n"
    );
    assert_eq!(
        run(
            "fn sub(a: int, b: int) -> int:\n    return a - b\nprint(sub(sub(20, 5), sub(8, 3)))\n"
        ),
        "10\n"
    );
    assert_eq!(
        run("fn sub(a: int, b: int) -> int:\n    return a - b\nprint(sub(10, 3) * 2 + 1)\n"),
        "15\n"
    );
    // Zero-arg call returning a value; result used in an expression.
    assert_eq!(
        run("fn five() -> int:\n    return 5\nprint(five() + 1)\n"),
        "6\n"
    );
    // Recursion through the call path.
    assert_eq!(
        run(
            "fn fib(n: int) -> int:\n    if n < 2:\n        return n\n    return fib(n - 1) + fib(n - 2)\nprint(fib(10))\n"
        ),
        "55\n"
    );
    // Closure value called via a binding (the Closure arm of the fast path).
    assert_eq!(run("g := fn(x: int) -> int: x * 2\nprint(g(21))\n"), "42\n");
    // Closure capturing an outer binding, then called.
    assert_eq!(
        run("k := 100\nadd := fn(x: int) -> int: x + k\nprint(add(7))\n"),
        "107\n"
    );
    // HOF native (`map`) still routes through the Vec path in invoke_value — must stay correct.
    assert_eq!(
        run("print([1, 2, 3].map(fn(x: int) -> int: x + 1))\n"),
        "[2, 3, 4]\n"
    );
    // `defer` inside a called fn runs at that fn's exit (LIFO), not the caller's.
    assert_eq!(
        run(
            "fn log(s: str):\n    print(s)\nfn f():\n    defer log(\"a\")\n    defer log(\"b\")\n    log(\"body\")\nf()\nlog(\"after\")\n"
        ),
        "body\nb\na\nafter\n"
    );
}

#[test]
fn fstring_and_str_render_all_value_shapes() {
    // P2 characterization: locks the exact BuildStr / stringify output across every value
    // shape before the stringify-into-buffer refactor (separators, braces, nesting, hooks).
    assert_eq!(run("print(\"{1} {2.5} {true}\")\n"), "1 2.5 true\n");
    assert_eq!(run("x := 42\nprint(\"i={x}\")\n"), "i=42\n");
    assert_eq!(run("print(\"{[1, 2, 3]}\")\n"), "[1, 2, 3]\n");
    assert_eq!(run("print(\"{(1, 2)}\")\n"), "(1, 2)\n");
    assert_eq!(run("print(\"{[[1], [2, 3]]}\")\n"), "[[1], [2, 3]]\n");
    assert_eq!(
        run("m := {\"a\": 1, \"b\": 2}\nprint(\"{m}\")\n"),
        "{'a': 1, 'b': 2}\n"
    );
    assert_eq!(run("print(str({1, 2}))\n"), "{1, 2}\n");
    assert_eq!(run("s: Set[int] = Set()\nprint(str(s))\n"), "Set()\n");
    // Struct default repr + a multi-part f-string mixing literal text and several holes.
    assert_eq!(
        run("struct P:\n    x: int\n    y: int\nprint(\"p={P(3, 4)} end\")\n"),
        "p=P(x=3, y=4) end\n"
    );
    // `str(self)` protocol hook overrides the default repr inside interpolation.
    assert_eq!(
        run(
            "struct Pt:\n    x: int\n    fn str(self) -> str:\n        return \"<{self.x}>\"\nprint(\"v={Pt(7)}\")\n"
        ),
        "v=<7>\n"
    );
    // Enum nullary + payload variants.
    assert_eq!(
        run("enum E:\n    A\n    B(int, int)\nprint(\"{E.A} {E.B(1, 2)}\")\n"),
        "A B(1, 2)\n"
    );
}

#[test]
fn list_comprehension_maps_and_filters() {
    assert_eq!(run("print([x * 2 for x in [1, 2, 3]])\n"), "[2, 4, 6]\n");
    assert_eq!(
        run("print([x for x in [1, 2, 3, 4] if x % 2 == 0])\n"),
        "[2, 4]\n"
    );
}

#[test]
fn list_comprehension_over_range() {
    assert_eq!(run("print([x * x for x in 0..5])\n"), "[0, 1, 4, 9, 16]\n");
}

#[test]
fn vm_two_clause_list_comprehension() {
    // ys iterate inner-most for each x (Python order): (1,10),(1,20),(2,10),(2,20).
    assert_eq!(
        run("print([x + y for x in [1, 2] for y in [10, 20]])\n"),
        "[11, 21, 12, 22]\n"
    );
}

#[test]
fn vm_three_clause_list_comprehension() {
    assert_eq!(
        run("print([a * 100 + b * 10 + c for a in [1, 2] for b in [3] for c in [4, 5]])\n"),
        "[134, 135, 234, 235]\n"
    );
}

#[test]
fn vm_guard_after_nonfinal_clause() {
    // Only odd x survive (1, 3); each pairs with y in [10, 20].
    assert_eq!(
        run("print([x * y for x in 1..4 if x % 2 == 1 for y in [10, 20]])\n"),
        "[10, 20, 30, 60]\n"
    );
}

#[test]
fn vm_later_clause_references_earlier_var() {
    // Flatten a list-of-lists: second clause iterates the first clause's binding.
    assert_eq!(
        run("print([y for xs in [[1, 2], [3], [4, 5]] for y in xs])\n"),
        "[1, 2, 3, 4, 5]\n"
    );
}

#[test]
fn vm_nested_set_and_map_comprehension() {
    assert_eq!(
        run("print({x + y for x in [0, 3] for y in [0, 3]})\n"),
        "{0, 3, 6}\n"
    );
    assert_eq!(
        run("print({x * 10 + y: x + y for x in [1, 2] for y in [3]})\n"),
        "{13: 4, 23: 5}\n"
    );
}

/// A comprehension over a STATEFUL struct iterator must drive `next()` LAZILY — the element/guard
/// see the iterator's per-step state, exactly like a `for` statement and like the VM's
/// `compile_for`. The old interp eagerly drained the iterator to `None` before evaluating any
/// element (so `c.n` read the fully-advanced field), diverging from the VM. This asserts both
/// engines agree, for the single-clause AND the nested-clause shape, AND that they match the
/// lazy/interleaved result the VM produces.
#[test]
fn comprehension_stateful_struct_iterator_lazy_parity() {
    let src = "\
struct Counter:
    n: int
    fn next(self) -> Option[int]:
        v := self.n
        self.n = self.n + 1
        if v >= 3:
            return None
        return Some(v)

fn main():
    c := Counter(0)
    print([x * 100 + c.n for x in c])
    c2 := Counter(0)
    print([x * 100 + c2.n for x in c2 for y in [0, 1]])

main()
";
    let vm = run_capture(src).expect("vm run");
    let interp = run_capture_parallel(src).expect("interp run");
    // Two-engine parity (the hard rule).
    assert_eq!(
        vm, interp,
        "VM vs interp divergence on stateful-iterator comprehension"
    );
    // And the canonical (lazy/interleaved) result: each element reads the just-advanced `n`.
    assert_eq!(
        vm, "[1, 102, 203]\n[1, 1, 102, 102, 203, 203]\n",
        "lazy interleaved iteration expected"
    );
}

// ----- M6c: native function values -----

pub(crate) fn empty_program() -> Program {
    Program {
        protos: vec![],
        structs: Default::default(),
        enum_methods: Default::default(),
        enum_home: Default::default(),
        newtype_methods: Default::default(),
        newtype_home: Default::default(),
        native_methods: Default::default(),
        native_home: Default::default(),
        variants: Default::default(),
        variants_by_id: Vec::new(),
        struct_names: Vec::new(),
        eq_struct: Vec::new(),
        eq_enum: Vec::new(),
        modules: vec![],
        field_ic_sites: 0,
        method_ic_sites: 0,
        cffi_defs: vec![],
        tests: vec![],
        suites: vec![],
        type_names: Default::default(),
    }
}

/// A boxed `BigInt`/`FloatBox` must be observationally identical to the inline `Int`/`Float` of the
/// same value — display, hash (the f64-bits scheme, NOT `*n as u64`), equality, and ordering. These
/// variants are unreachable from real programs this phase, so this direct test is their only exercise.
#[test]
fn bigint_floatbox_behave_like_inline() {
    use std::cmp::Ordering;
    let mut vm = Vm::new(Arc::new(empty_program()));
    let bi = Value::obj(vm.heap.alloc(Obj::BigInt(5)));
    let bi2 = Value::obj(vm.heap.alloc(Obj::BigInt(5)));
    let fb = Value::obj(vm.heap.alloc(Obj::FloatBox(1.5)));
    let fb2 = Value::obj(vm.heap.alloc(Obj::FloatBox(1.5)));
    // Display matches the inline scalar.
    assert_eq!(vm.display(bi), "5");
    assert_eq!(vm.display(bi), vm.display(Value::int(5)));
    let ref15 = vm.box_float(1.5);
    assert_eq!(vm.display(fb), vm.display(ref15));
    // Hash matches the inline scalar (validates the canonical f64-bits scheme, not `*n as u64`).
    assert_eq!(vm.scalar_hash(bi), vm.scalar_hash(Value::int(5)));
    assert_eq!(vm.scalar_hash(fb), vm.scalar_hash(ref15));
    // Equality holds across two independent boxes of the same value.
    assert!(vm.values_equal(bi, bi2));
    assert!(vm.values_equal(fb, fb2));
    // Ordering: equal boxes compare Equal.
    assert_eq!(vm.value_order(bi, bi2), Ordering::Equal);
    assert_eq!(vm.value_order(fb, fb2), Ordering::Equal);
}

#[test]
fn vm_calls_native_fn_value() {
    use crate::native::{Host, HostError, NativeRet};
    fn add(h: &mut dyn Host) -> Result<NativeRet, HostError> {
        crate::native::expect_args(h, "add", 2)?;
        Ok(NativeRet::Int(h.arg_int(0)? + h.arg_int(1)?))
    }
    let mut vm = Vm::new(Arc::new(empty_program()));
    let h = vm.heap.alloc(Obj::Native {
        name: "add".into(),
        func: add,
        kind: crate::native::Kind::Inline,
    });
    vm.push(Value::obj(h));
    vm.push(Value::int(40));
    vm.push(Value::int(2));
    vm.do_call(2, Span::RUNTIME).unwrap();
    assert_eq!(vm.pop(), Value::int(42));
}

#[test]
fn guarded_restores_native_reentry_on_panic() {
    // Regression (FFI callbacks): a Rust panic re-entered through a native FFI callback is caught
    // one frame up by `callback_trampoline`'s `catch_unwind` and re-raised as a recoverable error.
    // `guarded` MUST restore `native_reentry` on that unwind — otherwise the counter leaks at +1
    // for the VM's lifetime, and every blocking op gated on `native_reentry == 0` silently
    // demotes/inlines instead of parking on `--parallel` after a recovered callback panic
    // (diverging from interp, which has no such counter). A `Drop` guard can't fix this (it would
    // alias `self` across the re-entry), so `guarded` catches + decrements + resumes the unwind.
    let mut vm = Vm::new(Arc::new(empty_program()));
    assert_eq!(vm.native_reentry, 0);
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {})); // silence the expected panic print
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vm.guarded(|_vm| -> Result<(), RuntimeError> { panic!("callback boom") })
    }));
    std::panic::set_hook(prev);
    assert!(
        caught.is_err(),
        "the panic must propagate out of guarded unchanged"
    );
    assert_eq!(
        vm.native_reentry, 0,
        "native_reentry must return to its entry value after a panicking re-entry"
    );
}

#[test]
fn cursor_crosses_airlock_by_deep_copy() {
    // A cursor (`Obj::Iter`) IS sendable: `to_wire` deep-copies it (recursively wiring items +
    // carrying `pos`) into a `WireValue::Iter`, and `from_wire` rebuilds an INDEPENDENT cursor —
    // exactly how a `List` crosses, and matching the interpreter's `deep_clone`. (Earlier this was
    // gated non-sendable like a generator, which panicked `deep_clone`'s `.expect` and diverged VM
    // from interp; a cursor is plain data — a snapshot Vec + index — so it crosses by value.)
    let mut vm = Vm::new(Arc::new(empty_program()));
    let cursor = Value::obj(vm.heap.alloc(Obj::Iter {
        items: vec![Value::int(7), Value::int(8)],
        pos: 1,
    }));
    let wire = vm
        .to_wire(cursor)
        .expect("a cursor is sendable by deep copy");
    assert!(!wire.has_handle(), "a cursor of scalars carries no handle");
    match &wire {
        WireValue::Iter { items, pos, .. } => {
            assert_eq!(*pos, 1, "pos carried across the wire");
            assert_eq!(items.len(), 2, "both snapshot items wired");
        }
        other => panic!("expected WireValue::Iter, got {other:?}"),
    }
    // Rebuild on the heap: an independent cursor with the same items + pos.
    let rebuilt = vm.from_wire(wire);
    let Some(h) = rebuilt.as_obj() else {
        panic!("from_wire(cursor) must be a heap obj")
    };
    match vm.heap.get(h) {
        Obj::Iter { items, pos } => {
            assert_eq!(*pos, 1);
            assert_eq!(items, &vec![Value::int(7), Value::int(8)]);
        }
        _ => panic!("expected a rebuilt Obj::Iter"),
    }
}

#[test]
fn vm_native_str_return_lowers_to_heap_with_no_children() {
    use crate::native::{Host, HostError, NativeRet};
    fn greet(_h: &mut dyn Host) -> Result<NativeRet, HostError> {
        Ok(NativeRet::Str("hi".into()))
    }
    let mut vm = Vm::new(Arc::new(empty_program()));
    let nat = vm.heap.alloc(Obj::Native {
        name: "greet".into(),
        func: greet,
        kind: crate::native::Kind::Inline,
    });
    // A native fn handle has no GC children (guards the mark-phase claim).
    assert!(vm.heap.children(nat).is_empty());
    vm.push(Value::obj(nat));
    vm.do_call(0, Span::RUNTIME).unwrap();
    let result = vm.pop();
    assert_eq!(vm.display(result), "hi");
}

// ----- B3.0: WireValue airlock (to_wire / from_wire) -----

/// A round-trip through the wire form, into the *same* heap, must be value-equal to the original
/// over a deeply-nested sendable mix (scalars, str, list, tuple, map, set, struct, enum). This is
/// the airlock's correctness invariant (B3.0): serialize then reconstruct loses nothing.
#[test]
fn wire_roundtrip_preserves_value_equality() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let s = vm.heap.alloc(Obj::Str("s".into()));
    let tup = vm
        .heap
        .alloc(Obj::Tuple(vec![Value::bool(true), Value::nil()]));
    let st = vm.heap.alloc(Obj::Struct {
        tid: TID_NONE,
        fields: crate::vm::heap::Fields::from_vec(vec![Value::int(1), Value::obj(s)]),
    });
    let en = vm.heap.alloc(Obj::Enum {
        // `empty_program` has no variant table, so use the unregistered sentinel (the enum analogue
        // of the struct's `TID_NONE` above) — it round-trips ("?"→VID_NONE→"?") value-equal.
        variant_id: crate::vm::op::VID_NONE,
        payload: vec![Value::int(9)],
    });
    let mut m = MapData::default();
    m.push(10, Value::int(1), Value::int(100));
    m.push(20, Value::obj(s), Value::int(200));
    let map = vm.heap.alloc(Obj::Map(m));
    let mut set = SetData::default();
    set.push(5, Value::int(1));
    set.push(6, Value::int(2));
    let setobj = vm.heap.alloc(Obj::Set(set));
    let list = vm.heap.alloc(Obj::List(vec![
        Value::int(1),
        Value::obj(s),
        Value::obj(tup),
        Value::obj(st),
        Value::obj(en),
        Value::obj(map),
        Value::obj(setobj),
    ]));
    let v = Value::obj(list);

    let w = vm
        .to_wire(v)
        .expect("nested sendable value should serialize");
    let wired = vm.from_wire(w);
    assert!(
        vm.values_equal(v, wired),
        "wire round-trip changed the value"
    );
    // Data is reconstructed into a *fresh* handle (deep copy, not aliasing the original).
    assert_ne!(
        v, wired,
        "round-tripped data should be a distinct heap object"
    );
}

/// `Map`/`Set` cross the wire carrying their **cached hashes** and **insertion order** unchanged —
/// `from_wire` rebuilds via `push(hash, …)`, never re-hashing. Pins byte-identical reconstruction
/// (the iteration order + index a later `print`/lookup observes) even when two keys collide.
#[test]
fn wire_preserves_map_hashes_and_order() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let mut m = MapData::default();
    m.push(42, Value::int(1), Value::int(10)); // collides with the third entry on hash 42
    m.push(7, Value::int(2), Value::int(20));
    m.push(42, Value::int(3), Value::int(30));
    let map = Value::obj(vm.heap.alloc(Obj::Map(m)));

    let w = vm.to_wire(map).expect("map should serialize");
    let wired = vm.from_wire(w);
    let Some(h) = wired.as_obj() else {
        panic!("expected heap obj")
    };
    let Obj::Map(rebuilt) = vm.heap.get(h) else {
        panic!("expected map")
    };
    let hashes: Vec<u64> = rebuilt.entries.iter().map(|(hash, ..)| *hash).collect();
    assert_eq!(
        hashes,
        vec![42, 7, 42],
        "cached hashes / order must survive the round-trip"
    );
    // The index must reflect the cached hashes (collision bucket points at positions 0 and 2).
    assert_eq!(rebuilt.candidates(42), &[0, 2]);
    assert_eq!(rebuilt.candidates(7), &[1]);
}

/// By-reference callables (`Func`/`Closure`/`Module`/`Native`) cross the airlock **by handle** —
/// `to_wire`→`from_wire` returns the *same* `GcRef` (matching the old `deep_clone` by-handle arm).
/// (`Str` no longer qualifies — it crosses by value as of B3.3a; see `wire_crosses_str_by_value`.)
#[test]
fn wire_passes_by_reference_objects_as_same_handle() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let m = vm.heap.alloc(Obj::Module(Box::new(ModuleData {
        name: "m".into(),
        slots: Vec::new(),
        index: Default::default(),
    })));
    let v = Value::obj(m);
    let w = vm.to_wire(v).expect("by-ref object should serialize");
    assert_eq!(
        vm.from_wire(w),
        v,
        "by-reference object must round-trip to the same handle"
    );
}

/// B3.3a: a `str` crosses the airlock **by value** (owned bytes), not as a by-reference
/// `Handle(GcRef)`: `from_wire` allocates a *fresh* heap `str` that is value-equal but a distinct
/// handle. This is what lets a `str` cross a real OS-thread heap boundary at B3.3 (a `GcRef` would
/// be a meaningless slot index there). Parity-safe: `str` is immutable + value-compared and Chezzi
/// has no identity operator, so a fresh handle is observationally identical to the shared one.
#[test]
fn wire_crosses_str_by_value() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let s = vm.heap.alloc(Obj::Str("imm".into()));
    let v = Value::obj(s);
    let w = vm.to_wire(v).expect("str should serialize");
    let wired = vm.from_wire(w);
    assert_ne!(
        wired, v,
        "a crossed str gets a fresh handle (by value, not by handle)"
    );
    assert!(
        vm.values_equal(v, wired),
        "the fresh str must be value-equal to the original"
    );
}

/// B3.3a: a `str` used as a **map key** crosses by value and stays findable — the cached hash is
/// carried through and `from_wire` rebuilds the key as a fresh handle whose content hashes
/// identically (hashing keys on bytes, not `GcRef`), so the reconstructed map's bucket index is
/// preserved. Guards against a future change that hashed/compared str keys by handle identity.
#[test]
fn wire_str_map_key_survives_roundtrip() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let key = vm.heap.alloc(Obj::Str("k".into()));
    let mut m = MapData::default();
    let h = vm.scalar_hash(Value::obj(key));
    m.push(h, Value::obj(key), Value::int(42));
    let map = Value::obj(vm.heap.alloc(Obj::Map(m)));

    let w = vm
        .to_wire(map)
        .expect("map with a str key should serialize");
    let wired = vm.from_wire(w);
    let Some(mh) = wired.as_obj() else {
        panic!("expected map handle")
    };
    let Obj::Map(rebuilt) = vm.heap.get(mh) else {
        panic!("expected map")
    };
    // Same single entry: a fresh str key, value-equal, same cached hash → same bucket.
    assert_eq!(rebuilt.entries.len(), 1);
    let (rh, rk, rv) = &rebuilt.entries[0];
    assert_eq!(*rh, h, "cached hash preserved");
    assert_eq!(*rv, Value::int(42));
    assert_eq!(
        rebuilt.candidates(h),
        &[0],
        "index bucket points at the rebuilt key"
    );
    assert!(
        vm.values_equal(*rk, Value::obj(key)),
        "rebuilt str key is value-equal"
    );
}

/// B3.1: `Channel`/`Shared`/`Executor` cross the airlock as their shared `Arc<…Core>`. The
/// round-trip yields a *fresh* `GcRef` (a new handle obj) wrapping the **same** core — identity is
/// at the `Arc`, not the handle, so two tasks still reach one mailbox/box/queue.
#[test]
fn wire_shares_core_across_a_fresh_handle() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let ch = vm
        .heap
        .alloc(Obj::Channel(Arc::new(ChannelCore::default())));
    let sh = vm.heap.alloc(Obj::Shared(Arc::new(SharedCore::default())));
    let rw = vm
        .heap
        .alloc(Obj::RwShared(Arc::new(RwSharedCore::default())));
    let ex = vm
        .heap
        .alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
    for h in [ch, sh, rw, ex] {
        let v = Value::obj(h);
        let w = vm.to_wire(v).expect("core handle should serialize");
        let wired = vm.from_wire(w);
        assert_ne!(wired, v, "a crossed core gets a fresh handle (new GcRef)");
        // Same underlying core: an `Arc::ptr_eq` between the two handles' cores.
        let same = match (vm.heap.get(h), vm.heap.get(wired.as_obj().unwrap())) {
            (Obj::Channel(a), Obj::Channel(b)) => Arc::ptr_eq(a, b),
            (Obj::Shared(a), Obj::Shared(b)) => Arc::ptr_eq(a, b),
            (Obj::RwShared(a), Obj::RwShared(b)) => Arc::ptr_eq(a, b),
            (Obj::Executor(a), Obj::Executor(b)) => Arc::ptr_eq(a, b),
            _ => false,
        };
        assert!(same, "the fresh handle must point at the SAME shared core");
    }
}

/// B3.1: two handles produced from one core (the `from_wire` airlock copy) reach the SAME mailbox
/// — `send` on one handle is `recv`-able through the other. Proves the `Arc` core is shared, not
/// duplicated, across the wire.
#[test]
fn channel_core_shared_across_handles() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let h1 = vm
        .heap
        .alloc(Obj::Channel(Arc::new(ChannelCore::default())));
    // Cross the airlock → a second handle onto the same core.
    let w = vm.to_wire(Value::obj(h1)).unwrap();
    let Some(h2) = vm.from_wire(w).as_obj() else {
        panic!("expected handle")
    };
    let sp = Span::RUNTIME;
    vm.channel_method(h1, "send", &[Value::int(7)], sp).unwrap();
    // recv through the OTHER handle sees the message.
    assert_eq!(
        vm.channel_method(h2, "recv", &[], sp).unwrap(),
        Value::int(7)
    );
}

/// Cross-thread channel delivery through one shared `ChannelCore`: a value `send`-queued on the
/// main thread is `recv`-popped on a **separate OS thread** running its own `Vm` — proving both
/// the `Arc<ChannelCore>` airlock is genuinely shared across threads AND `Vm: Send` (it moves into
/// `thread::spawn`), the load-bearing fact for the whole thread-flip.
///
/// NOTE: the ORDER is send-then-spawn on purpose. The original B3.3-threads step-2 shape spawned
/// the recv first and relied on a bare-`parallel` `recv` **blocking on the core's `Condvar`** until
/// woken. That condvar-recv path was RETIRED in D2b (4ac1c1b) when the M:N scheduler replaced
/// thread-per-task blocking with fiber snapshot-park (`send_wake` + the predicate deadlock detector).
/// A bare `parallel = true` `Vm` with NO scheduler (`mn == None`) now takes the cooperative path, so
/// an empty top-level `recv` is a deadlock FAULT by design — spawning recv-first was racy (it only
/// passed when `send` won the race; CI caught the block-first interleaving). The real
/// block-until-woken contract lives in the scheduler `send_wake`/park tests above (and
/// `parallel_recv_parks_deep_in_flattened_frames_and_resumes`), which exercise the actual current
/// mechanism; this test now covers only the still-true cross-thread-delivery + `Vm: Send` facts.
#[test]
fn parallel_recv_blocks_until_send_wakes_it() {
    let core = Arc::new(ChannelCore::default());
    let mut sender = Vm::new(Arc::new(empty_program()));
    let sh = sender.heap.alloc(Obj::Channel(Arc::clone(&core)));
    let mut worker = Vm::new(Arc::new(empty_program()));
    worker.parallel = true;
    let wh = worker.heap.alloc(Obj::Channel(Arc::clone(&core)));
    let sp = Span::RUNTIME;
    // Queue first, then hand the receiving `Vm` to another thread: the value is already in the
    // shared core, so the cross-thread `recv` pops it deterministically (no interleaving race).
    sender
        .channel_method(sh, "send", &[Value::int(42)], sp)
        .unwrap();
    let handle = std::thread::spawn(move || worker.channel_method(wh, "recv", &[], sp).unwrap());
    assert_eq!(handle.join().unwrap(), Value::int(42));
}

// ----- bounded Channel[T](cap): capacity + backpressure -----

/// `cap()` reports the bound (`Channel[T](n)` → n) or 0 for unbounded — identical on both engines.
#[test]
fn bounded_channel_cap_method_both_engines() {
    let src = "b := Channel[int](3)\nprint(b.cap())\nu := Channel[int]()\nprint(u.cap())\n";
    let out = run(src);
    assert_eq!(out, "3\n0\n");
    assert_eq!(out, run_capture_parallel(src).expect("M:N run"));
}

/// `Channel[T](0)` / a negative capacity is a runtime fault (cap must be > 0), on both engines with
/// the byte-identical message.
#[test]
fn bounded_channel_zero_cap_faults_both_engines() {
    for src in [
        "c := Channel[int](0)\nprint(c.len())\n",
        "c := Channel[int](-1)\nprint(c.len())\n",
    ] {
        let e = run_err(src);
        assert!(e.contains("Channel capacity must be > 0"), "serial: {e}");
        let ep = run_capture_parallel(src)
            .expect_err("M:N should fault")
            .message;
        assert_eq!(e, ep, "serial and M:N fault text must match");
    }
}

/// `try_send` on a FULL bounded channel returns `false` (not true — the old unbounded contract);
/// after a `recv` frees a slot it returns `true`. Single fiber, so fully deterministic on both.
#[test]
fn bounded_channel_try_send_full_returns_false_both_engines() {
    let src = "c := Channel[int](1)\n\
               print(c.try_send(1))\n\
               print(c.try_send(2))\n\
               print(c.recv())\n\
               print(c.try_send(3))\n\
               print(c.recv())\n";
    let out = run(src);
    assert_eq!(out, "true\nfalse\n1\ntrue\n3\n");
    assert_eq!(out, run_capture_parallel(src).expect("M:N run"));
}

/// A `send` on a FULL bounded channel with NO possible consumer (top level, no nursery) is a
/// deadlock fault on both engines with a byte-identical message — NOT a silent over-fill.
#[test]
fn bounded_channel_full_send_top_level_deadlocks_both_engines() {
    let src = "c := Channel[int](1)\nc.send(1)\nc.send(2)\nprint(\"unreached\")\n";
    let e = run_err(src);
    assert!(e.contains("send on a full channel"), "serial: {e}");
    let ep = run_capture_parallel(src)
        .expect_err("M:N should fault")
        .message;
    assert_eq!(e, ep, "serial and M:N deadlock text must match");
}

/// Bugs 3/4 — a bounded `send` that parks inside a nursery and then genuinely deadlocks must report a
/// diagnostic that names the ACTUAL stall (a full `send()`), not the recv-only wording. The stuck task
/// is a SENDER blocked on a FULL channel, not a receiver on an empty one; the generic nursery message
/// must name `full send()` so a debugger is not misdirected toward a nonexistent receiver. Parity:
/// both engines emit byte-identical text.
#[test]
fn bounded_channel_nursery_send_deadlock_names_full_send_both_engines() {
    let src = "fn main():\n\
               \x20   c := Channel[int](1)\n\
               \x20   parallel:\n\
               \x20       spawn:\n\
               \x20           c.send(1)\n\
               \x20           c.send(2)\n\
               main()\n";
    let e = run_err(src);
    assert!(
        e.contains("full send()"),
        "serial diagnostic must name the send stall: {e}"
    );
    let ep = run_capture_parallel(src)
        .expect_err("M:N should fault")
        .message;
    assert_eq!(e, ep, "serial and M:N deadlock text must match");
}

/// Bounded fan-out golden: a single producer sends 0..5 into a cap-2 channel while a consumer
/// drains 5 in order. Backpressure (producer parks when full) changes WHICH task runs WHEN but not
/// the value sequence — so the output is byte-identical serial vs M:N. Single producer ⇒ no
/// multi-sender contention nondeterminism.
#[test]
fn bounded_channel_fanout_golden_both_engines() {
    let src = "fn main():\n\
               \x20   c := Channel[int](2)\n\
               \x20   parallel:\n\
               \x20       spawn:\n\
               \x20           for i in range(0, 5):\n\
               \x20               c.send(i)\n\
               \x20           c.close()\n\
               \x20       spawn:\n\
               \x20           for v in c:\n\
               \x20               print(v)\n\
               main()\n";
    let out = run(src);
    assert_eq!(out, "0\n1\n2\n3\n4\n");
    assert_eq!(out, run_capture_parallel(src).expect("M:N run"));
}

// ===== §6d `wait:` SEND-arms (Go-`select` symmetry) =====

/// The combined send-arm golden (`examples/wait_send.chz`): Phase A mixes recv + send + `else` at the
/// top level (source-order winner, deterministic single fiber); Phase B blocks a bounded send-arm in a
/// nursery until the consumer's `recv` frees the cap-1 slot (the M:N park + receiver-wake path). Single
/// producer/consumer + consumer-only output ⇒ byte-identical serial vs M:N.
#[test]
fn golden_wait_send_both_engines() {
    let src = include_str!("../../examples/wait_send.chz");
    let expected = include_str!("../../examples/wait_send.expected");
    let out = run(src);
    assert_eq!(
        out, expected,
        "serial output drifted from wait_send.expected"
    );
    assert_eq!(
        out,
        run_capture_parallel(src).expect("M:N run"),
        "serial/M:N divergence on wait_send"
    );
}

/// Stress the send-arm park + receiver-wake (the delicate M:N scheduler change): a bounded cap-1
/// send-arm producer that parks on every full slot, drained by a single consumer, run many trials on
/// the M:N engine. A lost wakeup (parked producer never re-scheduled after a `recv` frees a slot)
/// would hang or truncate the output — so every trial must reproduce the full ordered sequence.
#[test]
fn wait_send_arm_park_wake_stress_parallel() {
    let src = "fn main():\n\
               \x20   c := Channel[int](1)\n\
               \x20   parallel:\n\
               \x20       spawn:\n\
               \x20           for i in range(0, 12):\n\
               \x20               wait:\n\
               \x20                   c.send(i): pass\n\
               \x20           c.close()\n\
               \x20       spawn:\n\
               \x20           for x in c:\n\
               \x20               print(x)\n\
               main()\n";
    let expected: String = (0..12).map(|i| format!("{i}\n")).collect();
    assert_eq!(run(src), expected); // serial oracle
    for trial in 0..40 {
        assert_eq!(
            run_capture_parallel(src).expect("M:N run"),
            expected,
            "M:N send-arm park/wake diverged on trial {trial}"
        );
    }
}

/// A send-arm on a CLOSED channel is SELECTED (ready) and FAULTS `send on a closed channel` — NOT
/// skipped like a closed recv arm — even with an `else` present (a closed send arm is always ready, so
/// `else` never runs). Byte-identical fault on both engines.
#[test]
fn wait_send_arm_closed_channel_faults_both_engines() {
    let src = "fn main():\n\
               \x20   c := Channel[int](1)\n\
               \x20   c.close()\n\
               \x20   wait:\n\
               \x20       c.send(1): print(\"sent\")\n\
               \x20       else: print(\"idle\")\n\
               main()\n";
    let e = run_err(src);
    assert!(
        e.contains("send on a closed channel"),
        "closed send arm must fault, got: {e}"
    );
    let ep = run_capture_parallel(src)
        .expect_err("M:N should fault")
        .message;
    assert_eq!(e, ep, "serial and M:N closed-send fault text must match");
}

/// A full bounded send-arm reached INSIDE a native callback (`list.map`, `native_reentry > 0`) can
/// only block, and cannot be parked/demoted on either engine — so it FAULTS. The fault text must be
/// byte-identical on serial and M:N (parity): before this fix M:N emitted FULL_SEND_DEADLOCK while
/// serial fell through to the generic "wait on channels that are all empty" deadlock.
#[test]
fn wait_send_arm_in_callback_faults_same_on_both_engines() {
    let src = "c := Channel[int](1)\n\
               fn f(x: int) -> int:\n\
               \x20   wait:\n\
               \x20       c.send(9): pass\n\
               \x20   return x\n\
               fn main():\n\
               \x20   c.send(0)\n\
               \x20   parallel:\n\
               \x20       spawn:\n\
               \x20           print([1].map(f))\n\
               main()\n";
    let serial = run_capture(src).expect_err("serial should fault").message;
    let par = run_capture_parallel(src)
        .expect_err("M:N should fault")
        .message;
    assert_eq!(
        serial, par,
        "wait send-arm in-callback fault text must be byte-identical on both engines"
    );
    assert!(
        serial.contains("send on a full channel"),
        "expected the bounded full-send deadlock message, got: {serial}"
    );
}

/// An UNBOUNDED send-arm is always ready → placed first it wins immediately and `else` never runs.
#[test]
fn wait_unbounded_send_arm_always_ready_both_engines() {
    let src = "fn main():\n\
               \x20   u := Channel[int]()\n\
               \x20   wait:\n\
               \x20       u.send(1): print(\"sent\")\n\
               \x20       else: print(\"idle\")\n\
               \x20   print(\"q={u.recv()}\")\n\
               main()\n";
    assert_eq!(run(src), "sent\nq=1\n");
    assert_eq!(run_capture_parallel(src).expect("M:N run"), "sent\nq=1\n");
}

/// Deterministic source-order tie-break: with BOTH an (earlier) ready recv-arm and a (later) ready
/// send-arm, the earlier recv-arm wins every run; flip the order and the earlier send-arm wins. Never
/// Go-random. Identical on both engines.
#[test]
fn wait_send_source_order_first_ready_wins_both_engines() {
    // recv-arm first, both ready → recv wins (the send never fires: `s` stays empty).
    let recv_first = "fn main():\n\
               \x20   r := Channel[int]()\n\
               \x20   s := Channel[int]()\n\
               \x20   r.send(7)\n\
               \x20   wait:\n\
               \x20       v := r.recv(): print(\"recv {v}\")\n\
               \x20       s.send(1): print(\"sent\")\n\
               \x20   print(\"slen={s.len()}\")\n\
               main()\n";
    assert_eq!(run(recv_first), "recv 7\nslen=0\n");
    assert_eq!(
        run_capture_parallel(recv_first).expect("M:N run"),
        "recv 7\nslen=0\n"
    );
    // send-arm first + ready (unbounded), recv-arm empty → send wins.
    let send_first = "fn main():\n\
               \x20   s := Channel[int]()\n\
               \x20   r := Channel[int]()\n\
               \x20   wait:\n\
               \x20       s.send(9): print(\"sent\")\n\
               \x20       v := r.recv(): print(\"recv {v}\")\n\
               \x20   print(\"q={s.recv()}\")\n\
               main()\n";
    assert_eq!(run(send_first), "sent\nq=9\n");
    assert_eq!(
        run_capture_parallel(send_first).expect("M:N run"),
        "sent\nq=9\n"
    );
}

/// Bugs 1/3 — `try_send` on a bounded channel must be ATOMIC (check-space + enqueue under the same
/// lock the blocking `send` uses), or two concurrent M:N `try_send`s both see space and both push,
/// over-filling past `cap`. The invariant `len <= cap` must hold on every engine. Many trials of
/// 16 concurrent `try_send`s into a fresh cap-1 channel that no one drains: with the atomic path at
/// most ONE succeeds per trial, so `len` is never > 1. Buggy (non-atomic) M:N over-fills on ~1-in-5
/// trials → `over > 0` (reliably RED across 200 trials). Serial is single-thread so it is always 0.
#[test]
fn bounded_channel_try_send_atomic_no_overfill_both_engines() {
    let src = "fn producer(c: Channel[int]):\n\
               \x20   c.try_send(1)\n\
               fn main():\n\
               \x20   over := 0\n\
               \x20   for trial in range(0, 200):\n\
               \x20       c := Channel[int](1)\n\
               \x20       parallel:\n\
               \x20           for k in range(0, 16):\n\
               \x20               spawn producer(c)\n\
               \x20       if c.len() > 1:\n\
               \x20           over = over + 1\n\
               \x20   print(over)\n\
               main()\n";
    assert_eq!(run(src), "0\n");
    assert_eq!(run_capture_parallel(src).expect("M:N run"), "0\n");
}

/// Bug 2 — a full bounded `send` issued on the INLINE outermost-`parallel:` builder VM (`self.mn ==
/// None`, sched held only in `mn_enlist_sched`) has NO worker loop to drive `send_suspend` →
/// `Disp::SendPark`, so it must NOT snapshot-park: parking there leaks `send_suspend` set forever
/// (`paused()` stays true → the main VM silently halts / hangs). It must FAULT with the shared
/// full-channel deadlock message instead (the documented inline-owner-never-parks invariant).
///
/// `inner()`'s nested join early-enlists the OUTER nursery (seeding sibling O) and holds the sched in
/// `mn_enlist_sched`; the outer body then fills a cap-1 channel `t` (nobody drains it — O recvs a
/// DIFFERENT channel) and `t.send(42)` is full ⇒ a non-parkable inline-builder send ⇒ fault, not
/// hang. M:N-only (case A is M:N-only), 30s watchdog catches the buggy silent-halt/hang.
#[test]
fn bounded_channel_inline_builder_full_send_faults_not_hang() {
    let src = "fn inner():\n\
               \x20   spawn:\n\
               \x20       print(\"inner ran\")\n\
               fn main():\n\
               \x20   t := Channel[int](1)\n\
               \x20   g := Channel[int]()\n\
               \x20   parallel:\n\
               \x20       spawn:\n\
               \x20           g.recv()\n\
               \x20       inner()\n\
               \x20       t.try_send(7)\n\
               \x20       t.send(42)\n\
               \x20   print(\"done\")\n\
               main()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let e = r.expect_err("full inline-builder send must fault, not silently complete");
            assert!(
                e.message.contains("send on a full channel"),
                "unexpected fault: {}",
                e.message
            );
        }
        Err(_) => panic!("hung — inline-builder full send parked and leaked send_suspend (bug 2)"),
    }
}

// ----- std.concurrency.pmap: scoped parallel-map helpers (Item 2) -----

/// Run `src` from a temp entry file on BOTH engines (a real graph resolve, so `import … from
/// std.concurrency.pmap` pulls the embedded module) and assert identical stdout + clean run.
#[cfg(test)]
fn pmap_both(tag: &str, src: &str) -> String {
    let entry = write_temp_chz(tag, src);
    let (s_out, _e, s_res, _c) = run_file(&entry);
    let (m_out, _e2, m_res, _c2) = run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(s_res.is_ok(), "serial faulted: {s_res:?}");
    assert!(m_res.is_ok(), "M:N faulted: {m_res:?}");
    assert_eq!(s_out, m_out, "serial vs M:N output diverged");
    s_out
}

/// `pmap` returns results in SUBMISSION order (sort-by-index, never completion order) —
/// byte-identical serial vs M:N. Uses the real embedded std module, so it also proves discovery.
#[test]
fn pmap_submission_order_both_engines() {
    let src = "import pmap from std.concurrency.pmap\n\
               fn main():\n\
               \x20   print(pmap([1, 2, 3, 4], fn(x: int) -> int: x * 2))\n\
               main()\n";
    assert_eq!(pmap_both("pmap_order", src), "[2, 4, 6, 8]\n");
}

/// `pmap_limited` gives the same submission-order result as `pmap` regardless of the in-flight cap.
#[test]
fn pmap_limited_matches_pmap_both_engines() {
    let src = "import pmap_limited from std.concurrency.pmap\n\
               fn main():\n\
               \x20   print(pmap_limited([1, 2, 3, 4, 5, 6, 7, 8], fn(x: int) -> int: x + 100, 2))\n\
               main()\n";
    assert_eq!(
        pmap_both("pmap_limited", src),
        "[101, 102, 103, 104, 105, 106, 107, 108]\n"
    );
}

/// `pmap_limited`'s token bucket actually BOUNDS in-flight tasks: an Atomic max-in-flight probe never
/// exceeds `limit`. (Serial runs tasks one at a time so max is trivially <= limit; the M:N run is the
/// real test — the semaphore caps concurrent f-execution.)
#[test]
fn pmap_limited_bounds_in_flight_both_engines() {
    // A nested named fn (multi-statement) captures the two Atomics by-ref; both cross the airlock as
    // shared handles, so every task sees the one live counter/max pair. `cur` is incremented on entry
    // and decremented on exit, so it never exceeds the number of tasks running `f` at once — which the
    // token bucket caps at `limit`. `mx` records the peak, so `mx <= limit` proves the bound holds.
    let src = "import std.concurrency\n\
               import pmap_limited from std.concurrency.pmap\n\
               fn main():\n\
               \x20   cur := Atomic(0)\n\
               \x20   mx := Atomic(0)\n\
               \x20   fn probe(x: int) -> int:\n\
               \x20       n := cur.add(1)\n\
               \x20       m := mx.load()\n\
               \x20       if n > m:\n\
               \x20           mx.store(n)\n\
               \x20       cur.sub(1)\n\
               \x20       return x\n\
               \x20   r := pmap_limited([1, 2, 3, 4, 5, 6, 7, 8], probe, 2)\n\
               \x20   print(mx.load() <= 2)\n\
               main()\n";
    assert_eq!(pmap_both("pmap_bound", src), "true\n");
}

/// `std.concurrency.task` — `submit_task` returns a `Task[T]` handle whose `.get()`, awaited in
/// SUBMISSION order, is byte-identical serial vs M:N (the value is deterministic; only timing varies).
#[test]
fn task_submit_get_submission_order_both_engines() {
    let src = "import std.concurrency\n\
               import submit_task from std.concurrency.task\n\
               fn work(n: int) -> int:\n\
               \x20   return n * n\n\
               fn main():\n\
               \x20   ex := Executor()\n\
               \x20   ts := []\n\
               \x20   for i in range(1, 6):\n\
               \x20       x := i\n\
               \x20       ts.push(submit_task(ex, fn() -> int: work(x)))\n\
               \x20   ex.shutdown()\n\
               \x20   for t in ts:\n\
               \x20       print(t.get())\n\
               main()\n";
    assert_eq!(pmap_both("task_order", src), "1\n4\n9\n16\n25\n");
}

/// `Task.get()` MEMOIZES: a second `.get()` returns the cached value (it must NOT `recv` the drained
/// one-shot channel again, which would block forever). `.done()` is true once the result is in hand.
#[test]
fn task_get_idempotent_and_done_both_engines() {
    let src = "import std.concurrency\n\
               import submit_task from std.concurrency.task\n\
               fn main():\n\
               \x20   ex := Executor()\n\
               \x20   t := submit_task(ex, fn() -> int: 42)\n\
               \x20   ex.shutdown()\n\
               \x20   print(t.done())\n\
               \x20   print(t.get())\n\
               \x20   print(t.get())\n\
               \x20   print(t.done())\n\
               main()\n";
    assert_eq!(pmap_both("task_idem", src), "true\n42\n42\ntrue\n");
}

/// FIX 2 — `Executor.submit_result[T]` is a BODIED generic method on a native struct; its runtime
/// dispatch goes through `try_native_bodied_method` (newly wired into the `Executor` arm of
/// `do_method_call`). Submit a few results, drain the returned cap-1 channels in SUBMISSION order,
/// byte-identical serial vs M:N.
#[test]
fn executor_submit_result_both_engines() {
    let src = "import std.concurrency\n\
               fn work(n: int) -> int:\n\
               \x20   return n * n\n\
               fn main():\n\
               \x20   ex := Executor()\n\
               \x20   chs := []\n\
               \x20   for i in range(1, 6):\n\
               \x20       x := i\n\
               \x20       chs.push(ex.submit_result(fn() -> int: work(x)))\n\
               \x20   ex.shutdown()\n\
               \x20   for ch in chs:\n\
               \x20       print(ch.recv())\n\
               main()\n";
    assert_eq!(pmap_both("submit_result", src), "1\n4\n9\n16\n25\n");
}

/// Call-flattening × M:N parking: a fiber that `recv`-parks **several flattened plain-function
/// frames deep** (`main → collect → deep_recv ×6`, all `Op::Call`, parking at `ip > 0`) must
/// suspend with its frames intact and, on a sibling `send`, resume through `run_until(0)` and
/// thread the received value back up the flattened chain. Pre-flatten each of those frames was a
/// nested Rust `run_until`; now they share one loop, so resume reads them straight from the heap
/// `frames` Vec. (Closes the coverage gap the review flagged: park deep in *bytecode* frames, not
/// just inside a native HOF callback.)
#[test]
fn parallel_recv_parks_deep_in_flattened_frames_and_resumes() {
    let src = "\
fn deep_recv(ch: Channel[int], depth: int) -> int:
    if depth <= 0:
        return ch.recv()
    return deep_recv(ch, depth - 1)

fn collect(ch: Channel[int], out: Channel[int]):
    out.send(deep_recv(ch, 5))

fn produce(ch: Channel[int], v: int):
    ch.send(v)

fn main():
    ch := Channel[int]()
    out := Channel[int]()
    parallel:
        spawn collect(ch, out)
        spawn produce(ch, 99)
    print(out.recv())

main()
";
    assert_eq!(run_capture_parallel(src).expect("parallel run"), "99\n");
}

/// L7 round 1 — `Channel[int!]` (the `Error`-existential is sendable-bounded) runs identically on
/// both engines: a spawned task sends `Ok`/`Err` over the channel, the parent recvs and matches.
/// Two-engine parity (serial == M:N) on the newly-admitted `Channel[Error-existential]` shape.
#[test]
fn channel_of_error_existential_ok_err_two_engine_parity() {
    let src = "\
fn worker(ch: Channel[int!]):
    ch.send(Ok(7))
    ch.send(Err(\"boom\"))

fn main():
    ch := Channel[int!]()
    parallel:
        spawn worker(ch)
    a := ch.recv()
    b := ch.recv()
    match a:
        Ok(v): print(\"ok {v}\")
        Err(e): print(\"err {e.message()}\")
    match b:
        Ok(v): print(\"ok {v}\")
        Err(e): print(\"err {e.message()}\")

main()
";
    let expected = "ok 7\nerr boom\n";
    assert_eq!(run_capture(src).expect("serial run"), expected);
    assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
}

/// L7 round 3, Test 1 (Option-B regression) — an INFERRED return (no `-> int!`/`-> Result[..]`
/// annotation) that yields both an `Ok(int)` and an `Err(GErr(..))` branch, where `GErr` satisfies
/// `Error` but is itself non-sendable (holds a non-`Error` protocol field `Odd`), stays legal used
/// PURELY IN-TASK (no channel/spawn): `T` pins to `int`, the `E` slot infers `GErr` and is PRESERVED
/// concrete (`Result[int, GErr]`), not widened/laundered to `Error`. This is the whole point of
/// Option B — inference never triggers the sendable-bounded widening (only an explicit `Error`/`int!`
/// annotation, or a `Channel.send`, does), so it must type-check clean and run byte-identically on
/// both engines.
#[test]
fn inferred_non_sendable_error_in_task_ok_both_engines() {
    let src = "\
protocol Odd:
    fn tag(self) -> int

struct Impl:
    fn tag(self) -> int:
        return 1

struct GErr:
    w: Odd
    fn message(self) -> str:
        return \"x\"

fn f(x: int):
    if x == 0:
        return Ok(1)
    return Err(GErr(Impl()))

fn main():
    match f(0):
        Ok(v): print(v)
        Err(e): print(e.message())
    match f(1):
        Ok(v): print(v)
        Err(e): print(e.message())

main()
";
    let expected = "1\nx\n";
    assert_eq!(run_capture(src).expect("serial run"), expected);
    assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
}

/// D2a: an M:N fiber carries its OWN heap (share-nothing). `swap_ctx` swaps that heap with the
/// host `Vm`'s when the fiber is scheduled in, and back out when it parks — the prerequisite for
/// D2b parking a fiber across worker threads. Round-trip: a fiber heap holding `"fiber-obj"` and
/// a host heap holding `"vm-obj"` exchange on swap-in and restore on swap-out.
#[test]
fn swap_ctx_round_trips_an_mn_fiber_heap() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.parallel = true; // M:N fibers only carry their own heap under --parallel (decision A).
    let hv = vm.heap.alloc(Obj::Str("vm-obj".into()));

    let mut fiber_heap = Heap::new();
    let hf = fiber_heap.alloc(Obj::Str("fiber-obj".into()));
    let mut ctx = FiberCtx {
        heap: Some(fiber_heap),
        ..FiberCtx::default()
    };

    // Swap the fiber in: self.heap becomes the fiber's heap; the host heap parks in the ctx.
    vm.swap_ctx(&mut ctx);
    assert!(matches!(vm.heap.get(hf), Obj::Str(s) if &s[..] == "fiber-obj"));
    assert!(matches!(ctx.heap.as_ref().unwrap().get(hv), Obj::Str(s) if &s[..] == "vm-obj"));

    // Swap back out: the host heap is restored, the fiber keeps its own heap.
    vm.swap_ctx(&mut ctx);
    assert!(matches!(vm.heap.get(hv), Obj::Str(s) if &s[..] == "vm-obj"));
    assert!(matches!(ctx.heap.as_ref().unwrap().get(hf), Obj::Str(s) if &s[..] == "fiber-obj"));
}

/// D2b: an M:N fiber carries its own per-task SIDE state too — `out`/`stderr` (Decision-F output
/// buffers) and the heap-keyed roots `module_objs`/`module_faulted`/`executors` (each a `GcRef`
/// into the fiber's own heap, so they MUST travel atomically with that heap). `swap_ctx` round-
/// trips all of them alongside the heap, gated on `heap.is_some()` so a cooperative fiber
/// (`heap: None`) leaves the shell's side state untouched (byte-identical, asserted separately).
#[test]
fn mn_swap_ctx_round_trips_fiber_side_state() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.parallel = true;
    vm.out.extend_from_slice(b"host-out");
    vm.stderr.extend_from_slice(b"host-err");
    let host_mod = vm.heap.alloc(Obj::Str("host-mod".into()));
    let host_exec = vm.heap.alloc(Obj::Str("host-exec".into()));
    vm.module_objs = vec![host_mod];
    vm.module_faulted = vec![true];
    vm.executors = vec![host_exec];
    // M19 Phase 3 — the intern cache is heap-keyed too; it must round-trip with the heap.
    let host_str = vm.heap.alloc(Obj::Str("host-str".into()));
    vm.str_intern.insert(0x10, host_str);

    let mut fiber_heap = Heap::new();
    let fib_mod = fiber_heap.alloc(Obj::Str("fiber-mod".into()));
    let fib_exec = fiber_heap.alloc(Obj::Str("fiber-exec".into()));
    let fib_str = fiber_heap.alloc(Obj::Str("fiber-str".into()));
    let mut ctx = FiberCtx {
        heap: Some(fiber_heap),
        out: b"fiber-out".to_vec(),
        stderr: b"fiber-err".to_vec(),
        module_objs: vec![fib_mod],
        module_faulted: vec![false],
        executors: vec![fib_exec],
        str_intern: fxhash::FxHashMap::from_iter([(0x20usize, fib_str)]),
        ..FiberCtx::default()
    };

    // Schedule in: the fiber's side state becomes live; the shell's parks into the ctx.
    vm.swap_ctx(&mut ctx);
    assert_eq!(vm.out, b"fiber-out");
    assert_eq!(vm.stderr, b"fiber-err");
    assert_eq!(vm.module_objs, vec![fib_mod]);
    assert_eq!(vm.module_faulted, vec![false]);
    assert_eq!(vm.executors, vec![fib_exec]);
    assert_eq!(vm.str_intern.get(&0x20), Some(&fib_str));
    assert_eq!(vm.str_intern.get(&0x10), None);
    assert_eq!(ctx.out, b"host-out");
    assert_eq!(ctx.module_objs, vec![host_mod]);
    assert_eq!(ctx.str_intern.get(&0x10), Some(&host_str));

    // Park out: the shell's side state is restored; the fiber keeps its own.
    vm.swap_ctx(&mut ctx);
    assert_eq!(vm.out, b"host-out");
    assert_eq!(vm.stderr, b"host-err");
    assert_eq!(vm.module_objs, vec![host_mod]);
    assert_eq!(vm.module_faulted, vec![true]);
    assert_eq!(vm.executors, vec![host_exec]);
    assert_eq!(vm.str_intern.get(&0x10), Some(&host_str));
    assert_eq!(ctx.out, b"fiber-out");
    assert_eq!(ctx.module_objs, vec![fib_mod]);
    assert_eq!(ctx.str_intern.get(&0x20), Some(&fib_str));
}

/// Per-connection spawn: a fiber running an eager `parallel:` body can PARK (its acceptor blocks
/// on `accept`) between `EnterNursery` and `JoinNursery`, so the open eager scope — the live
/// inner sched + its monotonic spawn index — MUST travel with the fiber across `swap_ctx`, just
/// like `nurseries`. Otherwise the scope leaks onto whatever fiber the shell schedules next.
#[test]
fn eager_scope_round_trips_with_fiber_ctx() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.parallel = true;
    let host_sched = Arc::new(mk_sched(0));
    vm.eager_scheds.push(Some(EagerScope {
        sched: Arc::clone(&host_sched),
        cancel: Arc::new(AtomicBool::new(false)),
        drainer: None,
    }));

    let fiber_sched = Arc::new(mk_sched(0));
    let mut ctx = FiberCtx {
        eager_scheds: vec![Some(EagerScope {
            sched: Arc::clone(&fiber_sched),
            cancel: Arc::new(AtomicBool::new(false)),
            drainer: None,
        })],
        ..FiberCtx::default()
    };

    // Schedule the fiber in: its eager scope becomes live; the host's parks into the ctx.
    vm.swap_ctx(&mut ctx);
    assert_eq!(vm.eager_scheds.len(), 1);
    assert!(
        Arc::ptr_eq(&vm.eager_scheds[0].as_ref().unwrap().sched, &fiber_sched),
        "the fiber's eager scope is now live"
    );
    assert!(
        Arc::ptr_eq(&ctx.eager_scheds[0].as_ref().unwrap().sched, &host_sched),
        "the host's scope parked into the ctx"
    );

    // Park the fiber out: the host's scope is restored; the fiber keeps its own.
    vm.swap_ctx(&mut ctx);
    assert!(
        Arc::ptr_eq(&vm.eager_scheds[0].as_ref().unwrap().sched, &host_sched),
        "host scope restored"
    );
    assert!(Arc::ptr_eq(
        &ctx.eager_scheds[0].as_ref().unwrap().sched,
        &fiber_sched
    ));
}

/// D2b / Task 1 companion to [`swap_ctx_leaves_heap_untouched_for_cooperative_fiber`]: a cooperative
/// fiber (`heap: None`) still leaves the shell's HEAP-GATED side state (`out`/`executors`) untouched —
/// but `module_objs`/`module_faulted` now swap UNCONDITIONALLY (Task 1: a serial child carries its own
/// deep-copied module view). So swapping in a child with an empty view parks the host's REAL modules
/// into the ctx (where `root_ctx` keeps them rooted).
#[test]
fn mn_swap_ctx_swaps_module_objs_but_not_heap_gated_state_for_cooperative_fiber() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.out.extend_from_slice(b"host-out");
    let host_mod = vm.heap.alloc(Obj::Str("host-mod".into()));
    vm.module_objs = vec![host_mod];
    let mut ctx = FiberCtx::default();
    vm.swap_ctx(&mut ctx);
    // Heap-gated state (out) stays on the shell for a cooperative fiber.
    assert_eq!(vm.out, b"host-out");
    assert!(
        ctx.out.is_empty(),
        "swap must not give a cooperative fiber heap-gated side state"
    );
    // Task 1 — module_objs swaps: the host's real modules parked into the ctx, the shell now holds the
    // child's (empty) view.
    assert!(vm.module_objs.is_empty());
    assert_eq!(ctx.module_objs, vec![host_mod]);
}

// ---- D2b MnSched scheduler mechanics (Step 2 — hand-built fibers, no bytecode) ----

fn dl_err() -> RuntimeError {
    RuntimeError {
        message: DEADLOCK_MSG.to_string(),
        span: Span::RUNTIME,
        is_assert: false,
        is_over_memory: false,
        is_timed_out: false,
    }
}
fn mk_sched(total: usize) -> MnSched {
    // 4 worker slots by default — enough for the multi-`wid` steal tests; single-worker tests
    // just use `wid` 0.
    // `mem_cap` 0 = the `--max-heap` cap off, which is what every fixture here wants.
    MnSched::new(total, 4, Arc::new(AtomicBool::new(false)), dl_err(), 0)
}
fn mk_fiber(task_index: usize) -> Fiber {
    Fiber {
        ctx: FiberCtx::default(),
        state: FiberState::Ready,
        task_index,
        scope_id: 0,
        span: Span::RUNTIME,
        resume_native: None,
    }
}
/// An UNSTARTED fiber (`Pending`) — what `inject`/`seed` require so `run_one_fiber` runs the task
/// body via `start_task` (a `Ready` fiber is treated as a resume and runs no body).
fn mk_pending_fiber(task_index: usize) -> Fiber {
    let task = PendingCall::Call {
        callee: Value::nil(),
        args: Vec::new(),
        span: Span::RUNTIME,
    };
    Fiber {
        ctx: FiberCtx::default(),
        state: FiberState::Pending(task),
        task_index,
        scope_id: 0,
        span: Span::RUNTIME,
        resume_native: None,
    }
}
fn empty_core() -> Arc<ChannelCore> {
    Arc::new(ChannelCore::default())
}
fn core_key(core: &Arc<ChannelCore>) -> usize {
    Arc::as_ptr(core) as usize
}
fn take_run(s: &MnSched) -> Fiber {
    // tick=1 → not a periodic-global-check schedule, so the normal own-local-then-global order
    // applies (what the existing unit tests assert).
    match s.take_runnable(0, 1, 0) {
        Take::Run(f) => f,
        Take::Stop => panic!("expected a runnable fiber, got Stop"),
    }
}

/// D2b/U1: `take_runnable` pops the shared run queue in FIFO order and marks each popped fiber
/// `running`.
#[test]
fn mnsched_take_runnable_pops_in_order_and_counts_running() {
    let sched = mk_sched(2);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    assert_eq!(take_run(&sched).task_index, 0);
    assert_eq!(take_run(&sched).task_index, 1);
    assert_eq!(sched.lock().running, 2);
}

/// D4a: `runnable` is the authoritative count of runnable (queued, not running/parked/done)
/// fibers — in D2b's single-queue world it mirrors `runq.len()` exactly, but it is maintained as
/// an atomic so D4b's per-worker split (local rings + global, no single queue to `.len()`) can
/// keep using it for the deadlock predicate. This pins the bump/decrement discipline: seed +N,
/// pop −1 (runnable→running), park unchanged (running→parked), send_wake +woken (parked→ready),
/// finish unchanged (running→done).
#[test]
fn mnsched_runnable_tracks_single_queue() {
    let sched = mk_sched(3);
    let core = empty_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0), mk_fiber(1), mk_fiber(2)]);
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        3,
        "seed bumps runnable"
    );
    let f0 = take_run(&sched);
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "pop transitions runnable→running"
    );
    sched.park(key, &core, f0);
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "park transitions running→parked (no change)"
    );
    sched.send_wake(key, &core, WireValue::Int(7));
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        3,
        "send_wake transitions parked→ready"
    );
    let f = take_run(&sched);
    assert_eq!(sched.runnable.load(Ordering::Relaxed), 2);
    sched.finish(
        f.task_index,
        0,
        TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "finish transitions running→done (no change)"
    );
    // The invariant: with no per-worker locals populated, runnable == global.len() at quiescence.
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        sched.lock().global.len()
    );
}

/// Per-connection spawn: `inject` adds a task to a LIVE sched — it grows `total` + `slots`
/// (so the dynamically-spawned handler gets a Decision-F outcome slot) and queues the fiber
/// runnable, all under one core lock (the `complete_offload` twin). This is what lifts the
/// "fixed total — no spawn-after-join" restriction.
#[test]
fn mnsched_inject_grows_total_and_slots() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]); // total 1, slots.len 1, runnable 1
    sched.inject(mk_pending_fiber(1), 0);
    let c = sched.lock();
    assert_eq!(c.scopes[0].total, 2, "inject grows the scope's total");
    assert_eq!(c.slots.len(), 2, "inject grows the outcome-slot vec");
    assert_eq!(c.global.len(), 2, "the injected fiber is queued runnable");
    drop(c);
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "inject runnable-accounts the new fiber"
    );
}

/// Per-connection spawn: injecting a runnable fiber into a sched where every existing fiber is
/// parked must VETO the deadlock predicate — `total += 1` is paired with `runnable += 1` under
/// one lock, so the new fiber is immediately accounted and `is_deadlocked` sees `runnable > 0`.
#[test]
fn mnsched_inject_does_not_false_deadlock() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running 1, runnable 0
    let core = empty_core();
    sched.park(core_key(&core), &core, f0); // parked 1, running 0, runnable 0 → deadlock
    {
        let c = sched.lock();
        assert!(
            sched.is_deadlocked(&c),
            "all parked, nothing runnable/inflight = deadlock"
        );
    }
    sched.inject(mk_pending_fiber(1), 0); // runnable 1
    {
        let c = sched.lock();
        assert!(
            !sched.is_deadlocked(&c),
            "an injected runnable fiber vetoes the deadlock fire"
        );
    }
}

// ----- Cross-nursery flat scheduler (M:N): JoinScope / scope-scoped owner stop -----

/// `register_scope` appends a `JoinScope` and grows the flat slots: scope 0 (built by `new`) at
/// base 0, the next at base = previous total, slots length = sum of totals.
#[test]
fn mn_register_scope_appends_and_offsets_slots() {
    let sched = mk_sched(2); // scope 0: total 2, base 0
    let s1 = sched.register_scope(3, Arc::new(AtomicBool::new(false)), Vec::new());
    assert_eq!(s1, 1, "second scope id");
    let c = sched.lock();
    assert_eq!(c.scopes.len(), 2);
    assert_eq!(c.scopes[0].base_index, 0);
    assert_eq!(
        c.scopes[1].base_index, 2,
        "scope 1 starts after scope 0's 2 slots"
    );
    assert_eq!(c.slots.len(), 5, "flat slots grew to 2 + 3");
}

/// Scope-scoped owner stop: an owner whose OWN scope is done returns `Stop` (queue empty) even
/// while ANOTHER scope is still in flight (a running fiber → no global terminate / deadlock) — the
/// load-bearing case-A behavior (the nested owner returns the instant its scope completes, having
/// drained the global queue meanwhile). The owner of the NOT-done scope, by contrast, does not stop
/// on the same state (it would park to wait for its scope) — asserted via the fast-path scalars.
#[test]
fn mn_owner_stops_on_own_scope_not_global() {
    let sched = mk_sched(1); // scope 0: total 1
    let _s1 = sched.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new()); // scope 1: total 1
    {
        let mut c = sched.lock();
        c.scopes[0].done = 1; // scope 0 complete
        c.running = 1; // scope 1's fiber is running on some worker → not terminate, not deadlock
    }
    // The owner of the DONE scope 0 stops immediately (queue empty, its scope done).
    assert!(
        matches!(sched.take_runnable(0, 1, 0), Take::Stop),
        "owner of done scope 0 stops"
    );
    // The global terminate was NOT set (scope 1 still in flight), so the stop was scope-scoped, not
    // a global teardown — a SENTINEL would keep going (verified: terminate is still false).
    assert!(
        !sched.lock().terminate,
        "owner stop is scope-scoped, not a global terminate"
    );
}

/// `finish` routes the per-fiber outcome to the FIBER's scope `done` + the flat slot, and sets
/// global `terminate` only when EVERY scope is done.
#[test]
fn mn_finish_routes_done_to_scope_via_fiber() {
    let sched = mk_sched(1); // scope 0: total 1, base 0
    let _s1 = sched.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new()); // scope 1: total 1, base 1
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let _f0 = take_run(&sched); // scope 0's fiber (task_index 0)
    let _f1 = take_run(&sched); // queued as task_index 1
    sched.finish(
        1,
        1,
        TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        },
    ); // finish scope 1's fiber (flat slot 1)
    {
        let c = sched.lock();
        assert_eq!(c.scopes[1].done, 1, "scope 1 done bumped");
        assert_eq!(c.scopes[0].done, 0, "scope 0 untouched");
        assert!(c.slots[1].is_some(), "flat slot 1 set");
        assert!(!c.terminate, "not all scopes done yet");
    }
    sched.finish(
        0,
        0,
        TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    assert!(
        sched.lock().terminate,
        "global terminate once every scope is done"
    );
}

/// `take_scope_slots` drains only ONE scope's contiguous sub-range in task order, leaving others.
#[test]
fn mn_take_scope_slots_drains_only_its_range() {
    let sched = mk_sched(2); // scope 0: base 0, total 2
    let _s1 = sched.register_scope(2, Arc::new(AtomicBool::new(false)), Vec::new()); // scope 1: base 2, total 2
    {
        let mut c = sched.lock();
        for i in 0..4 {
            c.slots[i] = Some(TaskOutcome::Cancelled {
                out: Vec::new(),
                stderr: Vec::new(),
            });
        }
    }
    let s0 = sched.take_scope_slots(0);
    assert_eq!(s0.len(), 2, "scope 0 sub-range");
    assert!(s0.iter().all(|x| x.is_some()));
    {
        let c = sched.lock();
        assert!(
            c.slots[0].is_none() && c.slots[1].is_none(),
            "scope 0 slots taken"
        );
        assert!(
            c.slots[2].is_some() && c.slots[3].is_some(),
            "scope 1 slots intact"
        );
    }
}

/// §6d M:N wait-park (TDD step 2): a plain `recv` park (the 1-key `ParkedEntry::Recv` case)
/// round-trips through the refactored `parked` map — park a fiber, `send_wake` it, and it lands
/// back on `global` as Ready with `parked_n` returned to 0. Pins that the refactor keeps the
/// recv-park path byte-identical at the scheduler level.
#[test]
fn mn_parked_entry_recv_roundtrips() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running 1
    let core = empty_core();
    let key = core_key(&core);
    sched.park(key, &core, f0); // parked 1, running 0
    assert_eq!(
        sched.lock().parked_n,
        1,
        "recv park accounts one parked fiber"
    );
    sched.send_wake(key, &core, WireValue::Int(5));
    assert_eq!(
        sched.lock().parked_n,
        0,
        "send_wake un-parks the recv fiber"
    );
    let woke = take_run(&sched);
    assert_eq!(
        woke.task_index, 0,
        "the recv-parked fiber is back on the run queue"
    );
}

/// §6d M:N wait-park (TDD step 3): a wait fiber filed under keys [k1,k2,k3] as ONE shared
/// `WaitPark` token. A `send_wake` on the middle key claims it exactly once (moves the single
/// fiber to Ready, `claimed==true`, `parked_n` 1→0) AND sweeps the stale tokens out of k1 and k3.
/// A follow-up `send_wake` on either swept key is a no-op (no double-wake, no panic).
#[test]
fn mn_wait_park_first_waker_claims_and_sweeps() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running 1
    let c1 = empty_core();
    let c2 = empty_core();
    let c3 = empty_core();
    let (k1, k2, k3) = (core_key(&c1), core_key(&c2), core_key(&c3));
    sched.park_wait(
        vec![
            (k1, Arc::clone(&c1), false),
            (k2, Arc::clone(&c2), false),
            (k3, Arc::clone(&c3), false),
        ],
        f0,
    );
    assert_eq!(
        sched.lock().parked_n,
        1,
        "a wait fiber on N keys counts as ONE parked fiber"
    );
    // Wake on the MIDDLE key.
    sched.send_wake(k2, &c2, WireValue::Int(99));
    {
        let c = sched.lock();
        assert_eq!(
            c.parked_n, 0,
            "claiming the wait fiber returns parked_n to 0"
        );
        assert!(
            c.parked.get(&k1).is_none_or(|v| v.is_empty()),
            "k1 token swept"
        );
        assert!(
            c.parked.get(&k3).is_none_or(|v| v.is_empty()),
            "k3 token swept"
        );
    }
    assert_eq!(
        sched.lock().global.len(),
        1,
        "exactly one fiber re-queued by the wake"
    );
    let woke = take_run(&sched);
    assert_eq!(
        woke.task_index, 0,
        "the wait fiber is back on the run queue exactly once"
    );
    // A later send to a swept key must be a clean no-op (the token is gone): no new runnable fiber.
    sched.send_wake(k1, &c1, WireValue::Int(1));
    sched.send_wake(k3, &c3, WireValue::Int(3));
    let c = sched.lock();
    assert_eq!(c.parked_n, 0, "no double-wake from swept buckets");
    assert_eq!(
        c.global.len(),
        0,
        "no second wake of the already-moved fiber"
    );
}

/// WAIT-2/WAIT-3 unit guard — the timed-park files the timer channel as an ORDINARY arm bucket,
/// so the timer's own deadline `send_wake` claims+sweeps the fiber exactly like a data send. A
/// LATE `send_wake` on the swept data key (a sibling that landed after the alarm already won) is a
/// clean no-op. Pins the "timer-arm-as-bucket" design invariant the VM timed-park relies on.
#[test]
fn mn_wait_park_timer_self_send_claims_then_late_send_noop() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running 1
    let timer_core = empty_core(); // stands in for the timer channel's bucket
    let data_core = empty_core();
    let (timer_key, data_key) = (core_key(&timer_core), core_key(&data_core));
    sched.park_wait(
        vec![
            (timer_key, Arc::clone(&timer_core), false),
            (data_key, Arc::clone(&data_core), false),
        ],
        f0,
    );
    assert_eq!(
        sched.lock().parked_n,
        1,
        "wait on [timer,data] is ONE parked fiber"
    );
    // The timer's deadline job fires: `send_wake(true)` on the timer key claims + sweeps.
    sched.send_wake(timer_key, &timer_core, WireValue::Bool(true));
    {
        let c = sched.lock();
        assert_eq!(
            c.parked_n, 0,
            "the timer deadline send claims the wait fiber"
        );
        assert!(
            c.parked.get(&data_key).is_none_or(|v| v.is_empty()),
            "data_key token swept by the timer win"
        );
        assert_eq!(c.global.len(), 1, "exactly one fiber re-queued");
    }
    assert_eq!(
        take_run(&sched).task_index,
        0,
        "the wait fiber is back on the run queue exactly once"
    );
    // A LATE send on the swept data key (a sibling that lost the race) is a clean no-op.
    sched.send_wake(data_key, &data_core, WireValue::Int(9));
    let c = sched.lock();
    assert_eq!(c.parked_n, 0, "no double-wake from the swept data bucket");
    assert_eq!(
        c.global.len(),
        0,
        "the late send does not re-wake the already-moved fiber"
    );
}

/// §6d M:N wait-park: `close_wake` claims+sweeps a wait fiber identically to `send_wake` (a close
/// has no value but must still unblock the waiter so it re-polls and skips the closed arm).
#[test]
fn mn_wait_park_close_wake_claims_and_sweeps() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched);
    let c1 = empty_core();
    let c2 = empty_core();
    let (k1, k2) = (core_key(&c1), core_key(&c2));
    sched.park_wait(
        vec![(k1, Arc::clone(&c1), false), (k2, Arc::clone(&c2), false)],
        f0,
    );
    assert_eq!(sched.lock().parked_n, 1);
    sched.close_wake(k1, &c1);
    {
        let c = sched.lock();
        assert_eq!(c.parked_n, 0, "close_wake un-parks the wait fiber");
        assert!(
            c.parked.get(&k2).is_none_or(|v| v.is_empty()),
            "k2 token swept by close"
        );
    }
    assert_eq!(take_run(&sched).task_index, 0);
}

/// §6d M:N wait-park: a lone wait-parked fiber on N empty keys with nothing running/runnable/
/// inflight IS a deadlock — `park_wait` must increment `parked_n` so `is_deadlocked` fires (the
/// accounting choice: one fiber, +1 regardless of key count).
#[test]
fn mn_wait_park_lone_fiber_is_deadlock() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched);
    let c1 = empty_core();
    let c2 = empty_core();
    sched.park_wait(
        vec![
            (core_key(&c1), Arc::clone(&c1), false),
            (core_key(&c2), Arc::clone(&c2), false),
        ],
        f0,
    );
    let c = sched.lock();
    assert!(
        sched.is_deadlocked(&c),
        "a lone wait-parked fiber with no sender is a deadlock"
    );
}

/// W7-2 — the `close()`-vs-`wait:`-park lost wakeup. `op_wait_poll` treats a closed+EMPTY recv arm
/// as DEAD (skipped, counted only toward `all_closed`), so if EVERY arm is dead the poll faults
/// "wait: all channels closed". `park_wait`'s gap re-check must classify arms the SAME way: a
/// `close()` landing between the empty poll and the park runs `close_wake` against a bucket that is
/// still empty, so the re-check is the only thing left that can see it. Pre-fix the recv predicate
/// was `!g.is_empty()` alone → not ready → park on a key nothing will ever wake → the deadlock
/// detector (correctly) reaps a genuinely unreachable fiber, i.e. a SPURIOUS `deadlock:` fault.
#[test]
fn mn_park_wait_all_recv_arms_closed_requeues_instead_of_parking() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running 1
    let c1 = empty_core();
    let c2 = empty_core();
    for c in [&c1, &c2] {
        c.q.lock().unwrap().closed = true;
    }
    sched.park_wait(
        vec![
            (core_key(&c1), Arc::clone(&c1), false),
            (core_key(&c2), Arc::clone(&c2), false),
        ],
        f0,
    );
    {
        let c = sched.lock();
        assert_eq!(c.parked_n, 0, "an all-dead wait must NOT park");
        assert_eq!(c.global.len(), 1, "it is requeued Ready to re-poll");
    }
    assert_eq!(
        take_run(&sched).task_index,
        0,
        "the fiber re-runs WaitPoll, which faults `wait: all channels closed`"
    );
}

/// W7-2 anti-over-fire fence (green before AND after the fix): ONE closed arm among LIVE arms must
/// still PARK. Treating a closed+empty recv arm as *ready* was tried and reverted (parity-perf-0):
/// the re-poll SKIPS that arm, finds the live one still empty, and re-parks → requeue→re-poll→re-park
/// live-lock. Only an ALL-dead wait may requeue (its re-poll terminates in the all-closed fault).
/// Also pins that the deadlock detector stays sharp on the partially-closed park.
#[test]
fn mn_park_wait_one_closed_one_live_still_parks_and_is_deadlock() {
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched);
    let closed = empty_core();
    closed.q.lock().unwrap().closed = true;
    let live = empty_core();
    sched.park_wait(
        vec![
            (core_key(&closed), Arc::clone(&closed), false),
            (core_key(&live), Arc::clone(&live), false),
        ],
        f0,
    );
    let c = sched.lock();
    assert_eq!(c.parked_n, 1, "a live arm remains — the fiber parks");
    assert!(
        sched.is_deadlocked(&c),
        "a lone wait-parked fiber with no possible waker is still a deadlock"
    );
}

/// W7-2 end-to-end (RACY — loop it): a `close()` racing a sibling's `wait:` park must never produce
/// a spurious `deadlock:` fault. `--serial` never fails; pre-fix the M:N engine lost the wakeup
/// whenever the `close` landed inside the poll→park window.
///
/// PRE-FIX MEASURED (this exact 200-iteration loop, `park_wait` reverted to the pre-fix predicate,
/// `cargo test --lib -- --test-threads=1`, 12-core box): **43 / 45 / 50 / 56 failures out of 200**
/// over four consecutive runs — ~25%, so ≥1 failure is essentially certain and the fence really is
/// RED without the fix. It is a RATE, not a fixed count; quote the range, not a single number.
#[test]
fn wait_park_close_race_no_spurious_deadlock_parallel() {
    let src = "fn w(a: Channel[int]):\n    r := recover:\n        wait:\n            v := a.recv(): print(\"got\", v)\n    print(\"waiter done\")\nfn main():\n    a := Channel[int]()\n    parallel:\n        spawn w(a)\n        spawn: a.close()\n    print(\"end\")\nmain()\n";
    let mut failures = Vec::new();
    for i in 0..200 {
        match run_capture_parallel(src) {
            Ok(out) => {
                if !out.contains("waiter done") || !out.contains("end") {
                    failures.push(format!("iter {i}: missing lines in {out:?}"));
                }
            }
            Err(e) => failures.push(format!("iter {i}: {}", e.message)),
        }
    }
    assert!(
        failures.is_empty(),
        "close() racing a wait: park lost the wakeup (pre-fix this loop failed 43-56/200 across four \
         runs with `deadlock: every task in this parallel: block is blocked...` on a 12-core box); \
         {} failures, first: {}",
        failures.len(),
        failures.first().map_or("", |s| s.as_str())
    );
}

/// D4b: a `LocalQ` pops `runnext` first (locality), then the ring in FIFO order, then `None`.
#[test]
fn localq_runnext_then_ring_order() {
    let mut q = LocalQ::new();
    q.ring.push_back(mk_fiber(1));
    q.ring.push_back(mk_fiber(2));
    q.runnext = Some(mk_fiber(0));
    assert_eq!(q.pop().unwrap().task_index, 0, "runnext runs first");
    assert_eq!(q.pop().unwrap().task_index, 1, "then ring FIFO");
    assert_eq!(q.pop().unwrap().task_index, 2);
    assert!(q.pop().is_none());
}

/// D4b: `take_runnable(wid)` drains the worker's own `locals[wid]` BEFORE the shared global queue.
/// (In D4b nothing populates a local at runtime; this drives it directly to pin the search order
/// the D4c requeue/steal paths depend on.)
#[test]
fn take_runnable_prefers_local_over_global() {
    let sched = mk_sched(2);
    sched.seed(vec![mk_fiber(1)]); // task 1 → global, runnable == 1
    sched.lock_local(0).ring.push_back(mk_fiber(0)); // task 0 → worker 0's local
    sched.runnable.fetch_add(1, Ordering::Relaxed); // keep the counter consistent (==2)
    assert_eq!(
        take_run(&sched).task_index,
        0,
        "own local drained before the global queue"
    );
    assert_eq!(take_run(&sched).task_index, 1, "then the global queue");
    assert_eq!(sched.runnable.load(Ordering::Relaxed), 0);
    assert_eq!(sched.lock().running, 2);
}

/// A trivial blocking-shaped native for the offload tests: double the first int arg. Off-heap-safe
/// (reads only a primitive arg, returns a primitive), so it can run on an [`OffloadHost`].
fn double_native(
    h: &mut dyn crate::native::Host,
) -> Result<crate::native::NativeRet, crate::native::HostError> {
    Ok(crate::native::NativeRet::Int(h.arg_int(0)? * 2))
}

/// A native that panics — stands in for a misclassified blocking fn that hits an `OffloadHost`
/// `unreachable!`, or any panic inside an offloaded call.
fn panic_native(
    _h: &mut dyn crate::native::Host,
) -> Result<crate::native::NativeRet, crate::native::HostError> {
    panic!("boom inside offloaded native")
}

/// D5 — a panic inside an offloaded native must NOT lose the fiber. If the pool job lets the panic
/// escape, `complete_offload` never runs: `inflight` stays pinned, the fiber's slot stays empty,
/// and the nursery hangs forever (the deadlock predicate is vetoed by `inflight > 0`). The job
/// must catch the panic, surface it as a fault on the fiber, and always re-enqueue → `inflight`
/// returns to 0 and the resumed fiber faults like an inline native panic.
#[test]
fn offload_native_panic_still_completes_and_faults() {
    let sched = Arc::new(mk_sched(1));
    sched.seed(vec![mk_fiber(0)]);
    let f0 = take_run(&sched); // running == 1
    let req = OffloadReq {
        func: panic_native,
        args: vec![],
        span: Span::RUNTIME,
        timer: None,
    };
    sched.offload(f0, req);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sched.inflight.load(Ordering::Relaxed) != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "panic in offloaded native lost the fiber (inflight pinned → hang)"
        );
        std::thread::yield_now();
    }
    // The fiber came back runnable carrying a fault to raise on resume.
    let f0 = match sched.take_runnable(0, 1, 0) {
        Take::Run(f) => f,
        Take::Stop => panic!("fiber not requeued after panicking offload"),
    };
    assert!(
        matches!(f0.resume_native, Some(Err(_))),
        "panic surfaced as a fault on the fiber"
    );
}

/// D5: an in-flight blocking offload must SUPPRESS the deadlock fire. The predicate is
/// `running==0 && runnable==0 && parked_n>0 && done<total` — but a fiber off in the blocking pool
/// (counted by `inflight`) is neither running, runnable, nor parked, and *will* come back runnable,
/// so `inflight>0` must veto the deadlock declaration (else a program that parks everyone while one
/// fiber blocks in `read_file` would falsely fault `deadlock`).
#[test]
fn deadlock_predicate_suppressed_by_inflight_offload() {
    let sched = mk_sched(2);
    let c = sched.lock();
    // running==0, runnable==0, one parked, none done: a real deadlock with no in-flight work.
    let mut c = c;
    c.parked_n = 1;
    assert!(
        sched.is_deadlocked(&c),
        "all-parked + nothing in flight = deadlock"
    );
    sched.inflight.fetch_add(1, Ordering::Relaxed);
    assert!(
        !sched.is_deadlocked(&c),
        "an in-flight blocking offload vetoes the deadlock fire"
    );
}

/// D5 owe #3 Path C (#1) — the deadlock false-positive fix. A demoted (blocked-in-callback) fiber
/// polls its OWN channel queue; a value a sibling already queued there is invisible to the
/// counter-only predicate (a `send` `push_back`s + notifies the channel condvar — it does NOT bump
/// `runnable`). Registering the demoted channel lets `is_deadlocked` peek it: a non-empty queue
/// means that fiber WILL pop + make progress (possibly waking a parked sibling), so an apparent
/// all-blocked quiesce is NOT a deadlock — don't fault an innocent parked sibling.
#[test]
fn deadlock_predicate_vetoed_by_queued_value_on_demoted_channel() {
    let sched = mk_sched(2);
    let core = empty_core();
    let ptr = core_key(&core);
    let mut c = sched.lock();
    // The #1 race: one demoted fiber (blocked_native) + one parked sibling, nothing running /
    // runnable / inflight — the counter-only predicate fires (the false positive).
    c.parked_n = 1;
    sched.blocked_native.fetch_add(1, Ordering::Relaxed);
    c.register_demoted(ptr, &core);
    // A sibling already queued a value on the demoted fiber's channel: it will pop + progress.
    core.q.lock().unwrap().push(
        crate::vm::core::wire_summary(&WireValue::Int(7)),
        WireValue::Int(7),
    );
    assert!(
        !sched.is_deadlocked(&c),
        "a queued value on a demoted channel must veto the deadlock fire (#1 false-positive)"
    );
    // Drain it: now the demoted fiber truly has nothing queued → a real all-blocked deadlock.
    core.q.lock().unwrap().pop();
    assert!(
        sched.is_deadlocked(&c),
        "an empty demoted channel with all fibers blocked IS a genuine deadlock"
    );
    // Un-register restores the pre-demote predicate (no stale registry entry vetoing forever).
    c.unregister_demoted(ptr);
    assert!(
        sched.is_deadlocked(&c),
        "after un-register the predicate is unchanged (still all-blocked)"
    );
}

/// D5 owe #3 Path C (#1) — the registry is REFCOUNTED so 2+ fibers demoted on the SAME channel each
/// register/unregister independently (one `unregister` must not drop the channel while a second
/// demoted fiber still waits on it). Drives refcount 0→1→2→1→0 and asserts the veto survives the
/// single `unregister` (the entry is still present at refcount 1) and only the empty/fully-removed
/// state declares deadlock. Catches a refcount-direction regression (remove-at-1 / wrong increment)
/// that the single-fiber test cannot — exactly the "stale entry permanently vetoes a real deadlock"
/// vs "premature removal re-opens the false-positive" failure modes.
#[test]
fn demoted_channel_registry_is_refcounted_for_two_fibers_on_one_channel() {
    let sched = mk_sched(3);
    let core = empty_core();
    let ptr = core_key(&core);
    let mut c = sched.lock();
    // Two fibers demoted on the SAME channel + one parked sibling; nothing else running.
    c.parked_n = 1;
    sched.blocked_native.fetch_add(2, Ordering::Relaxed);
    c.register_demoted(ptr, &core);
    c.register_demoted(ptr, &core); // refcount now 2
    // A value queued on the shared channel → at least one demoted fiber pops + progresses.
    core.q.lock().unwrap().push(
        crate::vm::core::wire_summary(&WireValue::Int(7)),
        WireValue::Int(7),
    );
    assert!(
        !sched.is_deadlocked(&c),
        "queued value on the shared demoted channel vetoes deadlock"
    );
    // One fiber pops + un-registers (refcount 2→1); the OTHER is still demoted on this channel, so
    // the entry must remain. Queue now empty → but the entry's presence alone does NOT veto; the
    // peek is queue-driven, so an empty registered channel is a genuine all-blocked deadlock.
    core.q.lock().unwrap().pop();
    c.unregister_demoted(ptr); // refcount 2→1, entry retained
    assert!(
        sched.is_deadlocked(&c),
        "refcount 1 + empty queue = genuine deadlock (the surviving demoted fiber has nothing)"
    );
    // A fresh value for the surviving fiber re-vetoes via the retained entry (proves it wasn't
    // dropped at the first unregister).
    core.q.lock().unwrap().push(
        crate::vm::core::wire_summary(&WireValue::Int(9)),
        WireValue::Int(9),
    );
    assert!(
        !sched.is_deadlocked(&c),
        "the retained refcount-1 entry still peeks the queue (entry not dropped at refcount 1)"
    );
    core.q.lock().unwrap().pop();
    c.unregister_demoted(ptr); // refcount 1→0, entry removed
    assert!(
        c.demoted_chans.is_empty(),
        "the entry is removed only at refcount 0"
    );
    assert!(
        sched.is_deadlocked(&c),
        "all demoted fibers gone, still all-blocked = deadlock"
    );
}

/// D6: `poll_park_offload` hands a fiber whose socket op `WouldBlock`ed to the netpoller —
/// running→inflight — so a socket-parked fiber is accounted as in-flight (it WILL be woken by the
/// OS) and vetoes a false deadlock, exactly like a blocking-pool offload. Uses a real loopback fd
/// (never written) so the fiber genuinely stays parked; `deregister` cleans up (delete-before-drop).
#[test]
fn poll_park_offload_moves_running_to_inflight() {
    use std::os::fd::AsRawFd;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let _client = std::net::TcpStream::connect(addr).unwrap();
    let (server, _) = listener.accept().unwrap();
    server.set_nonblocking(true).unwrap();
    let key = usize::MAX - 10;

    let sched = Arc::new(mk_sched(2));
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched); // running == 1, runnable == 1
    assert_eq!(sched.lock().running, 1);

    sched.poll_park_offload(
        f0,
        PollPark {
            key,
            fd: server.as_raw_fd(),
            interest: poller::Interest::Read,
            in_flight: core::new_in_flight(),
            deadline: None,
        },
    );
    assert_eq!(
        sched.lock().running,
        0,
        "poll-park freed the worker (running decremented)"
    );
    assert_eq!(
        sched.inflight.load(Ordering::Relaxed),
        1,
        "running → inflight on poll-park"
    );

    // The in-flight socket op vetoes a deadlock even with the sibling still queued drained off.
    let mut c = sched.lock();
    c.parked_n = 1;
    assert!(
        !sched.is_deadlocked(&c),
        "a socket op in flight on the poller vetoes a false deadlock"
    );
    drop(c);

    // Clean up: deregister disarms the fd + re-injects (inflight→runnable), before `server` drops.
    assert!(
        poller::deregister(key),
        "the parked socket op was registered"
    );
    assert_eq!(
        sched.inflight.load(Ordering::Relaxed),
        0,
        "deregister re-injected the fiber"
    );
}

/// D5: `offload` hands a fiber to the blocking pool (running→inflight); when the pool finishes the
/// native it `complete_offload`s the fiber back onto the run queue (inflight→runnable) with the
/// raw [`NativeRet`] stashed for the worker to lower + push on resume.
#[test]
fn offload_runs_native_and_requeues_fiber_with_result() {
    let sched = Arc::new(mk_sched(2));
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched); // running==1, runnable==1
    assert_eq!(sched.lock().running, 1);

    let req = OffloadReq {
        func: double_native,
        args: vec![crate::native::NativeArg::Int(21)],
        span: Span::RUNTIME,
        timer: None,
    };
    sched.offload(f0, req);

    // The job runs asynchronously on the blocking pool; wait (bounded) for it to complete.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sched.inflight.load(Ordering::Relaxed) != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "offloaded native never completed"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        sched.lock().running,
        0,
        "offload freed the worker (running decremented)"
    );
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "f1 still queued + f0 requeued on completion"
    );

    // The requeued fiber carries the lowered-pending native result (Int(21)*2 == Int(42)).
    let mut found = None;
    while let Take::Run(f) = sched.take_runnable(0, 1, 0) {
        if f.task_index == 0 {
            found = Some(f);
            break;
        }
    }
    let f0 = found.expect("offloaded fiber requeued");
    assert_eq!(
        f0.resume_native,
        Some(Ok(crate::native::NativeRet::Int(42)))
    );
}

/// D5 owe #2: a `timer_ms` offload parks the fiber on the *timer* thread (not the blocking pool):
/// running→inflight at submit (so it vetoes the deadlock predicate while sleeping), then the timer
/// fires at the deadline and `complete_offload`s the fiber back (inflight→runnable) carrying
/// `Ok(Nil)` — the native is never run on this path (a sleep computes nothing). Guards the timer
/// branch + that the sleeping fiber can't fault a false deadlock.
#[test]
fn timer_offload_parks_then_requeues_fiber_with_nil() {
    let sched = Arc::new(mk_sched(2));
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched); // running == 1, runnable == 1
    assert_eq!(sched.lock().running, 1);

    // `func`/`args` are intentionally ignored on the timer path (the fiber resumes with `Nil`);
    // `double_native` is just a stand-in to satisfy the struct.
    let req = OffloadReq {
        func: double_native,
        args: vec![],
        span: Span::RUNTIME,
        timer: Some(crate::vm::TimerSleep {
            deadline: std::time::Instant::now() + std::time::Duration::from_millis(40),
            cancel: vec![],
            run_deadline: None,
            timeout_ms: 0,
        }),
    };
    sched.offload(f0, req);

    // While the timer holds it the fiber is `inflight` — neither running, runnable, nor parked —
    // and must veto a deadlock fire (it WILL come back).
    assert_eq!(
        sched.inflight.load(Ordering::Relaxed),
        1,
        "timer offload moved the fiber to inflight"
    );
    {
        let c = sched.lock();
        assert!(
            !sched.is_deadlocked(&c),
            "a timer-parked (inflight) fiber must not fault a false deadlock"
        );
    }

    // The timer fires at the deadline and requeues the fiber.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while sched.inflight.load(Ordering::Relaxed) != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "timer never fired the parked fiber back (lost wakeup?)"
        );
        std::thread::yield_now();
    }
    assert_eq!(sched.lock().running, 0, "timer offload freed the worker");
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        2,
        "f1 still queued + f0 requeued by the timer"
    );

    let mut found = None;
    while let Take::Run(f) = sched.take_runnable(0, 1, 0) {
        if f.task_index == 0 {
            found = Some(f);
            break;
        }
    }
    let f0 = found.expect("timer-parked fiber requeued");
    assert_eq!(
        f0.resume_native,
        Some(Ok(crate::native::NativeRet::Nil)),
        "sleep resumes with Nil, native not run"
    );
}

/// D4c: `try_steal` grabs ceil-half of the first non-empty victim's ring (from the back), leaving
/// the rest, and is net-zero on `runnable` (the fibers stay runnable, just change owner).
#[test]
fn schedule_steals_half_from_victim() {
    let sched = mk_sched(4);
    {
        let mut vq = sched.lock_local(1);
        for i in 0..4 {
            vq.ring.push_back(mk_fiber(i));
        }
    }
    sched.runnable.fetch_add(4, Ordering::Relaxed); // keep the counter consistent with the queues
    let stolen = sched.try_steal(0); // worker 0 steals from worker 1
    assert_eq!(stolen.len(), 2, "ceil(4/2) stolen");
    assert_eq!(
        sched.lock_local(1).ring.len(),
        2,
        "half left with the victim"
    );
    assert_eq!(
        sched.runnable.load(Ordering::Relaxed),
        4,
        "stealing is net-zero on runnable"
    );
}

/// D4d: on a `GLOBAL_CHECK_INTERVAL`th schedule a worker pulls from the global queue before its
/// own local (anti-starvation); on any other tick it drains its own local first.
#[test]
fn schedule_pulls_global_every_61st_tick() {
    // tick=61 (a multiple) → the global fiber wins over the local one.
    let periodic = mk_sched(4);
    periodic.lock_local(0).ring.push_back(mk_fiber(0)); // local
    periodic.seed(vec![mk_fiber(1)]); // global (bumps runnable)
    periodic.runnable.fetch_add(1, Ordering::Relaxed); // for the local fiber
    let got = match periodic.take_runnable(0, GLOBAL_CHECK_INTERVAL, 0) {
        Take::Run(f) => f.task_index,
        Take::Stop => panic!("expected a runnable fiber"),
    };
    assert_eq!(got, 1, "periodic tick drains the global queue first");

    // tick=1 (not a multiple) → the local fiber wins (normal order).
    let normal = mk_sched(4);
    normal.lock_local(0).ring.push_back(mk_fiber(0));
    normal.seed(vec![mk_fiber(1)]);
    normal.runnable.fetch_add(1, Ordering::Relaxed);
    let got = match normal.take_runnable(0, 1, 0) {
        Take::Run(f) => f.task_index,
        Take::Stop => panic!("expected a runnable fiber"),
    };
    assert_eq!(got, 0, "non-periodic tick drains the own local first");
}

/// D4c: a thief never steals from itself and skips empty victims (returns nothing when only its
/// own local has work).
#[test]
fn steal_skips_self_and_empty_victims() {
    let sched = mk_sched(4);
    {
        let mut own = sched.lock_local(0);
        own.ring.push_back(mk_fiber(0));
        own.ring.push_back(mk_fiber(1));
    }
    assert!(
        sched.try_steal(0).is_empty(),
        "no sibling has work; must not steal from self"
    );
}

/// D2b/U2: parking the running fiber on an EMPTY channel frees the worker (`running--`,
/// `parked++`); a `send_wake` on that channel enqueues the message and moves the fiber back onto
/// the run queue as `Ready`.
#[test]
fn mnsched_park_then_wake_requeues_fiber() {
    let sched = mk_sched(1);
    let core = empty_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    sched.park(key, &core, f);
    {
        let c = sched.lock();
        assert_eq!(c.running, 0);
        assert_eq!(c.parked_n, 1);
        assert!(c.global.is_empty());
    }
    sched.send_wake(key, &core, WireValue::Int(7));
    {
        let c = sched.lock();
        assert_eq!(c.parked_n, 0);
        assert_eq!(c.global.len(), 1);
    }
    let g = take_run(&sched);
    assert_eq!(g.task_index, 0);
    assert!(matches!(g.state, FiberState::Ready));
}

/// D3/U: a fiber that exhausts its reduction budget `yield_fiber`s — the scheduler frees the
/// worker (`running--`) and requeues it at the **tail** of `runq` (round-robin), still `Ready`.
/// No park bucket is touched (a yield carries no channel handle). Mirrors the park/wake test.
#[test]
fn mnsched_yield_fiber_requeues_at_tail() {
    let sched = mk_sched(2);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let f0 = take_run(&sched); // pops task 0, running == 1
    assert_eq!(f0.task_index, 0);
    sched.yield_fiber(f0); // requeue task 0 behind task 1
    {
        let c = sched.lock();
        assert_eq!(c.running, 0);
        assert_eq!(c.parked_n, 0); // a yield never parks
        assert_eq!(c.global.len(), 2);
    }
    // Round-robin: task 1 (which was behind task 0) now runs before the requeued task 0.
    assert_eq!(take_run(&sched).task_index, 1);
    let back = take_run(&sched);
    assert_eq!(back.task_index, 0);
    assert!(matches!(back.state, FiberState::Ready));
}

/// D4/stress: a combined-churn workload that exercises EVERY new D4 path together under a
/// watchdog — 500 consumers that block on `recv` (park + `send_wake`), 500 producers that do CPU
/// work (reduction `yield` → global, batch-grab, work-stealing between idle workers) then `send`
/// (waking a parked consumer), all `#fibers ≫ #workers`. The consumers accumulate into one
/// `Shared`; the join must complete with the exact arithmetic sum, with no lost/duplicated fiber,
/// no false deadlock, and no hang (the watchdog turns a regression — a lost wakeup, a steal/grab
/// accounting bug, a deadlock-predicate false positive — into a loud failure rather than a wedge).
#[test]
fn d4_worksteal_cpu_and_channel_stress() {
    let src = "\
fn producer(ch: Channel[int], lo: int, hi: int):
    acc := 0
    i := lo
    while i < hi:
        acc += i
        i += 1
    ch.send(acc)

fn consumer(ch: Channel[int], sink: Shared[int]):
    v := ch.recv()
    sink.update(fn(x): x + v)

fn main():
    ch := Channel[int]()
    sink := Shared(0)
    parallel:
        for _ in 0..500:
            spawn consumer(ch, sink)
        for k in 0..500:
            spawn producer(ch, k * 10, k * 10 + 10)
    print(sink.get())

main()
";
    // sum_{k=0}^{499} sum_{i=10k}^{10k+9} i = sum_{k=0}^{499} (100k + 45) = 12_475_000 + 22_500.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("mixed work-steal nursery completed"), "12497500\n"),
        Err(_) => panic!(
            "hung — D4 work-stealing/grab/wait_timeout regressed (lost wakeup or accounting bug)"
        ),
    }
}

/// D4e/stress: the lost-wakeup regression guard for the runnable-gated park. D4e removed the 2 ms
/// `wait_timeout` backstop — an idle worker now sleeps on `cv` INDEFINITELY when `runnable == 0`,
/// woken ONLY by a real `notify` from a sibling's `send`/`yield`/`finish`/offload-complete. A
/// single missed wakeup is no longer a 2 ms stall but a PERMANENT hang. The race is probabilistic
/// (the batch-grab in-hand window, the park-vs-send gap), so we REPEAT a park-heavy
/// consumer-first workload many rounds, each under a watchdog: any lost wakeup in any round =>
/// the round never completes => `recv_timeout` fires => loud failure. 300 consumers are spawned
/// FIRST (they all `recv`-park, driving every worker to a true `cv.wait` sleep with `runnable`
/// near zero), then 300 producers wake them — the exact sleep→`send_wake`→wake path D4e changed.
#[test]
fn d4e_pingpong_no_lost_wakeup_stress() {
    let src = "\
fn producer(ch: Channel[int], lo: int, hi: int):
    acc := 0
    i := lo
    while i < hi:
        acc += i
        i += 1
    ch.send(acc)

fn consumer(ch: Channel[int], sink: Shared[int]):
    v := ch.recv()
    sink.update(fn(x): x + v)

fn main():
    ch := Channel[int]()
    sink := Shared(0)
    parallel:
        for _ in 0..300:
            spawn consumer(ch, sink)
        for k in 0..300:
            spawn producer(ch, k * 10, k * 10 + 10)
    print(sink.get())

main()
";
    // sum_{k=0}^{299} sum_{i=10k}^{10k+9} i = sum_{k=0}^{299} (100k + 45) = 100*44850 + 300*45.
    let expected = format!("{}\n", 100 * (299 * 300 / 2) + 300 * 45);
    for round in 0..25 {
        let want = expected.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_capture_parallel(src));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(r) => assert_eq!(
                r.expect("park-heavy nursery completed"),
                want,
                "round {round}"
            ),
            Err(_) => panic!(
                "hung on round {round} — D4e runnable-gated park lost a wakeup \
                     (an idle worker slept on cv with a runnable fiber pending)"
            ),
        }
    }
}

/// D4e/stress: wake-from-TRUE-sleep. Distinct from the churn test above, this isolates the exact
/// state D4e introduced — workers asleep on `cv` with `runnable == 0` — and proves a later `send`
/// wakes them with the poll gone. One `slow_producer` burns CPU on a single worker while `N`
/// consumers `recv`-park; with nothing queued (`runnable == 0`) every OTHER worker reaches the
/// runnable-gated branch and does a real `cv.wait` (no 2 ms timeout to fall back on). Only when
/// the producer finishes its spin and fires its burst of `send`s are the sleepers woken. The join
/// completing with `sink == N` proves no sleeper was stranded. Watchdog 30 s.
#[test]
fn d4e_wake_parked_workers_from_true_sleep() {
    let n = 200usize;
    let src = format!(
        "\
fn slow_producer(ch: Channel[int], n: int):
    acc := 0
    i := 0
    while i < 8000000:
        acc += i
        i += 1
    j := 0
    while j < n:
        ch.send(acc + j)
        j += 1

fn consumer(ch: Channel[int], sink: Shared[int]):
    ch.recv()
    sink.update(fn(x): x + 1)

fn main():
    ch := Channel[int]()
    sink := Shared(0)
    parallel:
        for _ in 0..{n}:
            spawn consumer(ch, sink)
        spawn slow_producer(ch, {n})
    print(sink.get())

main()
"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(&src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(
            r.expect("wake-from-sleep nursery completed"),
            format!("{n}\n")
        ),
        Err(_) => panic!(
            "hung — D4e: a `send` failed to wake workers parked in the runnable-gated `cv.wait` \
                 (lost wakeup from true sleep)"
        ),
    }
}

/// D5 — the discriminating offload proof. `N = workers * 4` fibers each `sleep_ms(150)`. Run
/// INLINE on the core pool, each of the `workers` threads must run 4 sleeps back-to-back → wall
/// clock ≥ `4 * 150 = 600 ms` regardless of core count. OFFLOADED to the dirty pool, all `N`
/// sleeps run concurrently → wall clock ≈ `150 ms`. Asserting `< 450 ms` (`3 * sleep`) fails on
/// the inline path and passes once offload is wired — and the `N ∝ workers` construction keeps
/// that gap on any machine (the inline path is always 4 batches). Watchdog 30 s.
#[test]
fn d5_blocking_sleeps_run_concurrently_not_serialized() {
    let workers = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(1)
        .max(1);
    let n = workers * 4;
    let src = format!(
        "\
import std.time

fn sleeper():
    time.sleep_ms(150)

fn main():
    parallel:
        for _ in 0..{n}:
            spawn sleeper()
    print(\"done\")

main()
"
    );
    let entry = write_temp_chz("d5_sleeps", &src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let out = run_file_parallel(&run_entry, crate::native::HostConfig::default());
        let _ = tx.send((out, start.elapsed()));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok(((out, _err, res, _code), elapsed)) => {
            assert!(res.is_ok(), "sleeper nursery faulted: {res:?}");
            assert_eq!(out, "done\n");
            assert!(
                elapsed < std::time::Duration::from_millis(450),
                "{n} sleep_ms(150) fibers took {elapsed:?} — blocking calls serialized on the core pool \
                     instead of offloading to the dirty pool (G3 starvation)"
            );
        }
        Err(_) => {
            panic!("hung — D5 offload/complete regressed (lost wakeup or inflight accounting bug)")
        }
    }
}

/// D5 owe #3 Path C (#3 sleep-in-callback demote) — the discriminating proof, mirroring
/// `d5_blocking_sleeps_run_concurrently_not_serialized` but with the `sleep_ms` reached INSIDE a
/// native callback (`[1].map(nap)`, `native_reentry > 0`). The offload gate requires
/// `native_reentry == 0`, so without the demote this sleep runs INLINE and pins its worker:
/// `N = workers * 4` such tasks ⇒ 4 back-to-back batches ⇒ ≥ `4 * 150 = 600 ms` on any core count.
/// WITH the demote each sleeping callback frees its worker (spawns a replacement) so all `N` run
/// concurrently ⇒ ≈ `150 ms`. Asserting `< 450 ms` fails on the inline path and passes once the
/// in-callback demote is wired. Watchdog 30 s.
#[test]
fn d5_owe3_path_c_sleep_in_callback_demotes_frees_worker() {
    let workers = std::thread::available_parallelism()
        .map(|x| x.get())
        .unwrap_or(1)
        .max(1);
    let n = workers * 4;
    let src = format!(
        "\
import std.time

fn nap(x: int) -> int:
    time.sleep_ms(150)
    return x

fn sleeper():
    [1].map(nap)

fn main():
    parallel:
        for _ in 0..{n}:
            spawn sleeper()
    print(\"done\")

main()
"
    );
    let entry = write_temp_chz("d5_owe3_sleep_cb", &src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let out = run_file_parallel(&run_entry, crate::native::HostConfig::default());
        let _ = tx.send((out, start.elapsed()));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok(((out, _err, res, _code), elapsed)) => {
            assert!(res.is_ok(), "in-callback sleeper nursery faulted: {res:?}");
            assert_eq!(out, "done\n");
            assert!(
                elapsed < std::time::Duration::from_millis(450),
                "{n} sleep_ms(150)-in-callback fibers took {elapsed:?} — the in-callback sleep pinned \
                     its worker (ran inline) instead of demoting (#3)"
            );
        }
        Err(_) => panic!("hung — D5 owe #3 Path C sleep-in-callback demote regressed"),
    }
}

/// D5 owe #3 Path C (#3) — correctness of the in-callback sleep demote: a `sleep_ms` inside a
/// native `xs.map` still produces the right result after demoting (the worker is freed + resumed in
/// place; output unchanged). The sum proves all three callbacks ran past their sleep. Watchdog 30 s.
#[test]
fn d5_owe3_path_c_sleep_in_callback_correct() {
    let src = "\
import std.time

fn nap(x: int) -> int:
    time.sleep_ms(20)
    return x * 2

fn work(sink: Shared[int]):
    ys := [1, 2, 3].map(nap)
    sink.update(fn(x): x + ys[0] + ys[1] + ys[2])

fn main():
    sink := Shared(0)
    parallel:
        spawn work(sink)
        spawn work(sink)
    print(sink.get())

main()
";
    let entry = write_temp_chz("d5_owe3_sleep_correct", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "in-callback sleep demote faulted: {res:?}");
            // each work(): 2*(1+2+3) = 12; two tasks update the same sink → 24.
            assert_eq!(out, "24\n");
        }
        Err(_) => {
            panic!("hung — D5 owe #3 Path C sleep-in-callback demote regressed (correctness)")
        }
    }
}

/// D5 owe #3 Path C (#3 socket half) — correctness of the in-callback socket DEMOTE: a `Socket::read`
/// reached INSIDE a native `xs.map` callback (`native_reentry > 0`) that `WouldBlock`s must demote
/// the worker (spin a replacement, backoff-poll the non-blocking read in place) and resume with the
/// real bytes — NOT surface the `--parallel`-engine error. `park_on_fd` only parks on the netpoller
/// when `native_reentry == 0`; inside a callback the Rust-stack `map` loop can't snapshot-park, so
/// without the demote the read returns `Result::Err("read would block: ... require the --parallel
/// engine")`, which `?` propagates → the client prints `ERR:…` instead of the echoed line. The
/// server `sleep_ms(50)`s after `accept` before writing, so the client's in-callback read is
/// *guaranteed* empty (forces the demote path deterministically). Parallel-only, 30 s watchdog.
#[test]
fn d5_owe3_path_c_socket_read_in_callback_demotes() {
    let src = "\
import std.net
import std.time

fn read_reply(s: Socket) -> str!:
    line := s.read(64)?
    return Ok(line)

fn do_client(addr: str) -> str!:
    sock := net.connect(addr)?
    socks := [sock]
    replies := socks.map(read_reply)
    line := replies[0]?
    sock.close()
    return Ok(line)

fn client(addr: str):
    match do_client(addr):
        Ok(line): print(line)
        Err(e): print(\"ERR:\" + e.message())

fn server(listener: Listener) -> int!:
    conn := listener.accept()?
    time.sleep_ms(50)
    conn.write(\"hello\")?
    conn.close()
    listener.close()
    return Ok(0)

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
    let entry = write_temp_chz("d5_owe3_sock_read_cb", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(
                res.is_ok(),
                "in-callback socket read demote faulted: {res:?}"
            );
            // client prints the echoed line; main prints a trailing blank line.
            assert_eq!(
                out, "hello\n\n",
                "in-callback read did not demote (got: {out:?})"
            );
        }
        Err(_) => panic!("hung — D5 owe #3 Path C socket-read-in-callback demote regressed"),
    }
}

/// D5 owe #3 Path C (#3 socket half) — the listener path: an `accept` reached INSIDE a native `map`
/// callback that `WouldBlock`s (no client yet) demotes + resumes once a sibling client connects,
/// instead of erroring. Proves the `Listener::accept` gate, not just `Socket::read`. The client
/// `sleep_ms(50)`s before connecting so the in-callback `accept` is guaranteed to block first.
/// Parallel-only, 30 s watchdog.
#[test]
fn d5_owe3_path_c_accept_in_callback_demotes() {
    let src = "\
import std.net
import std.time

fn accept_one(l: Listener) -> int!:
    conn := l.accept()?
    conn.read(64)?
    conn.close()
    return Ok(1)

fn do_server(listener: Listener) -> int!:
    ls := [listener]
    got := ls.map(accept_one)
    n := got[0]?
    listener.close()
    return Ok(n)

fn server(listener: Listener):
    match do_server(listener):
        Ok(n): print(n)
        Err(e): print(\"ERR:\" + e.message())

fn client(addr: str) -> int!:
    time.sleep_ms(50)
    sock := net.connect(addr)?
    sock.write(\"ping\")?
    sock.close()
    return Ok(0)

fn run() -> int!:
    listener := net.listen(\"127.0.0.1:0\")?
    addr := listener.addr()?
    parallel:
        spawn server(listener)
        spawn client(addr)
    return Ok(0)

fn main():
    match run():
        Ok(_): print(\"\")
        Err(e): print(\"RUN-ERR:\" + e.message())

main()
";
    let entry = write_temp_chz("d5_owe3_sock_accept_cb", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "in-callback accept demote faulted: {res:?}");
            assert_eq!(
                out, "1\n\n",
                "in-callback accept did not demote (got: {out:?})"
            );
        }
        Err(_) => panic!("hung — D5 owe #3 Path C accept-in-callback demote regressed"),
    }
}

/// D5 — a blocking *filesystem* native (`fs.exists`, returns a `bool`) is offloaded, runs off the
/// core worker, and its result is lowered + pushed on resume so execution continues correctly
/// past the call. `N` fibers each check a real temp file and bump a `Shared` — the join sum must
/// be exactly `N` (every offloaded call returned `true` and resumed into the `if`). Guards the
/// resume-continues-past-the-call + bool-lowering path. Watchdog 30 s.
#[test]
fn d5_blocking_fs_calls_offload_and_resume_correctly() {
    let path = std::env::temp_dir().join(format!("chezzi_d5_exists_{}.txt", std::process::id()));
    std::fs::write(&path, b"x").expect("write temp file");
    let path_str = path.to_str().expect("utf8 temp path").to_string();
    let n = 64usize;
    let src = format!(
        "\
import std.fs

fn checker(sink: Shared[int], path: str):
    if fs.exists(path):
        sink.update(fn(x): x + 1)

fn main():
    sink := Shared(0)
    parallel:
        for _ in 0..{n}:
            spawn checker(sink, \"{path_str}\")
    print(sink.get())

main()
"
    );
    let entry = write_temp_chz("d5_fs", &src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "fs.exists nursery faulted: {res:?}");
            assert_eq!(out, format!("{n}\n"));
        }
        Err(_) => panic!("hung — D5 fs offload/resume regressed"),
    }
}

/// D5 — a blocking native reached *inside a native callback* (here `fs.exists` inside the
/// per-element fn of `list.map`, which runs under the `native_reentry` guard) must NOT be
/// offloaded to the dirty pool: the callback's loop state lives on the Rust host stack and cannot be
/// parked into a fiber. The offload is gated on `native_reentry == 0`, so it falls back to inline
/// execution and the map completes correctly (no fault, no corruption). Guards the gate for a
/// NON-sleep blocking native (`sleep_ms` specifically now DEMOTES the worker inside a callback —
/// see `d5_owe3_path_c_sleep_in_callback_*`). Watchdog 30 s.
#[test]
fn d5_blocking_native_in_callback_runs_inline() {
    let path = std::env::temp_dir().join(format!("chezzi_d5_cb_exists_{}.txt", std::process::id()));
    std::fs::write(&path, b"x").expect("write temp file");
    let path_str = path.to_str().expect("utf8 temp path").to_string();
    let src = format!(
        "\
import std.fs

fn dbl(x: int) -> int:
    if fs.exists(\"{path_str}\"):
        return x * 2
    return 0

fn work(sink: Shared[int]):
    ys := [1, 2, 3].map(dbl)
    sink.update(fn(x): x + ys[0] + ys[1] + ys[2])

fn main():
    sink := Shared(0)
    parallel:
        spawn work(sink)
    print(sink.get())

main()
"
    );
    let entry = write_temp_chz("d5_callback", &src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let _ = std::fs::remove_file(&path);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "in-callback nursery faulted: {res:?}");
            assert_eq!(out, "12\n");
        }
        Err(_) => panic!(
            "hung — D5 native_reentry gate regressed (offloaded an in-callback blocking native)"
        ),
    }
}

/// D5 owe #3 (Path A) — a blocking `recv` reached **through a chezzi-source HOF** (`iter.map`,
/// `std/iter.chz`) parks instead of faulting `deadlock`, unlike the native `.map` (whose Rust loop
/// frame breaks the snapshot chain). Every frame from the fiber's entry to the `recv` is a VM frame
/// (`map`'s `for`-loop + the closure), so the park is sound. The exact A/B of the contrast test
/// `fibers_recv_inside_map_callback_faults` (native `xs.map` → `deadlock`); here `iter.map` succeeds.
///
/// The **cooperative** leg is the deterministic guard: tasks run in spawn order on one thread, so
/// `consume` (spawned first) reaches `recv` on the still-empty channel and **must park** before the
/// `produce` sibling can run — a regressed park/wake faults `deadlock` or hangs, never flake-passes.
/// (Under `--parallel` the producer races the consumer on another thread and may fill the unbounded
/// FIFO before the first `recv`, so that leg can't *force* a park — it's the real-engine + hang
/// guard, run under a 30 s watchdog.) Sum `66` proves all three recvs threaded through the closure.
#[test]
fn d5_owe3_recv_in_iter_map_callback_parks() {
    let src = "\
import std.iter

fn produce(ch: Channel[int]):
    ch.send(10)
    ch.send(20)
    ch.send(30)

fn consume(ch: Channel[int], out: Shared[int]):
    ys := iter.map([1, 2, 3], fn(x: int) -> int: x + ch.recv())
    out.update(fn(a): a + ys[0] + ys[1] + ys[2])

fn main():
    ch := Channel[int]()
    out := Shared(0)
    parallel:
        spawn consume(ch, out)
        spawn produce(ch)
    print(out.get())

main()
";
    let entry = write_temp_chz("d5_owe3_iter_map", src);
    // Cooperative leg — deterministic: `consume` parks on the empty channel before `produce` runs.
    let (co, _ce, cr, _cc) = run_file_with(&entry, crate::native::HostConfig::default());
    assert!(
        cr.is_ok(),
        "cooperative iter.map recv-in-callback faulted (park regressed): {cr:?}"
    );
    assert_eq!(
        co, "66\n",
        "cooperative iter.map recv-in-callback wrong sum"
    );
    // Parallel leg — the real M:N engine, under a watchdog so a park/wake hang fails loud.
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(
                res.is_ok(),
                "parallel iter.map recv-in-callback nursery faulted: {res:?}"
            );
            assert_eq!(out, "66\n");
        }
        Err(_) => {
            panic!("hung — D5 owe #3 Path A regressed (recv inside iter.map did not park)")
        }
    }
}

/// D5 owe #3 (Path A) — the new `std/iter.chz` HOFs (`map`/`filter`/`fold`/`reduce`) are correct
/// and byte-identical across both engines. `map` to a different return type (`int -> str`)
/// exercises generic-return inference (`U` is bound solely from the closure, not from `xs`), the
/// primary risk flagged in the plan — it works without explicit type args. Cooperative (no
/// `--parallel`): this guards the functional surface; the park behaviour is guarded separately by
/// `d5_owe3_recv_in_iter_map_callback_parks`.
#[test]
fn d5_owe3_iter_hofs_correct_on_both_engines() {
    let src = "\
import std.iter

fn main():
    xs := [1, 2, 3, 4, 5]
    print(iter.map(xs, fn(x: int) -> int: x * x))
    print(iter.filter(xs, fn(x: int) -> bool: x % 2 == 0))
    print(iter.fold(xs, 0, fn(a: int, x: int) -> int: a + x))
    print(iter.reduce(xs, fn(a: int, b: int) -> int: a * b))
    print(iter.map([1, 2], fn(x: int) -> str: \"n{x}\"))
    # subtraction is non-commutative — locks the left-to-right fold order (0-1-2-3-4-5 = -15)
    print(iter.fold(xs, 0, fn(a: int, x: int) -> int: a - x))

main()
";
    let entry = write_temp_chz("d5_owe3_hofs", src);
    let cfg = crate::native::HostConfig::default;
    let (vo, _ve, vr, _vc) = run_file_with(&entry, cfg());
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(vr.is_ok(), "vm run faulted: {vr:?}");
    assert!(ir.is_ok(), "interp run faulted: {ir:?}");
    assert_eq!(vo, io, "vm/interp stdout divergence");
    assert_eq!(
        vo,
        "[1, 4, 9, 16, 25]\n[2, 4]\n15\n120\n['n1', 'n2']\n-15\n"
    );
}

/// D5 owe #3 (Path C) — a blocking `recv` reached inside a **native** callback (`xs.map`, whose
/// per-element loop frame lives on the Rust host stack and CANNOT be snapshot-parked) no longer
/// faults `deadlock` under `--parallel`: the worker thread is **demoted** (blocks in place on the
/// channel condvar, a fresh replacement worker covers its `wid`), and **resumes in place** when a
/// sibling `send`s — Go's `handoffp`. The contrast to Path A (`d5_owe3_recv_in_iter_map_callback_parks`,
/// where `iter.map` is chezzi source → pure VM frames → snapshot-parks) and to the cooperative-engine
/// pin (`fibers_recv_inside_map_callback_faults`, which still faults — demotion is M:N-only). The
/// result is written with `Shared.set` (NOT `update`) so the recv site is the `xs.map` callback only,
/// avoiding the `update_lock`-held-while-blocked hazard. Sum `66` = (1+10)+(2+20)+(3+30): all three
/// recvs threaded through the native map callback. Parallel-only, under a 30 s watchdog so a
/// demote/resume hang fails loud instead of hanging the suite. The producer `sleep_ms`s before its
/// first `send` so the consumer's first map-callback `recv` is **guaranteed empty** — forcing the
/// demote path deterministically (without the delay the producer races ahead and pre-fills the FIFO,
/// so the `recv` never blocks and the test would flake-pass even with Path C broken).
#[test]
fn d5_owe3_path_c_recv_in_native_map_callback_demotes() {
    let src = "\
import std.time

fn use_map(ch: Channel[int], out: Shared[int]):
    xs := [1, 2, 3]
    ys := xs.map(fn(x): x + ch.recv())
    out.set(ys[0] + ys[1] + ys[2])

fn fill(ch: Channel[int]):
    time.sleep_ms(50)
    ch.send(10)
    ch.send(20)
    ch.send(30)

fn main():
    ch := Channel[int]()
    out := Shared(0)
    parallel:
        spawn use_map(ch, out)
        spawn fill(ch)
    print(out.get())

main()
";
    let entry = write_temp_chz("d5_owe3_path_c", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(
                res.is_ok(),
                "Path C: recv inside native xs.map faulted under --parallel: {res:?}"
            );
            assert_eq!(out, "66\n");
        }
        Err(_) => panic!(
            "hung — D5 owe #3 Path C regressed (recv inside native xs.map did not demote-and-resume)"
        ),
    }
}

/// D5 owe #3 (Path C) — a `recv` inside a native callback with **no possible sender** must still
/// **fault `deadlock`**, not hang. This is the load-bearing half of the pragmatic deadlock scope:
/// the demoted thread is accounted as `blocked_native` (a 5th fiber state), which feeds
/// [`MnSched::is_deadlocked`] (`parked_n>0 || blocked_native>0`). The demote's `blocked_native++`
/// notifies `cv` so the idle replacement worker re-evaluates the predicate; on fire, `flag_deadlock`
/// sets `terminate`, the demoted thread observes it within `DEMOTE_POLL_BACKOFF` and faults in place,
/// and `wait_for_completion` lets the join reduce the deadlock outcome. Watchdog 30 s: a regressed
/// predicate (or a missing notify) would HANG here instead of faulting.
#[test]
fn d5_owe3_path_c_recv_in_callback_no_sender_still_deadlocks() {
    let src = "\
fn use_map(ch: Channel[int]):
    xs := [1]
    ys := xs.map(fn(x): x + ch.recv())
    print(ys)

fn main():
    ch := Channel[int]()
    parallel:
        spawn use_map(ch)

main()
";
    let entry = write_temp_chz("d5_owe3_path_c_dl", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((_out, _err, res, _code)) => match res {
            Err(e) => {
                let s = format!("{e:?}");
                assert!(
                    s.contains("deadlock"),
                    "Path C: no-sender recv-in-callback should fault deadlock, got: {s}"
                );
            }
            Ok(()) => panic!("Path C: no-sender recv-in-callback unexpectedly succeeded"),
        },
        Err(_) => panic!(
            "hung — D5 owe #3 Path C deadlock detection regressed (blocked_native predicate / notify)"
        ),
    }
}

/// §6d M:N wait-in-callback DEMOTE (TDD step 8) — a blocking `wait` reached inside a native `xs.map`
/// callback (`native_reentry > 0`) cannot snapshot-park (the HOF's loop state is on the Rust host
/// stack), so it DEMOTES: blocks the worker in place, polling all N arm queues on a bounded backoff
/// until a sibling `send` to either arm delivers (mirrors `demote_recv_block`, the documented
/// lower-throughput v1 path). The producer `sleep_ms`s before its `send` so the callback's first
/// `wait` poll is GUARANTEED empty — forcing the demote path deterministically. The send lands on
/// the SECOND arm, so the demote loop's source-order N-arm scan is exercised. Watchdog 30 s so a
/// demote/resume hang fails loud. `66` = 1 + 65 (the second-arm value).
#[test]
fn vm_wait_in_native_callback_demotes_under_parallel() {
    let src = "\
import std.time

fn pick(a: Channel[int], b: Channel[int], x: int) -> int:
    wait:
        v := a.recv(): return x + v
        w := b.recv(): return x + w

fn use_map(a: Channel[int], b: Channel[int], out: Shared[int]):
    xs := [1]
    ys := xs.map(fn(x): pick(a, b, x))
    out.set(ys[0])

fn fill(b: Channel[int]):
    time.sleep_ms(50)
    b.send(65)

fn main():
    a := Channel[int]()
    b := Channel[int]()
    out := Shared(0)
    parallel:
        spawn use_map(a, b, out)
        spawn fill(b)
    print(out.get())

main()
";
    let entry = write_temp_chz("vm_wait_callback_demote", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(
                res.is_ok(),
                "wait inside native xs.map faulted under --parallel: {res:?}"
            );
            assert_eq!(out, "66\n");
        }
        Err(_) => panic!("hung — M:N wait-in-callback demote-and-resume regressed"),
    }
}

/// WAIT-1 (HIGH) on the DEMOTE path — a `wait` over a long timer + data channel reached INSIDE a
/// native `xs.map` callback (`native_reentry > 0`, cannot snapshot-park) must let a mid-window
/// sibling `send` win the data arm, NOT pin the worker on the 2000ms timer. The demote poll loop's
/// source-order channel scan must beat the timer deadline. Pre-fix the demote loop had no deadline
/// handling and behaved inconsistently / could mishandle the timer arm. 30s watchdog.
#[test]
fn vm_wait_timer_loses_to_send_in_native_callback_parallel() {
    let src = "\
import std.time

fn pick(a: Channel[int], t: Channel[bool], x: int) -> int:
    wait:
        v := a.recv(): return x + v
        _ := t.recv(): return -1

fn use_map(a: Channel[int], t: Channel[bool], out: Shared[int]):
    xs := [0]
    ys := xs.map(fn(x): pick(a, t, x))
    out.set(ys[0])

fn fill(a: Channel[int]):
    time.sleep_ms(5)
    a.send(7)

fn main():
    a := Channel[int]()
    t := timer(2000)
    out := Shared(0)
    parallel:
        spawn use_map(a, t, out)
        spawn fill(a)
    print(out.get())

main()
";
    let entry = write_temp_chz("vm_wait_callback_timer", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(
                res.is_ok(),
                "wait+timer inside native xs.map faulted under --parallel: {res:?}"
            );
            assert_eq!(
                out, "7\n",
                "the timer arm took the wait instead of the mid-window send (WAIT-1, demote path)"
            );
        }
        Err(_) => panic!("hung — M:N wait-in-callback timer demote regressed"),
    }
}

/// §6d M:N wait-in-callback DEMOTE with no possible sender must still fault `deadlock`, not hang —
/// the demote loop's self-detected-deadlock path (`is_deadlocked` over the registered arm channels).
#[test]
fn vm_wait_in_native_callback_no_sender_deadlocks() {
    let src = "\
fn pick(a: Channel[int], b: Channel[int], x: int) -> int:
    wait:
        v := a.recv(): return x + v
        w := b.recv(): return x + w

fn use_map(a: Channel[int], b: Channel[int]):
    xs := [1]
    ys := xs.map(fn(x): pick(a, b, x))
    print(ys)

fn main():
    a := Channel[int]()
    b := Channel[int]()
    parallel:
        spawn use_map(a, b)

main()
";
    let entry = write_temp_chz("vm_wait_callback_dl", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((_out, _err, res, _code)) => match res {
            Err(e) => assert!(
                format!("{e:?}").contains("deadlock"),
                "no-sender wait-in-callback should deadlock: {e:?}"
            ),
            Ok(()) => panic!("no-sender wait-in-callback unexpectedly succeeded"),
        },
        Err(_) => panic!("hung — M:N wait-in-callback deadlock detection regressed"),
    }
}

/// D5 owe #3 Path C (#1 false-positive) — the deadlock checker must NOT fault an innocent parked
/// sibling when a demoted fiber has a value racing into its queue. F1 demotes on `a` inside a native
/// `xs.map`, then wakes F2 via `c.send(7)`; F2 snapshot-parks on `c`; F3 feeds `a` then finishes.
/// The bad interleaving (microseconds): F3's `running→0` quiesce can fire the predicate before F1
/// pops its queued `10`, wrongly killing the parked F2. With the #1 fix (`is_deadlocked` peeks the
/// demoted channel `a`, which holds `10`) the fire is vetoed. Run many times to expose the race; the
/// output must ALWAYS be `7` and NEVER a spurious `deadlock`. Watchdog per iteration.
#[test]
fn d5_owe3_path_c_no_false_deadlock_when_demoted_fiber_has_queued_value() {
    let src = "\
fn main():
    a := Channel[int]()
    c := Channel[int]()
    xs := [1]
    parallel:
        spawn:
            xs.map(fn(x): x + a.recv())
            c.send(7)
        spawn: print(c.recv())
        spawn: a.send(10)

main()
";
    let entry = write_temp_chz("d5_owe3_path_c_fp", src);
    for i in 0..200 {
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(
                &run_entry,
                crate::native::HostConfig::default(),
            ));
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(30));
        // Remove the temp file BEFORE asserting so an assert panic doesn't leak it (the other paths
        // out of this test all unwind, and the post-loop cleanup never runs on panic).
        if i == 199 {
            let _ = std::fs::remove_file(&entry);
        }
        match result {
            Ok((out, _err, res, _code)) => {
                if res.is_err() || out != "7\n" {
                    let _ = std::fs::remove_file(&entry);
                }
                assert!(
                    res.is_ok(),
                    "iter {i}: spurious fault (the #1 false-positive killing the parked sibling?): {res:?}"
                );
                assert_eq!(out, "7\n", "iter {i}: wrong output");
            }
            Err(_) => {
                let _ = std::fs::remove_file(&entry);
                panic!("iter {i}: hung — D5 owe #3 Path C regressed");
            }
        }
    }
}

/// D5 owe #1 — a blocking *subprocess* native (`process.cmd`, returns `Result[str]`) is offloaded,
/// runs off the core worker, and its `Ok`/`Err` result is lowered + pushed on resume so the `match`
/// continues correctly past the call. `N` fibers (≫ the core pool) each run a trivial command and
/// bump a `Shared` on `Ok` — the join sum must be exactly `N` (every offloaded `cmd` returned `Ok`
/// and resumed into the arm). Guards the request/process classification + the Result-lowering
/// resume path for a non-`io`/`fs` blocking native. Watchdog 30 s.
#[test]
fn d5_owe1_blocking_process_cmd_offloads_and_resumes_correctly() {
    let n = 64usize;
    let src = format!(
        "\
import std.process

fn checker(sink: Shared[int]):
    match process.cmd(\"true\"):
        Ok(out): sink.update(fn(x): x + 1)
        Err(e): print(e.message())

fn main():
    sink := Shared(0)
    parallel:
        for _ in 0..{n}:
            spawn checker(sink)
    print(sink.get())

main()
"
    );
    let entry = write_temp_chz("d5_owe1_cmd", &src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    match result {
        Ok((out, _err, res, _code)) => {
            assert!(res.is_ok(), "process.cmd nursery faulted: {res:?}");
            assert_eq!(out, format!("{n}\n"));
        }
        Err(_) => panic!("hung — D5 owe #1 process.cmd offload/resume regressed"),
    }
}

/// D5 test helper: write a Chezzi source to a uniquely-named temp `.chz` file and return its path
/// (so `run_file_parallel` resolves `import std.*` through the real module graph, unlike
/// `compile_module_standalone`). The caller removes it after the run.
fn write_temp_chz(tag: &str, src: &str) -> std::path::PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("chezzi_{tag}_{}_{seq}.chz", std::process::id()));
    std::fs::write(&path, src).expect("write temp .chz");
    path
}

/// C-ABI FFI under the M:N engine: an extern fn called INSIDE a `spawn:` block exercises the
/// `SnapValue::Cffi` snapshot path (the worker re-allocs `Obj::Cffi` from the shared `Arc` — no
/// re-dlopen, same address space). Must produce the same deterministic output as the cooperative
/// VM. Linux-only (needs libm.so.6). This is the hard parallel-parity proof from the FFI plan.
#[test]
#[cfg(target_os = "linux")]
fn extern_in_spawn_parallel_snapshot() {
    let src = "extern \"libm.so.6\":\n    fn sqrt(x: float) -> float\n\n\
                   ch := Channel[float]()\nparallel:\n    spawn:\n        ch.send(sqrt(9.0))\n\
                   r := ch.recv()\nprint(r)\n";
    let entry = write_temp_chz("ffi_spawn", src);
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(vm_res.is_ok(), "cooperative VM faulted: {vm_res:?}");
    assert!(par_res.is_ok(), "parallel engine faulted: {par_res:?}");
    assert_eq!(vm_out, "3.0\n");
    assert_eq!(
        vm_out, par_out,
        "cooperative VM and --parallel diverged on an extern-in-spawn call"
    );
}

/// Regression (blocker): an extern fn with an explicit `-> nil` (void) return must RUN, not panic.
/// The compiler's `ctype_of("nil")` is `None` meaning *void*, so the return slot must be built with
/// `and_then` (None ⇒ void), never `.expect`. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_explicit_nil_return_runs() {
    let src = "extern \"libc.so.6\":\n    fn srand(seed: int) -> nil\n\nsrand(1)\nprint(42)\n";
    let entry = write_temp_chz("ffi_nilret", src);
    let (out, _e, res, _) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "VM faulted on `-> nil` extern: {res:?}");
    assert_eq!(out, "42\n");
}

/// Regression (blocker): a type alias defined in an IMPORTED module, `from`-imported and used at
/// an extern signature, must lower to its underlying C type. The checker resolves it (module-scoped
/// types: a cross-module alias is reachable via `import Size from sizes`, not bare), and the FFI
/// backend consumes that checker-resolved C type — else `ctype_of` returns `None` and the call site
/// silently drops a scalar return. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_cross_module_alias_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_xmod_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("sizes.chz"), "type Size = int\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import Size from sizes\n\nextern \"libc.so.6\":\n    fn strlen(s: str) -> Size\n\nprint(strlen(\"hello\"))\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on cross-module alias extern: {vm_res:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on cross-module alias extern: {par_res:?}"
    );
    assert_eq!(vm_out, "5\n");
    assert_eq!(
        vm_out, par_out,
        "VM and --parallel diverged on a cross-module alias extern"
    );
}

/// Module-qualified enum-variant patterns (`geo.Color.Red`) in match arms, symmetric with
/// construction. The module binder is validated by the checker then dropped — both engines match
/// purely by the bare enum/variant identity, so VM == interp == --parallel byte-for-byte. Covers:
/// whole-module `import geo` qualified arms, an `import geo as g` aliased binder, and a
/// payload-binding `geo.Shape.Circle(r)` arm.
#[test]
fn match_module_qualified_variant_three_engine_parity() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_mqvp_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("geo.chz"),
        "enum Color:\n    Red\n    Green\n\nenum Shape:\n    Circle(int)\n    Square(int)\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import geo\nimport geo as g\n\nfn name(c: geo.Color) -> str:\n    match c:\n        geo.Color.Red: return \"red\"\n        geo.Color.Green: return \"green\"\n\nfn area(s: geo.Shape) -> int:\n    match s:\n        geo.Shape.Circle(r): return r * r\n        geo.Shape.Square(w): return w * w\n\nfn aliased(c: geo.Color) -> str:\n    match c:\n        g.Color.Red: return \"R\"\n        g.Color.Green: return \"G\"\n\nfn main():\n    print(name(geo.Color.Red))\n    print(name(geo.Color.Green))\n    print(area(geo.Shape.Circle(4)))\n    print(area(geo.Shape.Square(3)))\n    print(aliased(geo.Color.Green))\nmain()\n",
        )
        .unwrap();
    let cfg = crate::native::HostConfig::default;
    let (vo, _ve, vr, _vc) = run_file_with(&entry, cfg());
    let (po, _pe, pr, _pc) = run_file_parallel(&entry, cfg());
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert!(pr.is_ok(), "--parallel faulted: {pr:?}");
    assert!(ir.is_ok(), "interp faulted: {ir:?}");
    let expected = "red\ngreen\n16\n9\nG\n";
    assert_eq!(vo, expected, "cooperative VM output");
    assert_eq!(vo, po, "VM vs --parallel divergence");
    assert_eq!(vo, io, "VM vs interp divergence");
}

/// Bug #1 (L2) — a struct reached via a WHOLE-module import (`import geo`) is destructured with a
/// QUALIFIED struct pattern `geo.Point(x, y)` (the bare name is not in scope). Must RUN byte-identically
/// on the serial VM AND the M:N engine (a checker-superset guard: check_graph is asserted first).
#[test]
fn struct_match_qualified_whole_module_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_sqwm_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("geo.chz"),
        "struct Point:\n    x: int\n    y: int\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import geo\nfn f(p: geo.Point) -> int:\n    match p:\n        geo.Point(x, y): return x + y\nfn main():\n    print(f(geo.Point(3, 4)))\nmain()\n",
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "serial VM faulted: {vr:?}");
    assert!(ir.is_ok(), "M:N engine faulted: {ir:?}");
    assert_eq!(vo, "7\n", "serial VM output");
    assert_eq!(vo, io, "serial vs M:N divergence");
}

/// M24 Task 3 — a static-witness call ACROSS a module boundary must RUN, on both engines. Type-check
/// alone proves nothing here: the hidden argument is the concrete type's runtime IDENTITY KEY, which
/// is the OWNING module's (`<module-key>::Name`) — a wrong key surfaces only at run time, as
/// `type 'X' has no static method 'default'`. Every direction is exercised: the qualified callee
/// (`lib.reset`), the `from`-imported one (bare + aliased), a LOCAL type witnessing the IMPORTED
/// generic, an IMPORTED type witnessing a LOCAL generic, and a local generic FORWARDING its own
/// still-abstract witness into the imported one.
#[test]
fn witness_cross_module_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_xmod_witness_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.chz"),
        "protocol Default:\n    fn default() -> Self\n\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\n\nfn reset[T: Default](old: T) -> T:\n    return T.default()\n",
    )
    .unwrap();
    // A THIRD module in the middle: its `twice` forwards its own still-abstract witness into `lib`'s
    // `reset` — so a type declared in the ENTRY module reaches its `default()` across TWO boundaries.
    std::fs::write(
        dir.join("mid.chz"),
        "import lib\nimport reset from lib\n\nfn twice[U: Default](x: U) -> U:\n    return reset(lib.reset(x))\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import lib\nimport mid\nimport reset from lib\nimport reset as again from lib\n\nstruct Local:\n    k: str\n    fn default() -> Local:\n        return Local(\"loc\")\n\nfn mine[T: Default](x: T) -> T:\n    return T.default()\n\nfn fwd[U: Default](x: U) -> U:\n    return lib.reset(x)\n\nfn main():\n    print(lib.reset(lib.Counter(1)).n)\n    print(reset(lib.Counter(1)).n)\n    print(again(lib.Counter(1)).n)\n    print(lib.reset[lib.Counter](lib.Counter(1)).n)\n    print(reset(Local(\"x\")).k)\n    print(mine(lib.Counter(1)).n)\n    print(fwd(lib.Counter(1)).n)\n    print(fwd(Local(\"y\")).k)\n    print(mid.twice(Local(\"z\")).k)\n    print(mid.twice(lib.Counter(1)).n)\nmain()\n",
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "serial VM faulted: {vr:?}");
    assert!(ir.is_ok(), "M:N engine faulted: {ir:?}");
    assert_eq!(
        vo, "7\n7\n7\n7\nloc\n7\n7\nloc\nloc\n7\n",
        "serial VM output (a wrong identity key faults, a wrong witness prints another type)"
    );
    assert_eq!(vo, io, "serial vs M:N divergence");
}

/// M24 Task 5 — the same, for a witness declared BY A MEMBER: an instance method and a static method
/// on an IMPORTED type, witnessed by both an imported and a LOCALLY-declared type. The member's proto
/// is compiled in the DECLARING module (which is where its hidden param is added) while the witness
/// constant is pushed by the CALLING one, so the two modules' identity keys must agree — a mismatch
/// only shows up at run time.
#[test]
fn witness_member_cross_module_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_xmod_member_w_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.chz"),
        "protocol Default:\n    fn default() -> Self\n\nstruct Counter:\n    n: int\n    fn default() -> Counter:\n        return Counter(7)\n\nstruct Holder:\n    v: int\n    fn make[T: Default](self, old: T) -> T:\n        return T.default()\n    fn build[T: Default](old: T) -> T:\n        return T.default()\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import lib\nimport Holder, Counter from lib\n\nstruct Local:\n    k: str\n    fn default() -> Local:\n        return Local(\"loc\")\n\nfn main():\n    print(lib.Holder(1).make(lib.Counter(9)).n)\n    print(lib.Holder(1).make(Local(\"x\")).k)\n    print(lib.Holder.build(lib.Counter(9)).n)\n    print(lib.Holder.build(Local(\"x\")).k)\n    print(Holder(2).make(Counter(1)).n)\n    print(Holder.build(Local(\"y\")).k)\nmain()\n",
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "serial VM faulted: {vr:?}");
    assert!(ir.is_ok(), "M:N engine faulted: {ir:?}");
    assert_eq!(
        vo, "7\nloc\n7\nloc\n7\nloc\n",
        "serial VM output (a wrong identity key faults, a wrong witness builds the other type)"
    );
    assert_eq!(vo, io, "serial vs M:N divergence");
}

// ===== W7-49 — a spliced default must not alias the caller's own side-table entries =====
//
// These three are RUST tests, not `tests/chz/` ones, for one reason: they are inherently MULTI-FILE.
// The bug only exists because `desugar` splices a callee's default-parameter expression into the
// CALLER's AST as a clone that keeps the DEFINING module's spans, so it takes two files at matched
// `line:col` to express — which `chezzi test` (one file per suite) cannot say. The single-file half
// (both guards' over-fire modes) IS in Chezzi, at `tests/chz/spec/default_splice_keys_test.chz`.
//
// Each one is a `check`-clean program whose ANSWER is wrong on `19f7696a` and right now. The source
// is laid out flush-left in raw strings on purpose: the collision IS the column alignment, so the
// alignment has to be readable. Shift either side by one column and the fault disappears — which is
// what isolates it to the key rather than to the lowering.

/// W7-49 (1/3) — `KeywordTable`. `lib.chz`'s default `g(a=7, b=9)` and `main.chz`'s own
/// `g(b=1, a=2)` put their FIRST NAMED-ARG VALUE at the same `line:col`, so before `Span::file` they
/// shared one `KeywordKey`: the later insert won and lib's identity permutation was applied to
/// main's reversed call.
///
/// Fails on `19f7696a` with `709` / **`102`** — `h2(a=1, b=2)`, lib's permutation on main's call —
/// under a clean `chezzi check`, byte-identical on both engines (parity is blind to it).
#[test]
fn a_spliced_default_does_not_alias_the_callers_keyword_call() {
    let dir = std::env::temp_dir().join(format!("chezzi_w749_keyword_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // `fn f`'s first named-arg value `7` sits at line 6, col 19 (the two comments are the padding
    // that puts it there) — the same line:col as main's `1` below.
    std::fs::write(
        dir.join("lib.chz"),
        r#"# lib
# two comment lines put `fn f` on line 6
fn h(a: int, b: int) -> int:
    return a * 100 + b
g := h
fn f(x: int = g(a=7, b=9)) -> int:
    return x
"#,
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        r#"import f from lib
fn h2(a: int, b: int) -> int:
    return a * 100 + b
g := h2
fn probe() -> int:
    vvvvvv := g(b=1, a=2)
    return vvvvvv
print(f())
print(probe())
"#,
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "serial VM faulted: {vr:?}");
    assert!(ir.is_ok(), "M:N engine faulted: {ir:?}");
    assert_eq!(
        vo, "709\n201\n",
        "probe() is g(b=1, a=2) = h2(a=2, b=1) = 201; 102 means lib's permutation was applied"
    );
    assert_eq!(vo, io, "serial vs M:N divergence");
}

/// W7-49 (2/3) — `CarrierTable`. An Option-mode `?.` inside `lib.chz`'s default-parameter
/// expression and a Result-mode `?.` in `main.chz` land their NAME TOKENs at the same `line:col`
/// (both `5:36`), so before `Span::file` they shared one `CarrierKey` and main's `Result` carrier
/// got the `Option` lowering.
///
/// Fails on `19f7696a` with `3` then `runtime error … no match arm for variant 'Ok'`, under a clean
/// `chezzi check`, on both engines. CONTROL: widening main's `vvv…` binder by one character (so its
/// `len` lands at col 37) makes `19f7696a` print `3` / `4` — the fault is the KEY, not the lowering.
#[test]
fn a_spliced_default_carrier_does_not_alias_the_callers_carrier() {
    let dir = std::env::temp_dir().join(format!("chezzi_w749_carrier_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // The default is SELF-CONTAINED (`Some("abc")`, no lib global): a default is cloned into the
    // CALLER's scope, so one that names a lib-only global would not resolve in main at all.
    std::fs::write(
        dir.join("lib.chz"),
        r#"# four pad lines put `fn f` on line 5, so its
# `len` name token lands at exactly 5:36 — the
# same line:col as main's Result-mode carrier.
# The default is SELF-CONTAINED (no lib global).
fn f(x: Option[int] = Some("abc")?.len()) -> int:
    match x:
        Some(n): return n
        None: return -1
"#,
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        r#"import f from lib
fn getr() -> Result[str, str]:
    return Ok("wxyz")
fn probe() -> Result[int, str]:
    vvvvvvvvvvvvvvvvvvv := getr()?.len()
    return Ok(vvvvvvvvvvvvvvvvvvv)
print(f())
match probe():
    Ok(n): print(n)
    Err(e): print(e)
"#,
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vr.is_ok(),
        "serial VM faulted (the Option lowering on a Result carrier): {vr:?}"
    );
    assert!(ir.is_ok(), "M:N engine faulted: {ir:?}");
    assert_eq!(
        vo, "3\n4\n",
        "lib's Option carrier is Some(3); main's Result carrier is \"wxyz\".len() = 4"
    );
    assert_eq!(vo, io, "serial vs M:N divergence");
}

/// W7-49 (3/3) — `WitnessTable`. `lib.chz`'s default `empty[Counter]()` and `main.chz`'s
/// `empty()` (pinned to `Other` by its annotated result) put their CALLEE TOKEN at the same
/// `line:col` (`17:19`), so before `Span::file` they shared one `WitnessKey`.
///
/// MEASURED on `19f7696a`, not predicted: the mode is **the wrong CONCRETE TYPE constructed** —
/// `chezzi check` is clean, then `probe()` builds lib's `Counter` and faults
/// `no field 's' on Counter(n=7)`. The gaps row had listed three possible modes worst-first and
/// this is the MIDDLE one; neither the wrong-`argc` mode nor the false `internal:` compile error
/// occurs, because both colliding callees are the same one-witness `empty` and both witnesses
/// resolve to a concrete key. CONTROL: renaming main's `vvvv` binder one character longer makes
/// `19f7696a` print `7` / `oth`.
#[test]
fn a_spliced_default_witness_does_not_alias_the_callers_witness() {
    let dir = std::env::temp_dir().join(format!("chezzi_w749_witness_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // `empty` in `fn f`'s default sits at line 17, col 19.
    std::fs::write(
        dir.join("lib.chz"),
        r#"protocol Default:
    fn default() -> Self

struct Counter:
    n: int
    fn default() -> Counter:
        return Counter(7)

struct Other:
    s: str
    fn default() -> Other:
        return Other("oth")

fn empty[T: Default]() -> T:
    return T.default()

fn f(c: Counter = empty[Counter]()) -> int:
    return c.n
"#,
    )
    .unwrap();
    // `Counter` is imported here only because the SPLICED default's turbofish resolves in the
    // CALLER's scope — the same residual the whole row is about, in its benign form.
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        r#"import f, empty, Other, Counter from lib
# pad 2
# pad 3
# pad 4
# pad 5
# pad 6
# pad 7
# pad 8
# pad 9
# pad 10
# pad 11
# pad 12
# pad 13
# pad 14
# pad 15
fn probe() -> str:
    vvvv: Other = empty()
    return vvvv.s
print(f())
print(probe())
"#,
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        let _ = std::fs::remove_dir_all(&dir);
        panic!("program must type-check, got: {errs:?}");
    }
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vr.is_ok(),
        "serial VM faulted (lib's Counter witness on main's Other call): {vr:?}"
    );
    assert!(ir.is_ok(), "M:N engine faulted: {ir:?}");
    assert_eq!(
        vo, "7\noth\n",
        "f() witnesses Counter; probe() witnesses Other — a shared key builds Counter twice"
    );
    assert_eq!(vo, io, "serial vs M:N divergence");
}

/// W7-49 residual — the SURVIVING hole, made loud. `Span::file` separates modules, but the same
/// default spliced twice into ONE module still keeps one set of spans and therefore one key; a
/// default is cloned into the CALLER's scope, so a caller-side local can shadow the definer's global
/// and the two splices genuinely resolve differently. Here `g` is the module global `ab(a, b)` at one
/// site and the local `ba(b, a)` at the other, so the two permutations DIFFER.
///
/// On `19f7696a` this is a silent wrong value: `709` / **`907`** (both should be `709`), clean
/// `check`, both engines. Now the checker refuses to overwrite the key and the build stops. This is
/// a REJECT, so it is proven against its own premise, not just against the diagnostic: the same
/// program with the shadowing local removed — and the same-value re-insert that IS the common case —
/// stay green in `tests/chz/spec/default_splice_keys_test.chz`.
#[test]
fn a_double_spliced_default_that_resolves_two_ways_is_a_loud_error() {
    let dir = std::env::temp_dir().join(format!("chezzi_w749_residual_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("lib.chz"),
        r#"fn h(a: int, b: int) -> int:
    return a * 100 + b
g := h
fn f(x: int = g(a=7, b=9)) -> int:
    return x
"#,
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        r#"import f from lib
fn ab(a: int, b: int) -> int:
    return a * 100 + b
fn ba(b: int, a: int) -> int:
    return a * 100 + b
g := ab
fn probe() -> int:
    g := ba
    return f()
print(f())
print(probe())
"#,
    )
    .unwrap();
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    // The CHECKER still accepts it — the two `g`s are both `fn(int, int) -> int`. The disagreement
    // is a backend one, which is exactly why the backstop lives where the table is written.
    assert!(
        crate::checker::check_graph(&graph).is_ok(),
        "the program is well-typed; the conflict is a side-table one"
    );
    let err =
        crate::compiler::compile_graph(&graph).expect_err("the aliased key must stop the build");
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        err.message
            .contains("two different keyword-argument decisions"),
        "expected the side-table conflict backstop, got: {}",
        err.message
    );
}

/// Entry-last backstop — when the ENTRY file IS the always-injected prelude stub
/// (`chezzi run std/prelude.chz`), the resolver dedups the entry's own visit and the entry-last
/// reorder must restore `modules.last() == entry` so the positional-entry consumers designate the
/// right module. The stub is side-effect-free (no top-level output, no test fns), so all three
/// engines (cooperative VM, M:N `--parallel`, interp) must run clean with empty stdout —
/// byte-identical. Proves "runs clean, no panic" across all three engines.
#[test]
fn entry_is_always_linked_stub_runs_clean_three_engine() {
    let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let entry = manifest.join("std").join("prelude.chz");
    let cfg = crate::native::HostConfig::default;
    let (vo, _ve, vr, _vc) = run_file_with(&entry, cfg());
    let (po, _pe, pr, _pc) = run_file_parallel(&entry, cfg());
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    assert!(vr.is_ok(), "cooperative VM faulted on prelude.chz: {vr:?}");
    assert!(pr.is_ok(), "--parallel faulted on prelude.chz: {pr:?}");
    assert!(ir.is_ok(), "interp faulted on prelude.chz: {ir:?}");
    assert_eq!(vo, "", "cooperative VM stdout not empty on prelude.chz");
    assert_eq!(vo, po, "VM vs --parallel divergence on prelude.chz");
    assert_eq!(vo, io, "VM vs interp divergence on prelude.chz");
}

/// Task 4 — `import std.concurrency` then construct + use ALL FOUR runtime concurrency ctors must
/// RUN end-to-end byte-identically on the cooperative VM AND the interp (the deprecated parity
/// oracle). Exercises the native EMPTY-members module-object alloc on both engines + the
/// opcode-dispatched ctors (the import gate is checker-only, so runtime is unchanged).
#[test]
fn concurrency_whole_module_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_conc_whole_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import std.concurrency\nfn main():\n    s := Shared(0)\n    s.set(5)\n    r := RwShared(1)\n    a := Atomic(2)\n    ex := Executor()\n    ex.shutdown()\n    print(s.get())\n    print(r.get())\n    print(a.load())\nmain()\n",
        )
        .unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert!(ir.is_ok(), "interp faulted: {ir:?}");
    let expected = "5\n1\n2\n";
    assert_eq!(vo, expected, "cooperative VM output");
    assert_eq!(vo, io, "VM vs interp divergence");
}

/// Task 4 — CRITICAL FIX 2: a SELECTIVE `import Shared from std.concurrency` must RUN on both
/// engines. This is the exact case the prior attempt crashed: the from-import type-checks green but
/// the engine `bind_import` would fault `module 'std.concurrency' has no member 'Shared'` without
/// the runtime skip. The ctor is resolved by the compiler name→opcode dispatch, not a bound member.
#[test]
fn concurrency_from_import_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_conc_from_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import Shared from std.concurrency\nfn main():\n    print(Shared(0).get())\nmain()\n",
    )
    .unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted (bind_import skip missing?): {vr:?}");
    assert!(
        ir.is_ok(),
        "interp faulted (bind_import skip missing?): {ir:?}"
    );
    assert_eq!(vo, "0\n", "cooperative VM output");
    assert_eq!(vo, io, "VM vs interp divergence");
}

/// Gate `Socket`/`Listener` behind `import std.net` — a `Socket` carries no runtime module-member
/// value (the runtime resolves `Ty::Socket` directly; the ctor is `connect`/`listen`), so a
/// from-import (`import Socket from std.net`) would fault `module 'std.net' has no member 'Socket'`
/// WITHOUT the `bind_import` skip. This pins the skip on BOTH engines (single-engine fault = red)
/// plus a whole-module twin. Phase 4c-net: the bodies now also call `Socket`/`Listener` METHODS
/// (`read`/`write`/`accept`/`close`) so the harvested method table (the retired bespoke arm's
/// replacement) is exercised at check time on both engines; the `use_*` fns are checked though never
/// called (no live I/O — the method resolution is a front-end concern, identical across engines).
#[test]
fn net_from_import_runs_both_engines() {
    for src in [
        "import Socket from std.net\nfn use_sock(s: Socket) -> int!:\n    a := s.read(64)?\n    n := s.write(a)?\n    s.close()\n    return Ok(n)\nfn main():\n    print(1)\nmain()\n",
        "import std.net\nfn use_sock(s: Socket) -> int!:\n    a := s.read(64, 100)?\n    n := s.write(a, 100)?\n    s.close()\n    return Ok(n)\nfn use_listener(l: Listener) -> str!:\n    c := l.accept()?\n    ad := l.addr()?\n    c.close()\n    l.close()\n    return Ok(ad)\nfn main():\n    print(1)\nmain()\n",
    ] {
        let dir = std::env::temp_dir().join(format!("chezzi_vm_net_from_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let entry = dir.join("main.chz");
        std::fs::write(&entry, src).unwrap();
        let (vo, _ve, vr, _vc) = run_file(&entry);
        let (io, _ie, ir, _ic) = run_file_p(&entry);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(vr.is_ok(), "VM faulted (bind_import skip missing?): {vr:?}");
        assert!(
            ir.is_ok(),
            "interp faulted (bind_import skip missing?): {ir:?}"
        );
        assert_eq!(vo, "1\n", "cooperative VM output");
        assert_eq!(vo, io, "VM vs interp divergence");
    }
}

/// Gate `timer` behind `import std.time` — a whole-module `import std.time` then `timer(50).recv()`
/// must RUN end-to-end byte-identically on the cooperative VM AND the interp (the deprecated parity
/// oracle). The import gate is checker-only, so the opcode-dispatched `timer` runtime is unchanged;
/// the timer fires after 50ms and `recv()` yields `true`.
#[test]
fn timer_whole_module_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_timer_whole_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import std.time\nfn main():\n    print(timer(50).recv())\nmain()\n",
    )
    .unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted: {vr:?}");
    assert!(ir.is_ok(), "interp faulted: {ir:?}");
    assert_eq!(vo, "true\n", "cooperative VM output");
    assert_eq!(vo, io, "VM vs interp divergence");
}

/// CRITICAL — a SELECTIVE `import timer from std.time` must RUN on both engines. `timer` is opcode-
/// backed with NO runtime module-member value, so the from-import type-checks green but BOTH engines
/// would fault `module 'std.time' has no member 'timer'` WITHOUT the timer-specific `bind_import`
/// skip. The call resolves via the compiler name→opcode dispatch, not a bound member.
#[test]
fn timer_from_import_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_timer_from_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import timer from std.time\nfn main():\n    print(timer(50).recv())\nmain()\n",
    )
    .unwrap();
    let (vo, _ve, vr, _vc) = run_file(&entry);
    let (io, _ie, ir, _ic) = run_file_p(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vr.is_ok(), "VM faulted (bind_import skip missing?): {vr:?}");
    assert!(
        ir.is_ok(),
        "interp faulted (bind_import skip missing?): {ir:?}"
    );
    assert_eq!(vo, "true\n", "cooperative VM output");
    assert_eq!(vo, io, "VM vs interp divergence");
}

/// Regression (blocker): a module-QUALIFIED struct type (`cdefs.DivT`) written at the extern
/// return boundary must lower to its C struct just like the bare/named-import spelling. The
/// qualified `Type::Qualified` was previously passed through unchanged → `ctype_of` returned
/// `None` → silent void return → reading a field of the (nil) result faulted. All three engines
/// must yield quot=3, rem=2 for `div(17, 5)`. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_return_struct_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qret_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("cdefs.chz"),
        "import int32 from std.ffi\n\nstruct DivT:\n    quot: int32\n    rem: int32\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import core.cdefs\nimport int32 from std.ffi\n\nextern \"libc.so.6\":\n    \
             fn div(numer: int32, denom: int32) -> cdefs.DivT\n\nr: cdefs.DivT = div(17, 5)\n\
             print(r.quot)\nprint(r.rem)\n",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (io, _ie, ir, _) = run_file_p(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on qualified return struct extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on qualified return struct extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on qualified return struct extern: {par_res:?}"
    );
    assert_eq!(vm_out, "3\n2\n");
    assert_eq!(
        vm_out, io,
        "VM and interp diverged (qualified return struct)"
    );
    assert_eq!(
        vm_out, par_out,
        "VM and --parallel diverged (qualified return struct)"
    );
}

/// Regression (blocker): a module-QUALIFIED width alias (`w3.Len`, `type Len = int32`) at both
/// the extern PARAM and return boundary must resolve through the alias table like the bare/
/// named-import spelling. Previously the qualified param panicked the VM at the marshal loop's
/// `.expect("checker verified marshallable param")`. All three engines must yield 7 for
/// `abs(-7)`. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_param_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qparam_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("w3.chz"),
        "import int32 from std.ffi\n\ntype Len = int32\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import core.w3\n\nextern \"libc.so.6\":\n    fn abs(n: w3.Len) -> w3.Len\n\nprint(abs(-7))\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (io, _ie, ir, _) = run_file_p(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted/panicked on qualified width-alias param extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on qualified width-alias param extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on qualified width-alias param extern: {par_res:?}"
    );
    assert_eq!(vm_out, "7\n");
    assert_eq!(vm_out, io, "VM and interp diverged (qualified width param)");
    assert_eq!(
        vm_out, par_out,
        "VM and --parallel diverged (qualified width param)"
    );
}

/// Regression (blocker, the adversarial-panel find): a module-QUALIFIED width alias must resolve
/// to its DEFINING module's body even when the CALLING module declares a colliding bare alias of
/// the SAME name but a DIFFERENT width. `w3.Len` (= int64 in core/w3) must marshal as int64 — NOT
/// collapse to the calling module's local `type Len = int8`. With the bug, `abs(-300)` rounds
/// through int8 to 44; correctly resolved (int64) it stays 300. All three engines must agree on
/// 300. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_param_collision_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qcollide_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("w3.chz"),
        "import int64 from std.ffi\n\ntype Len = int64\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import core.w3\nimport int8 from std.ffi\n\ntype Len = int8\n\nextern \"libc.so.6\":\n    \
             fn abs(n: w3.Len) -> w3.Len\n\nprint(abs(-300))\n",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (io, _ie, ir, _) = run_file_p(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on colliding qualified width-alias extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on colliding qualified width-alias extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on colliding qualified width-alias extern: {par_res:?}"
    );
    assert_eq!(
        vm_out, "300\n",
        "qualified `w3.Len` must marshal as the DEFINING module's int64 (300), not the local int8 (44)"
    );
    assert_eq!(
        vm_out, io,
        "VM and interp diverged (colliding qualified width param)"
    );
    assert_eq!(
        vm_out, par_out,
        "VM and --parallel diverged (colliding qualified width param)"
    );
}

/// Regression (ROOT, the deeper adversarial find): a module-QUALIFIED width alias whose body is
/// itself ANOTHER alias (a CHAIN, `type Len = Inner; type Inner = int64`) must resolve EVERY hop
/// in its DEFINING module's scope — NOT re-enter the flat last-write-wins bare `aliases` map on
/// the inner hop. The calling module declares a colliding bare `type Inner = int8`; with the bug
/// the inner hop collapses to int8 and `abs(-300)` rounds to 44. Correctly resolved (int64) it
/// stays 300 across all three engines. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_chain_depth2_collision_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qchain2_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("w3.chz"),
        "import int64 from std.ffi\n\ntype Inner = int64\ntype Len = Inner\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import core.w3\nimport int8 from std.ffi\n\ntype Inner = int8\n\nextern \"libc.so.6\":\n    \
             fn abs(n: w3.Len) -> w3.Len\n\nprint(abs(-300))\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (io, _ie, ir, _) = run_file_p(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on chained qualified width-alias extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on chained qualified width-alias extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on chained qualified width-alias extern: {par_res:?}"
    );
    assert_eq!(
        vm_out, "300\n",
        "chained `w3.Len -> Inner -> int64` must marshal as int64 (300), not the inner hop's colliding local int8 (44)"
    );
    assert_eq!(
        vm_out, io,
        "VM and interp diverged (chained qualified width param, depth 2)"
    );
    assert_eq!(
        vm_out, par_out,
        "VM and --parallel diverged (chained qualified width param, depth 2)"
    );
}

/// Regression (ROOT, depth-3): proves arbitrary chain depth, not just one inner hop. `w3.Len ->
/// A -> B -> int64`, with the calling module colliding BOTH inner names (`type A = int8`,
/// `type B = int8`). Every hop must resolve in w3's scope; correctly resolved it stays 300, the
/// bug rounds to 44. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_chain_depth3_collision_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qchain3_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("w3.chz"),
        "import int64 from std.ffi\n\ntype B = int64\ntype A = B\ntype Len = A\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import core.w3\nimport int8 from std.ffi\n\ntype A = int8\ntype B = int8\n\nextern \"libc.so.6\":\n    \
             fn abs(n: w3.Len) -> w3.Len\n\nprint(abs(-300))\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (io, _ie, ir, _) = run_file_p(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on depth-3 chained qualified width-alias extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on depth-3 chained qualified width-alias extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on depth-3 chained qualified width-alias extern: {par_res:?}"
    );
    assert_eq!(
        vm_out, "300\n",
        "depth-3 `w3.Len -> A -> B -> int64` must marshal as int64 (300), not the colliding local int8 (44)"
    );
    assert_eq!(
        vm_out, io,
        "VM and interp diverged (chained qualified width param, depth 3)"
    );
    assert_eq!(
        vm_out, par_out,
        "VM and --parallel diverged (chained qualified width param, depth 3)"
    );
}

/// A CYCLIC qualified alias chain (`type A = B; type B = A`, no scalar leaf) used at an extern
/// boundary must surface a clean "not C-marshallable" error and — critically — must NOT hang or
/// overflow the stack while following the chain. The recursive module-scoped resolver is bounded
/// by a visited set: a repeated hop yields `None`, which propagates to the marshal backstop's
/// clean error. No libc needed (the checker/marshal gate rejects before any dlopen). The test
/// merely COMPLETING proves no infinite loop.
#[test]
fn extern_qualified_alias_cycle_is_clean_error() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qcycle_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("w3.chz"), "type A = B\ntype B = A\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import core.w3\n\nextern \"libc.so.6\":\n    fn abs(n: w3.A) -> w3.A\n\nprint(0)\n",
    )
    .unwrap();
    let (_vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_err(),
        "expected a clean error for a cyclic qualified alias extern type, got Ok"
    );
    let msg = format!("{:?}", vm_res.unwrap_err());
    assert!(
        msg.contains("not C-marshallable") || msg.contains("C-marshallable"),
        "error should mention marshallability, got: {msg}"
    );
}

/// A genuinely non-marshallable QUALIFIED type at an extern boundary must surface a clean
/// compile/runtime error (the checker is the real gate), and — even if a path slipped past the
/// checker — the marshal loop's backstop must NEVER panic the VM. No libc needed (the checker
/// rejects before any dlopen). Asserts `is_err`, the "not C-marshallable" wording, and that the
/// process did not abort (the test simply completing proves no panic).
#[test]
fn extern_non_marshallable_qualified_is_clean_error() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qbad_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("bag.chz"), "struct Bag:\n    items: List[int]\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import core.bag\n\nextern \"libc.so.6\":\n    fn use_it(b: bag.Bag) -> int\n\nprint(0)\n",
    )
    .unwrap();
    let (_vm_out, _e, vm_res, _) = run_file(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_err(),
        "expected a clean error for a non-marshallable qualified extern type, got Ok"
    );
    let msg = format!("{:?}", vm_res.unwrap_err());
    assert!(
        msg.contains("not C-marshallable") || msg.contains("C-marshallable"),
        "error should mention marshallability, got: {msg}"
    );
}

/// Regression (ROOT, the named-import chain hop — fix4): a module-QUALIFIED width alias whose
/// resolution hops through a NAMED-IMPORTED alias (`import W from core.widths` in the defining
/// module) must resolve EVERY hop in its own module's import/alias scope — NOT fall back to the
/// flat bare `aliases` map on the imported hop. `main` declares a colliding `type W = int8`; the
/// true chain is `w3.Len -> W(from widths) -> int64`. With the bug the imported hop collapses to
/// main's int8 and `abs(-300)` rounds to 44; correctly resolved (int64) it stays 300 across all
/// three engines. Linux-only (needs libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_named_import_hop_collision_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qnamedhop_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("widths.chz"),
        "import int64 from std.ffi\n\ntype W = int64\n",
    )
    .unwrap();
    std::fs::write(
        core.join("w3.chz"),
        "import W from core.widths\n\ntype Len = W\n",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import core.w3\nimport int8 from std.ffi\n\ntype W = int8\n\nextern \"libc.so.6\":\n    \
             fn abs(n: w3.Len) -> w3.Len\n\nprint(abs(-300))\n",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (io, _ie, ir, _) = run_file_p(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on named-import-hop qualified width-alias extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on named-import-hop qualified width-alias extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on named-import-hop qualified width-alias extern: {par_res:?}"
    );
    assert_eq!(
        vm_out, "300\n",
        "named-import-hop `w3.Len -> W(from widths) -> int64` must marshal as int64 (300), not main's colliding int8 (44)"
    );
    assert_eq!(
        vm_out, io,
        "VM and interp diverged (named-import-hop qualified width param)"
    );
    assert_eq!(
        vm_out, par_out,
        "VM and --parallel diverged (named-import-hop qualified width param)"
    );
}

/// Regression (ROOT, the struct-field scope — fix5): a qualified extern RETURN STRUCT whose FIELDS
/// are typed via the DEFINING module's LOCAL alias (`type Half = int32`; `struct DivT{quot:Half;
/// rem:Half}`). fix4 resolved the struct's fields in the IMPORTER's scope (where `Half` is
/// invisible) → field None → struct CType None → void return → `cannot read field 'quot' of nil`.
/// The single-resolver fix computes the struct's CType in ITS defining module's scope, so the
/// return marshals as a real two-int32 struct and `div(17,5)` reads quot 3 / rem 2 on all three
/// engines. Linux-only (needs libc.so.6 `div`).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_return_struct_aliased_field_runs() {
    let dir =
        std::env::temp_dir().join(format!("chezzi_vm_ffi_structalias_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
            core.join("cdefs.chz"),
            "import int32 from std.ffi\n\ntype Half = int32\n\nstruct DivT:\n    quot: Half\n    rem: Half\n",
        )
        .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import core.cdefs\nimport int32 from std.ffi\n\nextern \"libc.so.6\":\n    \
             fn div(numer: int32, denom: int32) -> cdefs.DivT\n\nr := div(17, 5)\nprint(r.quot)\nprint(r.rem)\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (io, _ie, ir, _) = run_file_p(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on qualified return struct with aliased fields: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on qualified return struct with aliased fields: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on qualified return struct with aliased fields: {par_res:?}"
    );
    assert_eq!(
        vm_out, "3\n2\n",
        "qualified return struct whose fields use the defining module's local alias must marshal as two int32 (quot 3, rem 2)"
    );
    assert_eq!(
        vm_out, io,
        "VM and interp diverged (qualified return struct aliased fields)"
    );
    assert_eq!(
        vm_out, par_out,
        "VM and --parallel diverged (qualified return struct aliased fields)"
    );
}

/// Regression (ROOT, the MIXED chain — fix4): a single qualified alias whose resolution hops
/// through a LOCAL alias, then a NAMED-IMPORTED alias, then a QUALIFIED alias — with a colliding
/// `type W = int8` shadow at each module along the way — must resolve to the true width (int64)
/// at every hop in that hop's own module scope. The chain: main's extern names `mid.Outer`;
/// `mid` declares `type Outer = ImpW` (local hop) where `ImpW` is `import ImpW from core.widths`
/// (named-import hop) and `core.widths` declares `type ImpW = base.Base` (qualified hop into
/// `core.base` `type Base = int64`). Every module ALSO declares a colliding `type W = int8` and
/// some declare colliding `Outer`/`ImpW`/`Base` bare. Correctly resolved it stays 300; any single
/// mis-resolved hop rounds to 44. All three engines must agree on 300. Linux-only (libc.so.6).
#[test]
#[cfg(target_os = "linux")]
fn extern_qualified_width_alias_mixed_chain_collision_runs() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_ffi_qmixed_{}", std::process::id()));
    let core = dir.join("core");
    std::fs::create_dir_all(&core).unwrap();
    // core/base.chz: the true leaf width (`Base = int64`). It ALSO declares `Outer = int8` and
    // `ImpW = int8` — the names the OTHER modules' hops use — so a stray flat-map fallback there
    // would round to 44.
    std::fs::write(
            core.join("base.chz"),
            "import int64 from std.ffi\nimport int8 from std.ffi\n\ntype Base = int64\ntype Outer = int8\ntype ImpW = int8\n",
        )
        .unwrap();
    // core/widths.chz: a QUALIFIED hop into base (`ImpW = base.Base`). It collides `Base` (its
    // own qualified body-name spelled bare here would be int8) and `Outer`.
    std::fs::write(
            core.join("widths.chz"),
            "import core.base\nimport int8 from std.ffi\n\ntype ImpW = base.Base\ntype Base = int8\ntype Outer = int8\n",
        )
        .unwrap();
    // mid.chz: a NAMED-IMPORT hop (`ImpW from core.widths`), then a LOCAL hop (`Outer = ImpW`).
    // It does NOT locally redefine `ImpW` (that would legitimately shadow the import); the
    // collisions live in the OTHER modules. It collides `Base`.
    std::fs::write(
            dir.join("mid.chz"),
            "import ImpW from core.widths\nimport int8 from std.ffi\n\ntype Outer = ImpW\ntype Base = int8\n",
        )
        .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
            &entry,
            "import mid\nimport int8 from std.ffi\n\ntype Outer = int8\ntype ImpW = int8\ntype Base = int8\n\nextern \"libc.so.6\":\n    \
             fn abs(n: mid.Outer) -> mid.Outer\n\nprint(abs(-300))\n",
        )
        .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (io, _ie, ir, _) = run_file_p(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "VM faulted on mixed-chain qualified width-alias extern: {vm_res:?}"
    );
    assert!(
        ir.is_ok(),
        "interp faulted on mixed-chain qualified width-alias extern: {ir:?}"
    );
    assert!(
        par_res.is_ok(),
        "parallel engine faulted on mixed-chain qualified width-alias extern: {par_res:?}"
    );
    assert_eq!(
        vm_out, "300\n",
        "mixed chain (local -> named-import -> qualified) must marshal as int64 (300), not any colliding int8 hop (44)"
    );
    assert_eq!(
        vm_out, io,
        "VM and interp diverged (mixed-chain qualified width param)"
    );
    assert_eq!(
        vm_out, par_out,
        "VM and --parallel diverged (mixed-chain qualified width param)"
    );
}

/// D3/the discriminating fairness test. 64 CPU "hog" fibers (≫ the core-sized worker pool) each
/// busy-wait on a `Shared[int]` until it reaches 50, spawned FIRST; then 50 "short" fibers that
/// each `update(+1)` and exit. WITHOUT preemption every worker grabs a hog (FIFO seed order), all
/// spin forever on a counter the never-scheduled shorts can't advance → permanent hang. WITH
/// reduction-counting preemption the hogs yield, the shorts run, the counter reaches 50, the hogs
/// observe it and exit. A watchdog turns the no-preemption hang into a test FAILURE (not an
/// infinite hang) and stands as the regression guard if preemption ever regresses.
#[test]
fn d3_preemption_prevents_cpu_hog_starvation() {
    let src = "\
fn hog(s: Shared[int], k: int):
    while s.get() < k:
        continue

fn short(s: Shared[int]):
    s.update(fn(x): x + 1)

fn main():
    s := Shared(0)
    parallel:
        for _ in 0..64:
            spawn hog(s, 50)
        for _ in 0..50:
            spawn short(s)
    print(s.get())

main()
";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("hog/short nursery completed"), "50\n"),
        Err(_) => {
            panic!("starved — D3 preemption regressed (CPU hogs never yielded their workers)")
        }
    }
}

/// D3/soundness: thousands of CPU-bound fibers (each a bounded loop + a `Shared` increment), far
/// more than the worker pool, all complete under heavy yield churn — no corruption, no lost fiber,
/// no false deadlock. Bounded loops terminate regardless of preemption, so this is a soundness
/// guard for the yield/requeue machinery rather than the discriminating fairness test above.
#[test]
fn d3_thousands_of_cpu_fibers_all_complete() {
    let src = "\
fn work(s: Shared[int]):
    i := 0
    while i < 100:
        i += 1
    s.update(fn(x): x + 1)

fn main():
    s := Shared(0)
    parallel:
        for _ in 0..10000:
            spawn work(s)
    print(s.get())

main()
";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(60)) {
        Ok(r) => assert_eq!(r.expect("10k-fiber nursery completed"), "10000\n"),
        Err(_) => {
            panic!("10k CPU-bound fibers did not all complete in time (yield machinery hang?)")
        }
    }
}

/// D3/regression: a reduction yield must unwind cleanly through **nested function calls**. A yield
/// is detected at the safepoint of the innermost `run_until`; every enclosing `run_proto`/call site
/// must propagate it up WITHOUT popping a result (the frames replay on resume) — the same contract
/// as a `recv`-park. The first cut only guarded `suspend`, so a yield deep in a call chain
/// (main → work → middle → inner, the shape of `primes_parallel`) let `run_proto` pop a live stack
/// temp as a bogus return value → "expected bool, found int". This computes a known sum across two
/// workers, each looping 50 k times through a 3-deep call chain (millions of ops ≫ CONTEXT_REDS, so
/// many yields fire mid-chain), and also crosses a channel `recv` — exercising yield + park together.
#[test]
fn d3_yield_unwinds_through_nested_calls() {
    let src = "\
fn inner(n: int) -> int:
    return n * 2

fn middle(n: int) -> int:
    return inner(n) + 1

fn work(lo: int, hi: int, out: Channel[int]):
    acc := 0
    i := lo
    while i < hi:
        acc += middle(i)
        i += 1
    out.send(acc)

fn main():
    out := Channel[int]()
    parallel:
        spawn work(0, 50000, out)
        spawn work(50000, 100000, out)
    total := 0
    for _ in 0..2:
        total += out.recv()
    print(total)

main()
";
    // sum_{i=0}^{99999} (2*i + 1) = 100000 * 100000.
    assert_eq!(run_capture_parallel(src).unwrap(), "10000000000\n");
}

/// D2b park-gap guard (the lost-wakeup fix): if a message is already queued when `park` runs (a
/// `send` landed in the gap between `recv`'s empty-check and the park), the fiber must NOT park —
/// it is requeued `Ready` so it re-runs `recv` and pops the message. Without this the fiber would
/// park forever behind a delivered-but-unconsumed message → a false deadlock.
#[test]
fn mnsched_park_requeues_when_message_already_waiting() {
    let sched = mk_sched(1);
    let core = empty_core();
    let key = core_key(&core);
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    // Simulate a send that landed in the gap (message queued, but this fiber wasn't parked yet).
    core.q.lock().unwrap().push(
        crate::vm::core::wire_summary(&WireValue::Int(7)),
        WireValue::Int(7),
    );
    sched.park(key, &core, f);
    let c = sched.lock();
    assert_eq!(c.parked_n, 0, "must not park behind a waiting message");
    assert_eq!(c.global.len(), 1, "fiber requeued to re-run recv");
}

/// D2b park-gap guard (cancel half): if cancel was tripped in the gap before the park, the fiber
/// must be requeued (to unwind on the back-edge) rather than parked (where it would be stranded).
#[test]
fn mnsched_park_requeues_when_cancel_tripped() {
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(1, 4, Arc::clone(&cancel), dl_err(), 0);
    let core = empty_core();
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    cancel.store(true, Ordering::Relaxed);
    sched.park(core_key(&core), &core, f);
    let c = sched.lock();
    assert_eq!(c.parked_n, 0, "must not park a cancelled fiber");
    assert_eq!(c.global.len(), 1);
}

/// D2b/U4: every not-done fiber parked, none running, run queue empty ⇒ deadlock. `take_runnable`
/// detects it, records a `Deadlocked` outcome (`err.message == DEADLOCK_MSG`) for every parked
/// fiber, and terminates.
#[test]
fn mnsched_deadlock_when_all_parked_runq_empty() {
    let sched = mk_sched(2);
    let c1 = empty_core();
    let c2 = empty_core();
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    sched.park(core_key(&c1), &c1, a);
    sched.park(core_key(&c2), &c2, b);
    assert!(matches!(sched.take_runnable(0, 1, 0), Take::Stop));
    let slots = sched.take_slots();
    assert_eq!(slots.len(), 2);
    for s in slots {
        assert!(
            matches!(s, Some(TaskOutcome::Deadlocked { err, .. }) if err.message == DEADLOCK_MSG)
        );
    }
}

/// N4 (cancel-teardown veto): a scope whose `cancel` is tripped but whose tasks have not all
/// settled is MID-TEARDOWN, not deadlocked. Every cancel trip and its `cancel_drain` are two
/// separate core-lock acquisitions apart (`mn_worker_loop`'s `finish`→`cancel_drain`,
/// `abort_enlisted_scope`, `abort_eager_nursery`), and an idle worker's `take_runnable` can land in
/// that gap and see the pre-drain quiesce (`running == 0 && runnable == 0 && parked_n > 0`). Without
/// the veto it declares DEADLOCK and `flag_deadlock` DROPS the parked siblings without running
/// `unwind_deferred` — silently skipping their `defer`s. Pins the invariant itself (the scenario
/// repro is `parallel_defer_runs_on_cancelled_sibling`).
#[test]
fn mnsched_cancelled_scope_with_parked_fibers_is_not_deadlock() {
    // Scope 0's `JoinScope::cancel` IS this Arc (see `mnsched_park_requeues_when_cancel_tripped`).
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(2, 4, Arc::clone(&cancel), dl_err(), 0);
    let c1 = empty_core();
    let c2 = empty_core();
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    // Cancel still false → these really park (the park-gap re-check does not requeue them).
    sched.park(core_key(&c1), &c1, a);
    sched.park(core_key(&c2), &c2, b);
    {
        let c = sched.lock();
        assert_eq!(c.parked_n, 2, "both fibers parked (quiesced, pre-cancel)");
    }
    // A sibling faults: `classify_mn_outcome` trips the scope cancel BEFORE `finish`/`cancel_drain`.
    cancel.store(true, Ordering::Relaxed);
    let c = sched.lock();
    assert!(
        !sched.is_deadlocked(&c),
        "a cancel-tripped scope mid-teardown is not deadlocked (its parked fibers are about to be \
         drained by `cancel_drain` so they can unwind their defers)"
    );
}

/// N4 boundary (the other half): the veto covers the trip→`cancel_drain` window ONLY. A cancelled
/// scope whose last unsettled fiber is DEMOTED — blocked in place inside its own `defer` on a `recv`
/// nobody will ever answer — has no undrained park left, so nothing can ever wake it: that IS a
/// genuine deadlock and must be REPORTED, never left to hang silently (`demote_recv_block`'s own
/// `is_deadlocked` self-detect is what fires). Before the veto was bounded to the parked window the
/// scope stayed `done < total && cancel` forever (incomplete *because* that fiber is stuck), so the
/// veto never lifted → silent M:N hang (repro: a `defer` whose body does a `recv` nobody answers).
#[test]
fn mnsched_cancelled_scope_whose_only_fiber_is_demoted_is_deadlock() {
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(2, 4, Arc::clone(&cancel), dl_err(), 0);
    let core = empty_core();
    let ptr = core_key(&core);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    // Fiber b enters its `defer` and blocks in place on an empty channel (running → blocked_native).
    // Inside a `defer` a cancel is SUPPRESSED (`cancel_requested()`'s `deferring == 0` term), so it
    // registers NO cancel watch — nothing can ever wake it. That is what makes it a real deadlock.
    {
        let mut c = sched.lock();
        c.running -= 1;
        sched.blocked_native.fetch_add(1, Ordering::Relaxed);
        c.register_demoted(ptr, &core);
        c.watch_demoted_cancel(vec![]);
    }
    // Sibling a faults: trips the scope cancel, then finishes. No fiber of the scope is parked.
    cancel.store(true, Ordering::Relaxed);
    sched.finish(
        a.task_index,
        0,
        TaskOutcome::Fault {
            err: dl_err(),
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    let c = sched.lock();
    assert_eq!(c.parked_n, 0, "no fiber of the cancelled scope is parked");
    assert!(
        sched.is_deadlocked(&c),
        "a cancelled scope whose remaining fiber is demoted-blocked forever inside its cleanup is a \
         REAL deadlock — report it, never hang"
    );
    let _ = b;
}

/// N4 (the DEMOTED half of the cancel-teardown veto): a demoted fiber whose cancel flag is TRIPPED is
/// NOT stuck — `demote_recv_block` ranks `cancel_requested()` ABOVE `terminate` and its own deadlock
/// self-detect (sched.rs), so it resumes within one `DEMOTE_POLL_BACKOFF`, unwinds and runs its
/// `defer`s (which can `send`, waking parked siblings). CANCEL is a wakeup source the park/inflight/
/// `runnable` counters do not model, so without a veto an idle worker's `take_runnable` sees
/// `running == 0 && runnable == 0 && inflight == 0 && blocked_native > 0` and declares a SPURIOUS
/// deadlock: `flag_deadlock` then reaps every parked fiber of EVERY scope with no `unwind_deferred`
/// (their `defer`s silently skipped — the exact N4 harm) and LATCHES `terminate`, which truncates the
/// cleanup of any sibling demoted inside its own `defer`. The parked-only veto
/// (`any_cancelled_scope_awaiting_drain`) cannot see this fiber — it is in `blocked_native`, not
/// `parked` — hence the explicit watch. Boundary vs the test above: a fiber demoted INSIDE a `defer`
/// registers no watch and stays a genuine deadlock, so the hang fix is preserved.
#[test]
fn mnsched_demoted_fiber_with_a_tripped_cancel_is_not_deadlock() {
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(2, 4, Arc::clone(&cancel), dl_err(), 0);
    let core = empty_core();
    let ptr = core_key(&core);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    // Fiber b blocks in place on an empty channel from inside a native callback, in its BODY
    // (`deferring == 0`) — a cancel would be honoured, so it watches its scope's flag.
    let tok = {
        let mut c = sched.lock();
        c.running -= 1;
        sched.blocked_native.fetch_add(1, Ordering::Relaxed);
        c.register_demoted(ptr, &core);
        c.watch_demoted_cancel(vec![Arc::clone(&cancel)])
    };
    // Sibling a faults: trips the scope cancel, then finishes. No fiber of the scope is parked, so
    // the parked-window veto is silent — only the demoted watch can save b.
    cancel.store(true, Ordering::Relaxed);
    sched.finish(
        a.task_index,
        0,
        TaskOutcome::Fault {
            err: dl_err(),
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    {
        let c = sched.lock();
        assert_eq!(c.parked_n, 0, "no fiber of the cancelled scope is parked");
        assert!(
            !sched.is_deadlocked(&c),
            "a demoted fiber with a tripped cancel is about to resume and unwind — that is progress, \
             not a deadlock (declaring one latches `terminate` and skips every parked fiber's defers)"
        );
    }
    // It resumes, unwinds and settles: the watch is dropped on the way out and the veto lifts, so a
    // genuine post-teardown quiesce still fires (no stale entry vetoing forever).
    {
        let mut c = sched.lock();
        c.unwatch_demoted_cancel(tok);
        assert!(
            sched.is_deadlocked(&c),
            "once the demoted fiber has settled the veto must lift"
        );
    }
    let _ = b;
}

/// N4 boundary (pins the veto's remaining half): a cancelled scope with BOTH an undrained parked
/// fiber and a demoted one is still mid-teardown — the parked one is about to be requeued by
/// `cancel_drain` to unwind its `defer`s, so the deadlock must NOT fire (`flag_deadlock` would drop it
/// without `unwind_deferred`). If this flips, the parked-scan is keyed wrong.
#[test]
fn mnsched_cancelled_scope_with_a_parked_and_a_demoted_fiber_is_not_deadlock() {
    let cancel = Arc::new(AtomicBool::new(false));
    let sched = MnSched::new(3, 4, Arc::clone(&cancel), dl_err(), 0);
    let park_core = empty_core();
    let demote_core = empty_core();
    let ptr = core_key(&demote_core);
    sched.seed(vec![mk_fiber(0), mk_fiber(1), mk_fiber(2)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    let d = take_run(&sched);
    sched.park(core_key(&park_core), &park_core, b); // cancel still false → really parks
    {
        let mut c = sched.lock();
        c.running -= 1;
        sched.blocked_native.fetch_add(1, Ordering::Relaxed);
        c.register_demoted(ptr, &demote_core);
    }
    cancel.store(true, Ordering::Relaxed);
    sched.finish(
        a.task_index,
        0,
        TaskOutcome::Fault {
            err: dl_err(),
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    let c = sched.lock();
    assert_eq!(c.parked_n, 1);
    assert!(
        !sched.is_deadlocked(&c),
        "an undrained parked fiber of the cancelled scope still vetoes: it is about to be requeued \
         by `cancel_drain` to unwind its defers"
    );
    let _ = d;
}

/// W7-56 (the veto, BOTH directions): an eager `Executor` job outstanding anywhere in the run is a
/// live sender none of the predicate's counters can see — it runs on the shared pool with no fiber of
/// this sched, so it bumps neither `running`/`runnable` nor `inflight`, and a nursery task parked on
/// the channel that job is about to feed reads as an all-parked quiesce. Without the veto,
/// `ex.submit(feeder)` beside `parallel: spawn waiter()` faulted `deadlock` at ~7 ms — before the job
/// had even run (repro: `executor_job_feeds_a_parked_nursery_task_instead_of_a_false_deadlock`).
///
/// Both directions in ONE test on purpose. The FIRST half is the fence against over-vetoing (this is
/// the `parked-is-not-stuck` family: three predicates in a row have shipped green and faulted healthy
/// programs); the SECOND pins that the veto LIFTS at `finish()`, because a veto that never expires is
/// a permanent silent hang, which is strictly worse than the false fault it replaces.
#[test]
fn mnsched_outstanding_eager_job_vetoes_the_deadlock_until_it_finishes() {
    let sched = mk_sched(1);
    let exec = Arc::new(crate::vm::core::ExecutorCore::default());
    sched.exec_registry.lock().unwrap().push(Arc::clone(&exec));
    let chan = empty_core();
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    // `submit` reserves the slot BEFORE the job is dispatched, so it counts from here.
    let idx = exec.eager.lock().unwrap().reserve();
    sched.park(core_key(&chan), &chan, f);
    {
        let c = sched.lock();
        assert_eq!(c.parked_n, 1, "the nursery's only task is parked");
        assert!(
            !sched.is_deadlocked(&c),
            "an outstanding eager job is an UNCOUNTED sender — it may still feed the parked task, so \
             this is not a deadlock (the same veto `quiesce::QuiesceState::quiesced` applies \
             process-wide via `parties.len() < live`)"
        );
    }
    // The job ends without sending. Nothing can feed the parked task now, so the verdict must fire —
    // `dispatch_eager_job`'s completion closure pokes every live sched so an idle worker re-runs this.
    exec.eager.lock().unwrap().finish(
        idx,
        (0, false),
        TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    let c = sched.lock();
    assert!(
        sched.is_deadlocked(&c),
        "once the job is finished the veto must LIFT — a veto that never expires is a silent hang, \
         which is worse than the false fault it replaces"
    );
}

// ----- gaps.md W7-58 — the nursery OWNER is a counted party of the process-wide verdict -----

/// W7-58, the REFACTOR FENCE. `is_deadlocked` was split into "the W7-56 outstanding-job veto" plus
/// `is_deadlocked_ignoring_jobs` (the old body).
///
/// **It asserts the VERDICT, not the agreement.** With an empty registry `is_deadlocked` literally
/// delegates, so `f(x) == f(x)` is a tautology and would stay green if the move had dropped a gate on
/// the way. So each state below pins the concrete expected answer: drop `any_body_open`,
/// `all_incomplete_awaiting_builder` or `any_cancelled_scope_awaiting_drain` from the moved body and
/// exactly one of these flips. The delegation check rides along, but it is not the fence.
///
/// Walks the states the predicate's own gates discriminate, in order.
#[test]
fn w758_is_deadlocked_ignoring_jobs_matches_is_deadlocked_with_no_executors() {
    let check = |sched: &MnSched, want: bool, what: &str| {
        let c = sched.lock();
        assert_eq!(
            sched.is_deadlocked_ignoring_jobs(&c),
            want,
            "wrong verdict for: {what}"
        );
        assert_eq!(
            sched.is_deadlocked(&c),
            sched.is_deadlocked_ignoring_jobs(&c),
            "with an empty registry the veto is a no-op, so the two must agree: {what}"
        );
    };
    // (a) fresh, nothing seeded — no incomplete scope, so nothing to be stuck about.
    check(&mk_sched(0), false, "no tasks at all");
    // (b) seeded and runnable — work is queued.
    let sched = mk_sched(1);
    sched.seed(vec![mk_fiber(0)]);
    check(&sched, false, "one runnable fiber");
    // (c) running.
    let f = take_run(&sched);
    check(&sched, false, "one running fiber");
    // (d) all parked, nothing runnable/inflight → the genuine deadlock state.
    let chan = empty_core();
    sched.park(core_key(&chan), &chan, f);
    check(&sched, true, "all parked with no possible feeder");
    // (e) `body_open` veto — an eager nursery body may still `inject` a feeder.
    sched.open_body(0);
    check(&sched, false, "body open");
    sched.close_body(0);
    check(&sched, true, "body closed again");
    // (f) `awaiting_builder` veto — needs a SECOND scope (`all_incomplete_awaiting_builder` returns
    // false at `scopes.len() == 1`), i.e. exactly the early-enlisted shape it exists for.
    let two = mk_sched(1);
    let s1 = two.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new());
    two.seed(vec![mk_fiber(0)]);
    let g = take_run(&two);
    two.park(core_key(&chan), &chan, g);
    check(&two, true, "two incomplete scopes, all parked");
    {
        let mut c = two.lock();
        c.scopes[0].awaiting_builder = true;
        c.scopes[s1].awaiting_builder = true;
    }
    check(&two, false, "every incomplete scope awaits its builder");
    // (g) cancelled-scope-awaiting-drain veto — its parked fibers are about to be requeued to unwind.
    sched.trip_scope_cancel(0);
    check(&sched, false, "cancelled scope awaiting drain");
    sched.cancel_drain(0);
}

/// W7-58 — `PartyWait::Nursery` answers the nursery's OWN predicate, live, on every evaluation.
///
/// This is the "a wait predicate that answers a CONSTANT is a bug waiting for a window" fence: the
/// arm must be unsatisfiable ONLY while the sched is genuinely stuck, and satisfiable again the
/// instant ANY of `is_deadlocked_ignoring_jobs`'s vetoes holds. Each veto is exercised in turn,
/// because a snapshot taken at registration would pass the first assertion and fail every later one.
#[test]
fn w758_nursery_party_is_satisfiable_whenever_the_sched_can_still_move() {
    let sched = Arc::new(mk_sched(1));
    let party = crate::vm::quiesce::PartyWait::Nursery(Arc::clone(&sched));
    sched.seed(vec![mk_fiber(0)]);
    assert!(
        party.satisfiable(),
        "a runnable fiber → the nursery can move"
    );
    let f = take_run(&sched);
    assert!(
        party.satisfiable(),
        "a running fiber → the nursery can move"
    );
    let chan = empty_core();
    sched.park(core_key(&chan), &chan, f);
    assert!(
        !party.satisfiable(),
        "the only fiber is parked with nothing to feed it — the owner's wait CANNOT end"
    );
    // …and now each veto in turn puts it back to satisfiable. `runnable`:
    sched.inject(mk_pending_fiber(1), 0);
    assert!(party.satisfiable(), "runnable > 0");
    let g = take_run(&sched);
    assert!(party.satisfiable(), "running > 0");
    sched.park(core_key(&chan), &chan, g);
    assert!(!party.satisfiable(), "back to stuck");
    // `inflight` (a blocking-pool call WILL come back):
    sched.inflight.fetch_add(1, Ordering::Relaxed);
    assert!(party.satisfiable(), "inflight > 0");
    sched.inflight.fetch_sub(1, Ordering::Relaxed);
    // `blocked_native` is deliberately NOT a veto, and that asymmetry with `inflight` is the point: an
    // `inflight` fiber WILL come back from the pool, a demoted one comes back only if a sibling sends,
    // so an all-parked-or-demoted quiesce IS a deadlock. Assert it rather than describe it.
    sched.blocked_native.fetch_add(1, Ordering::Relaxed);
    assert!(
        !party.satisfiable(),
        "a demoted fiber is not a feeder — `blocked_native` must NOT veto the way `inflight` does"
    );
    sched.blocked_native.fetch_sub(1, Ordering::Relaxed);
    // `body_open` (eager nursery still injecting):
    sched.open_body(0);
    assert!(party.satisfiable(), "body_open");
    sched.close_body(0);
    // a cancelled scope mid-teardown:
    sched.trip_scope_cancel(0);
    assert!(party.satisfiable(), "cancelled scope awaiting drain");
    sched.cancel_drain(0);
    // every scope complete → nothing to wait for at all:
    let done = Arc::new(mk_sched(0));
    assert!(
        crate::vm::quiesce::PartyWait::Nursery(done).satisfiable(),
        "a sched with no incomplete scope is not stuck"
    );
    // `awaiting_builder` — needs its own fixture: the veto is defined only for a MULTI-scope sched
    // (`all_incomplete_awaiting_builder` returns false at `scopes.len() == 1`), i.e. exactly the
    // early-enlisted shape it exists for.
    let two = Arc::new(mk_sched(1));
    let s1 = two.register_scope(1, Arc::new(AtomicBool::new(false)), Vec::new());
    let two_party = crate::vm::quiesce::PartyWait::Nursery(Arc::clone(&two));
    two.seed(vec![mk_fiber(0)]);
    let h = take_run(&two);
    two.park(core_key(&chan), &chan, h);
    assert!(
        !two_party.satisfiable(),
        "both scopes incomplete, all parked"
    );
    {
        let mut c = two.lock();
        c.scopes[0].awaiting_builder = true;
        c.scopes[s1].awaiting_builder = true;
    }
    assert!(
        two_party.satisfiable(),
        "an enlisted scope awaiting its builder has a LIVE feeder the counters cannot see"
    );
}

/// W7-58, the COUNT. A nursery owner is counted against the very same `live` the verdict already
/// used — `1 (main) + Σ outstanding` — so a stuck job PLUS a stuck nursery owner is exactly `live`
/// parties and the verdict fires. Before the fix the owner never registered, so `parties.len() == 1 <
/// live == 2` vetoed forever: that is W7-58's whole mechanism, and this is the direct fence for it.
///
/// The last leg is the fence in the OTHER direction (the one that faults live programs): drop the
/// job's party while its slot is still reserved and the count must veto again.
#[test]
fn w758_quiesced_counts_a_nursery_owner_against_live() {
    let state: Arc<crate::vm::quiesce::QuiesceState> = Arc::default();
    let registry: crate::vm::core::ExecRegistry = Arc::default();
    let exec = Arc::new(crate::vm::core::ExecutorCore::default());
    registry.lock().unwrap().push(Arc::clone(&exec));
    let _idx = exec.eager.lock().unwrap().reserve(); // live == 1 (main) + 1 (the job)

    // A stuck nursery: its only fiber is parked with nothing to feed it.
    let sched = Arc::new(mk_sched(1));
    sched.seed(vec![mk_fiber(0)]);
    let f = take_run(&sched);
    let chan = empty_core();
    sched.park(core_key(&chan), &chan, f);

    let job = state.block(crate::vm::quiesce::PartyWait::Recv(empty_core()));
    assert!(
        !state.quiesced(&registry),
        "the owner is not registered yet — 1 party < live 2, so the verdict must decline (this IS \
         the W7-58 hang)"
    );
    let owner = state.block(crate::vm::quiesce::PartyWait::Nursery(Arc::clone(&sched)));
    assert!(
        state.quiesced(&registry),
        "owner + job == live, both unsatisfiable → the run really is stuck"
    );
    // The nursery becomes able to move → the owner's wait is satisfiable → veto.
    sched.inject(mk_pending_fiber(1), 0);
    assert!(
        !state.quiesced(&registry),
        "a runnable fiber makes the owner's wait satisfiable, which must veto the verdict"
    );
    let g = take_run(&sched);
    sched.park(core_key(&chan), &chan, g);
    assert!(state.quiesced(&registry), "stuck again");
    // The other direction: fewer parties than `live` must always veto.
    drop(job);
    assert!(
        !state.quiesced(&registry),
        "1 party < live 2 — an unregistered job is a RUNNING job, which may yet send"
    );
    drop(owner);
}

/// W7-58 — the GATE. A worker shell must never register a nursery party: it is not in `live`, so
/// registering it would let `parties.len()` exceed `live`, which is the one error direction that
/// faults a live program (`quiesce`'s error-direction table).
///
/// Also pins the deliberate WIDENING versus `is_counted_party`: a builder holding an early-enlisted
/// sched (`mn_enlist_sched.is_some()`) is NOT a counted party by `owns_os_thread`, yet it is exactly
/// `main`, i.e. the `1 +` in `live` — so it MUST register.
#[test]
fn w758_only_a_thread_with_no_scheduler_under_it_registers_a_nursery_party() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let sched = Arc::new(mk_sched(0));
    assert!(
        vm.nursery_party_guard(&sched).is_some(),
        "top-level main owns its OS thread — it is the `1 +` in `live`"
    );
    vm.mn_enlist_sched = Some(Arc::clone(&sched));
    assert!(
        vm.nursery_party_guard(&sched).is_some(),
        "an early-enlisted builder is still main; `is_counted_party` would wrongly exclude it"
    );
    vm.mn_enlist_sched = None;
    vm.mn = Some(Arc::clone(&sched));
    assert!(
        vm.nursery_party_guard(&sched).is_none(),
        "a worker SHELL is not in `live` — registering it would let parties exceed live"
    );
    vm.mn = None;
    vm.scheduler_stack.push(crate::vm::Nursery {
        parent: FiberCtx::default(),
        children: Vec::new(),
        ready: Default::default(),
        blocked_on: Default::default(),
    });
    assert!(
        vm.nursery_party_guard(&sched).is_none(),
        "a cooperative nursery level under this VM means it does not own the thread"
    );
}

/// D2b: `finish` records a task's outcome in its slot, drops it from `running`, and flips
/// `terminate` once every task is done.
#[test]
fn mnsched_finish_writes_slot_and_terminates_at_total() {
    let sched = mk_sched(2);
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    sched.finish(
        a.task_index,
        0,
        TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    {
        let c = sched.lock();
        assert_eq!(c.scopes[0].done, 1);
        assert!(!c.terminate);
    }
    sched.finish(
        b.task_index,
        0,
        TaskOutcome::Cancelled {
            out: Vec::new(),
            stderr: Vec::new(),
        },
    );
    {
        let c = sched.lock();
        assert_eq!(c.scopes[0].done, 2);
        assert!(c.terminate);
    }
    assert!(matches!(sched.take_runnable(0, 1, 0), Take::Stop));
}

/// D2b/U5 (mechanics half): `cancel_drain` moves every parked fiber back onto the run queue so a
/// worker resumes it and it observes the cancel flag on its next dispatch back-edge.
#[test]
fn mnsched_cancel_drain_requeues_parked() {
    let sched = mk_sched(2);
    let c1 = empty_core();
    let c2 = empty_core();
    sched.seed(vec![mk_fiber(0), mk_fiber(1)]);
    let a = take_run(&sched);
    let b = take_run(&sched);
    sched.park(core_key(&c1), &c1, a);
    sched.park(core_key(&c2), &c2, b);
    sched.cancel_drain(0);
    let c = sched.lock();
    assert_eq!(c.parked_n, 0);
    assert_eq!(c.global.len(), 2);
}

/// D2b/G1 (headline): #fibers ≫ #threads. 64 consumer fibers each block on an empty channel while
/// 64 producer fibers send. On the legacy "one OS thread per task, block the thread on `recv`"
/// engine the blocked consumers pin every pool thread and the queued producers never run
/// (starvation/hang). Under the M:N engine the consumers PARK (freeing their workers), the
/// producers run and wake them, and the sum completes.
#[test]
fn mn_many_blocked_consumers_complete_without_starving() {
    let src = "\
fn producer(ch: Channel[int], i: int):
    ch.send(i)
fn consumer(ch: Channel[int], acc: Shared[int]):
    v := ch.recv()
    acc.update(fn(x): x + v)
fn main():
    ch := Channel[int]()
    acc := Shared(0)
    parallel:
        for i in 0..64:
            spawn consumer(ch, acc)
        for i in 0..64:
            spawn producer(ch, i)
    print(acc.get())
main()
";
    assert_eq!(run_capture_parallel(src).unwrap(), "2016\n"); // sum 0..64
}

/// D2b: 1000 producer fibers + 1 consumer recv-looping 1000 times, multiplexed over the
/// core-sized pool — 1001 fibers on ~N threads, no thread-per-fiber. The consumer parks between
/// sends and resumes via the rewound-ip `recv` replay.
#[test]
fn mn_thousand_fiber_pipeline_completes() {
    let src = "\
fn producer(ch: Channel[int], i: int):
    ch.send(i)
fn consumer(ch: Channel[int], acc: Shared[int]):
    total := 0
    for _ in 0..1000:
        total = total + ch.recv()
    acc.update(fn(x): x + total)
fn main():
    ch := Channel[int]()
    acc := Shared(0)
    parallel:
        spawn consumer(ch, acc)
        for i in 0..1000:
            spawn producer(ch, i)
    print(acc.get())
main()
";
    assert_eq!(run_capture_parallel(src).unwrap(), "499500\n"); // sum 0..1000
}

/// D2a: a cooperative fiber carries NO heap (`heap: None`) — every cooperative fiber aliases the
/// single `Vm::heap` (decision A, share-by-ref). `swap_ctx` must leave `self.heap` untouched, so
/// the cooperative engine stays byte-identical.
#[test]
fn swap_ctx_leaves_heap_untouched_for_cooperative_fiber() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let hv = vm.heap.alloc(Obj::Str("vm-obj".into()));
    let mut ctx = FiberCtx::default();
    assert!(
        ctx.heap.is_none(),
        "a default (cooperative) fiber carries no heap"
    );
    vm.swap_ctx(&mut ctx);
    assert!(matches!(vm.heap.get(hv), Obj::Str(s) if &s[..] == "vm-obj"));
    assert!(
        ctx.heap.is_none(),
        "swap must not give a cooperative fiber a heap"
    );
}

/// D2a GC canary: a `collect` while an M:N fiber is swapped in must trace the FIBER's heap (via
/// the swapped-in operand stack) and must NOT touch the parked host heap. After the fiber parks
/// back out, the host heap and its stack-rooted object are intact. This is the one path the
/// swap-with-heap logic adds that the goldens can't reach (no runtime site parks a fiber until
/// D2b), so it guards the moved-heap rooting directly.
#[test]
fn collect_under_swapped_in_fiber_heap_preserves_parked_host_object() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.parallel = true;
    let hv = vm.heap.alloc(Obj::Str("vm-obj".into()));
    vm.push(Value::obj(hv)); // keep the host object stack-rooted

    let mut fiber_heap = Heap::new();
    let hf = fiber_heap.alloc(Obj::Str("fiber-obj".into()));
    let mut ctx = FiberCtx {
        heap: Some(fiber_heap),
        stack: vec![Value::obj(hf)], // the fiber's own stack roots its object
        ..FiberCtx::default()
    };

    // Schedule in: host heap + stack park into the ctx; self.{heap,stack} are the fiber's.
    vm.swap_ctx(&mut ctx);
    vm.collect(); // roots self.stack (the fiber's) → hf survives in the fiber heap
    assert!(matches!(vm.heap.get(hf), Obj::Str(s) if &s[..] == "fiber-obj"));

    // Park back out: the untouched host heap + its object are restored.
    vm.swap_ctx(&mut ctx);
    assert!(matches!(vm.heap.get(hv), Obj::Str(s) if &s[..] == "vm-obj"));
    assert_eq!(vm.pop(), Value::obj(hv));
}

/// D2a share-nothing lock: a `collect` while an M:N fiber is swapped in must leave the parked
/// HOST heap fully quiescent — not even sweeping its UNROOTED garbage. An object rooted by
/// nothing in any context would be swept by a normal host-heap collect; here the collect runs on
/// the fiber heap, so the host heap is never traced and the garbage survives. This proves the
/// parked heap is untouched (the positive canary only shows a *stack-rooted* host object
/// survives; this shows the collect didn't run on the host heap at all) — the guarantee D2b
/// relies on when parking fibers across worker threads.
#[test]
fn collect_under_swapped_in_fiber_heap_leaves_parked_host_heap_quiescent() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.parallel = true;
    // Rooted by nothing — a host-heap collect would sweep it.
    let garbage = vm.heap.alloc(Obj::Str("host-garbage".into()));

    let mut fiber_heap = Heap::new();
    let hf = fiber_heap.alloc(Obj::Str("fiber-obj".into()));
    let mut ctx = FiberCtx {
        heap: Some(fiber_heap),
        stack: vec![Value::obj(hf)],
        ..FiberCtx::default()
    };

    vm.swap_ctx(&mut ctx);
    vm.collect(); // runs on the fiber heap only — the parked host heap is not traced
    vm.swap_ctx(&mut ctx);

    // The unrooted host object is still alive: collect never ran on the host heap. (Were it
    // swept, `heap.get` would panic on the dangling GcRef.)
    assert!(matches!(vm.heap.get(garbage), Obj::Str(s) if &s[..] == "host-garbage"));
}

/// B3.1: `shut` lives in the shared core, so a `from_wire`'d alias observes a shutdown done through
/// the original handle — `submit` on the alias then fails with the byte-identical message.
#[test]
fn executor_core_shut_is_shared_across_handles() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let h1 = vm
        .heap
        .alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
    let w = vm.to_wire(Value::obj(h1)).unwrap();
    let Some(h2) = vm.from_wire(w).as_obj() else {
        panic!("expected handle")
    };
    let sp = Span::RUNTIME;
    vm.executor_method(h1, "shutdown", &[], sp).unwrap();
    let dummy = vm.heap.alloc(Obj::Str("task".into()));
    let err = vm
        .executor_method(h2, "submit", &[Value::obj(dummy)], sp)
        .unwrap_err();
    assert_eq!(
        err.message,
        "submit on a shut-down Executor (it no longer accepts work)"
    );
}

/// B3.1: `display` of a `Shared` box renders its contents through `display_wire` (a boxed `str`
/// renders from its owned bytes — B3.3a), since `display` is `&self` and can't `from_wire`.
#[test]
fn display_shared_renders_contents() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let s = vm.heap.alloc(Obj::Str("hi".into()));
    let boxed = vm.to_wire(Value::obj(s)).unwrap();
    let sh = vm.heap.alloc(Obj::Shared(Arc::new(SharedCore {
        v: Mutex::new(boxed),
        ..Default::default()
    })));
    // The payload sits INSIDE `Shared(…)`, i.e. nested — so it renders as its `repr` (W7-25),
    // matching `print(s.get())` on the same box.
    assert_eq!(vm.display(Value::obj(sh)), "Shared('hi')");
}

// ----- B3.2: isolated worker-VM construction (no threads) -----

/// Build a one-proto program + a parent `Vm`, plus a zero-arg closure over proto 0 with a dummy
/// home module (the test protos never read globals). Mirrors how `do_spawn_block` shapes a task.
fn worker_fixture(code: Vec<Op>) -> (Vm, PendingCall) {
    let sp = Span::RUNTIME;
    let proto = op::Proto {
        name: "task".into(),
        arity: 0,
        n_slots: 0,
        lines: vec![sp; code.len()],
        code,
        has_implicit_nursery: false,
        is_generator: false,
        is_test: false,
        capture_names: Vec::new(),
    };
    let program = Program {
        protos: vec![proto],
        ..empty_program()
    };
    let mut vm = Vm::new(Arc::new(program));
    let home = vm.heap.alloc(Obj::Module(Box::new(ModuleData {
        name: "<test>".into(),
        slots: Vec::new(),
        index: Default::default(),
    })));
    let clo = vm.heap.alloc(Obj::Closure {
        proto: 0,
        captured: Default::default(),
        home,
    });
    (
        vm,
        PendingCall::Call {
            callee: Value::obj(clo),
            args: Vec::new(),
            span: sp,
        },
    )
}

/// The worker allocates into its OWN heap, not the parent's: a task that builds a fresh list runs
/// to completion in the worker, the parent heap's live-object count is unchanged, and the result
/// crosses back as a `WireValue` that reconstructs (in the parent) to the expected value.
#[test]
fn worker_runs_in_distinct_heap() {
    // () -> [1, 2]
    let (mut vm, task) = worker_fixture(vec![
        Op::ConstInt(1),
        Op::ConstInt(2),
        Op::NewList(2),
        Op::Return,
    ]);
    let before = vm.heap.live();
    let res = vm.run_task_isolated(task).expect("isolated task runs");
    assert_eq!(
        vm.heap.live(),
        before,
        "worker must not allocate into the parent heap"
    );
    let got = vm.from_wire(res.value);
    let want = Value::obj(vm.heap.alloc(Obj::List(vec![Value::int(1), Value::int(2)])));
    assert!(
        vm.values_equal(got, want),
        "result must round-trip back to [1, 2]"
    );
}

/// B3.3-threads: a worker inherits the parent's read-only host state (process args + env) so a
/// `--parallel` task reading `std.os.args` / an env var isn't silently inert — AND the one shared
/// stdin, so a task's `read_line` reads the real stream instead of a false EOF.
#[test]
fn worker_inherits_host_args_and_env() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.host.args = vec!["prog".into(), "--flag".into()];
    vm.host
        .env
        .lock()
        .unwrap()
        .insert("KEY".into(), "val".into());
    vm.host.stdin = crate::native::Stdin::Real;
    let worker = vm.spawn_worker();
    assert_eq!(
        worker.host.args,
        vec!["prog".to_string(), "--flag".to_string()]
    );
    assert_eq!(
        worker.host.env.lock().unwrap().get("KEY").cloned(),
        Some("val".to_string())
    );
    assert!(
        matches!(worker.host.stdin, crate::native::Stdin::Real),
        "the one stdin source must be shared with workers (no false EOF in a task)"
    );
}

/// The worker's stdin is the SAME source, not a copy: a line a worker consumes is gone for the
/// parent. A naive `#[derive(Clone)]` over a by-value queue would hand every worker its own copy of
/// every line — delivering each line N times, which is worse than the false EOF this replaced.
#[test]
fn worker_shares_the_one_stdin_source() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    vm.host.stdin = crate::native::Stdin::lines(["a".to_string()]);
    let mut worker = vm.spawn_worker();
    assert_eq!(worker.host.stdin.read_line(), Ok(Some("a".to_string())));
    assert_eq!(
        vm.host.stdin.read_line(),
        Ok(None),
        "the line the worker consumed must be gone for the parent (shared, not cloned)"
    );
}

/// A worker's stdout is captured in ITS `out` and returned on the `WorkerResult` (decision F:
/// buffer-per-worker), and never leaks into the parent's `out`. The return value crosses back too.
#[test]
fn worker_returns_value_and_out() {
    // () -> { print("hi from worker"); 7 }
    let (mut vm, task) = worker_fixture(vec![
        Op::ConstStr("hi from worker".into()),
        Op::CallPrint(1),
        Op::Pop,
        Op::ConstInt(7),
        Op::Return,
    ]);
    let res = vm.run_task_isolated(task).expect("isolated task runs");
    assert_eq!(
        res.out, b"hi from worker\n",
        "worker stdout returns on the result"
    );
    assert_eq!(
        res.stderr, b"",
        "stderr is captured separately and empty here"
    );
    assert_eq!(
        vm.from_wire(res.value),
        Value::int(7),
        "return value crosses back"
    );
    assert_eq!(
        vm.out, b"",
        "worker output must not leak into the parent's stdout"
    );
}

/// A worker shares the compiled program by `Arc` (read-only), never copying it: `spawn_worker`
/// bumps the strong count and points at the SAME allocation, and drops its clone when finished.
#[test]
fn worker_shares_program_arc() {
    let program = Arc::new(empty_program());
    let vm = Vm::new(Arc::clone(&program)); // program + vm = 2 refs
    assert_eq!(Arc::strong_count(&program), 2);
    let worker = vm.spawn_worker();
    assert_eq!(
        Arc::strong_count(&program),
        3,
        "worker shares the program (no copy)"
    );
    assert!(
        Arc::ptr_eq(&program, &worker.program),
        "same Program allocation, not a clone"
    );
    drop(worker);
    assert_eq!(
        Arc::strong_count(&program),
        2,
        "worker releases its program ref on drop"
    );
}

/// B3.3a: a `str` return value crosses the worker boundary **by value** — the worker serializes its
/// own-heap `str` to owned bytes, and the parent reconstructs a fresh `str` from them (no dangling
/// `GcRef`). Replaces B3.2's reject-the-str fault now that `str` is sendable by value.
#[test]
fn worker_crosses_str_by_value() {
    // () -> "oops"
    let (mut vm, task) = worker_fixture(vec![Op::ConstStr("oops".into()), Op::Return]);
    let res = vm
        .run_task_isolated(task)
        .expect("a str result now crosses by value");
    let got = vm.from_wire(res.value);
    let want = Value::obj(vm.heap.alloc(Obj::Str("oops".into())));
    assert!(
        vm.values_equal(got, want),
        "str result round-trips to \"oops\""
    );
}

// ----- B3.3c: read-only `home` snapshot (worker module-graph reconstruction) -----

/// Compile + run a single-module program, returning the populated parent `Vm` (its `module_objs[0]`
/// holds the top-level globals). Mirrors the live load path so a worker reconstructs a real graph.
fn ran_standalone(src: &str) -> Vm {
    let tokens = lexer::tokenize(src).expect("tokenize");
    let module = parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    let mut vm = Vm::new(Arc::new(program));
    vm.run().expect("run");
    vm
}

/// Compile + run a multi-file graph from its entry path, returning the populated parent `Vm`
/// (all imported modules present in `module_objs`).
fn ran_graph(entry: &std::path::Path) -> Vm {
    let graph = crate::resolver::build_graph(entry).expect("graph");
    let program = crate::compiler::compile_graph(&graph).expect("compile");
    let mut vm = Vm::new(Arc::new(program));
    vm.run().expect("run");
    vm
}

/// Look up a top-level global in the entry module (modules run deps-first, entry last).
fn entry_global(vm: &Vm, name: &str) -> Value {
    let m = *vm.module_objs.last().expect("at least one module");
    vm.module_global(m, name)
        .unwrap_or_else(|| panic!("no global '{name}'"))
}

fn sp() -> Span {
    Span::RUNTIME
}

/// A spawned task reads a module-level constant — needs the read-only `home` snapshot (B3.3c):
/// the worker's `home` is a reconstruction of the parent module's globals, not a fresh-empty one.
#[test]
fn worker_reads_module_global() {
    let mut vm = ran_standalone("answer := 42\nfn get_answer() -> int:\n    return answer\n");
    let task = PendingCall::Call {
        callee: entry_global(&vm, "get_answer"),
        args: Vec::new(),
        span: sp(),
    };
    let res = vm
        .run_task_isolated(task)
        .expect("task reads a module global in its worker");
    assert_eq!(vm.from_wire(res.value), Value::int(42));
}

#[test]
fn worker_reads_last_of_many_globals() {
    // M19 Phase 2b: three globals defined before the fn, and the task reads the LAST one. A
    // slot scramble between the parent's compiled slots and the worker's faulted-in slots would
    // surface here as the wrong global's value (e.g. reading `a` or `b` instead of `c`).
    let mut vm = ran_standalone("a := 1\nb := 2\nc := 99\nfn get_c() -> int:\n    return c\n");
    let task = PendingCall::Call {
        callee: entry_global(&vm, "get_c"),
        args: Vec::new(),
        span: sp(),
    };
    let res = vm
        .run_task_isolated(task)
        .expect("task reads the last module global in its worker");
    assert_eq!(vm.from_wire(res.value), Value::int(99));
}

#[test]
fn globals_compile_to_stable_slots() {
    // M19 Phase 2b: top-level bindings get compile-time slots. Collection order is fns first,
    // then lets — so `read_b`=0, `a`=1, `b`=2 — and a fn body reads a global by its slot
    // (`GetGlobalSlot`), never by name.
    let tokens =
        lexer::tokenize("a := 1\nb := 2\nfn read_b() -> int:\n    return b\n").expect("tok");
    let module = parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    assert_eq!(
        program.modules[0].global_slots,
        vec!["read_b".to_string(), "a".to_string(), "b".to_string()]
    );
    let read_b = program
        .protos
        .iter()
        .find(|p| p.name == "read_b")
        .expect("fn proto");
    assert!(
        read_b
            .code
            .iter()
            .any(|op| matches!(op, Op::GetGlobalSlot(2))),
        "read_b should load global slot 2 (`b`): {:?}",
        read_b.code
    );
    let top = &program.protos[program.modules[0].toplevel];
    let defines: Vec<u32> = top
        .code
        .iter()
        .filter_map(|op| {
            if let Op::DefineGlobalSlot(s) = op {
                Some(*s)
            } else {
                None
            }
        })
        .collect();
    // toplevel defines the fn (slot 0 via hoist) and both lets (slots 1, 2).
    assert!(
        defines.contains(&0) && defines.contains(&1) && defines.contains(&2),
        "toplevel defines slots 0, 1, 2: {:?}",
        top.code
    );
}

/// A spawned task calls another top-level fn in its module — sibling resolution via the
/// reconstructed `home` globals (the sibling `Func` is re-allocated over the worker's home).
#[test]
fn worker_calls_sibling_free_fn() {
    let mut vm = ran_standalone(
        "fn helper() -> int:\n    return 7\nfn task() -> int:\n    return helper() + 1\n",
    );
    let task = PendingCall::Call {
        callee: entry_global(&vm, "task"),
        args: Vec::new(),
        span: sp(),
    };
    let res = vm
        .run_task_isolated(task)
        .expect("task calls a sibling fn in its worker");
    assert_eq!(vm.from_wire(res.value), Value::int(8));
}

/// A spawned task calls a function from an IMPORTED module — proves cross-module `module_objs`
/// reconstruction (the `text` import alias maps to the worker's std.string module obj, whose own
/// globals are reconstructed too).
#[test]
fn worker_calls_imported_fn() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static C: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "chezzi_b33c_{}_{}",
        std::process::id(),
        C.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import std.string as text\nfn task() -> str:\n    return text.repeat(\"ab\", 2)\n",
    )
    .unwrap();
    let mut vm = ran_graph(&entry);
    let _ = std::fs::remove_dir_all(&dir);
    let task = PendingCall::Call {
        callee: entry_global(&vm, "task"),
        args: Vec::new(),
        span: sp(),
    };
    let res = vm
        .run_task_isolated(task)
        .expect("task calls an imported fn in its worker");
    let got = vm.from_wire(res.value);
    let want = Value::obj(vm.heap.alloc(Obj::Str("abab".into())));
    assert!(vm.values_equal(got, want), "imported repeat returns abab");
}

// ----- B3.3d: method tasks (`spawn recv.m()`) -----

/// A method task on a primitive receiver — `"hello".len()` dispatches in the worker (B3.3d,
/// replaces the B3.2 reject). Core-type methods need no module graph, but exercise the new path.
#[test]
fn worker_runs_method_task() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let recv = vm.heap.alloc(Obj::Str("hello".into()));
    let task = PendingCall::Method {
        recv: Value::obj(recv),
        name: "len".into(),
        args: Vec::new(),
        span: sp(),
    };
    let res = vm
        .run_task_isolated(task)
        .expect("method task now runs in a worker");
    assert_eq!(vm.from_wire(res.value), Value::int(5));
}

/// A struct method resolved through reconstructed `module_objs` — and its body **reads a module
/// global** (`scale`), so dispatch must resolve through the rebuilt home *contents*, not merely
/// index an in-bounds placeholder. `(3 + 4) * 10 == 70`.
#[test]
fn worker_method_on_struct() {
    let mut vm = ran_standalone(
        "scale := 10\nstruct Point:\n    x: int\n    y: int\n    fn weighted(self) -> int:\n        return (self.x + self.y) * scale\np := Point(3, 4)\n",
    );
    let task = PendingCall::Method {
        recv: entry_global(&vm, "p"),
        name: "weighted".into(),
        args: Vec::new(),
        span: sp(),
    };
    let res = vm
        .run_task_isolated(task)
        .expect("struct method task dispatches in its worker");
    assert_eq!(vm.from_wire(res.value), Value::int(70));
}

/// Cross-heap safety: a module global that is a **container of callables** (`[fn …]`) must have its
/// nested `Func` *rebuilt* in the worker heap, not carried across as a by-reference `Handle` (a
/// parent-heap `GcRef`). A task that calls through the list exercises the reconstructed funcs; a
/// smuggled `GcRef` would read a wrong/out-of-range worker slot. `bump(20) == 21`.
#[test]
fn worker_calls_through_global_fn_container() {
    let mut vm = ran_standalone(
        "fn bump(n: int) -> int:\n    return n + 1\nhandlers := [bump]\nfn task() -> int:\n    return handlers[0](20)\n",
    );
    let task = PendingCall::Call {
        callee: entry_global(&vm, "task"),
        args: Vec::new(),
        span: sp(),
    };
    let res = vm
        .run_task_isolated(task)
        .expect("task calls a fn from a global container in its worker");
    assert_eq!(vm.from_wire(res.value), Value::int(21));
}

/// The module-graph reconstruction must be GC-safe: with `gc_stress` on (collect before every
/// instruction), a task that reads a **heap-typed** module global (a list) through the rebuilt home
/// must still round-trip — the reconstructed globals stay rooted via `module_objs`.
#[test]
fn worker_reconstruction_survives_gc_stress() {
    let mut vm = ran_standalone(
        "data := [1, 2, 3]\nfn total() -> int:\n    s := 0\n    for x in data:\n        s += x\n    return s\n",
    );
    vm.gc_stress = true;
    let task = PendingCall::Call {
        callee: entry_global(&vm, "total"),
        args: Vec::new(),
        span: sp(),
    };
    let res = vm
        .run_task_isolated(task)
        .expect("reconstruction survives GC stress");
    assert_eq!(vm.from_wire(res.value), Value::int(6));
}

// ----- arithmetic -----

#[test]
fn int_div_truncates() {
    assert_eq!(run("print(7 / 2)"), "3\n");
    assert_eq!(run("print(-7 / 2)"), "-3\n"); // Rust trunc-toward-zero, matching interp
}

#[test]
fn int_overflow_is_error_not_wrap() {
    // A wrapping VM would print a negative number; we must error like the interpreter.
    assert!(run_err("print(9223372036854775807 + 1)").contains("integer overflow in Add"));
}

#[test]
fn int_min_neg_and_div_overflow() {
    // The two other unrepresentable results: -i64::MIN and i64::MIN / -1. Both must error.
    let neg = "fn main():\n    x := -9223372036854775807 - 1\n    print(-x)\nmain()\n";
    assert!(run_err(neg).contains("integer overflow"));
    let div = "fn main():\n    x := -9223372036854775807 - 1\n    print(x / -1)\nmain()\n";
    assert!(run_err(div).contains("integer overflow"));
}

#[test]
fn float_promotion_when_either_side_float() {
    assert_eq!(run("print(1 + 2.0)"), "3.0\n");
    assert_eq!(run("print(7.0 / 2.0)"), "3.5\n");
    assert_eq!(run("print(7 / 2.0)"), "3.5\n");
}

#[test]
fn division_and_modulo_by_zero_error() {
    // INTEGER division/modulo by zero still faults (unchanged).
    assert_eq!(run_err("print(1 / 0)"), "division by zero");
    assert_eq!(run_err("print(1 % 0)"), "modulo by zero");
    // Float by zero is IEEE-754 — never faults; produces inf/-inf/NaN.
    assert_eq!(run("print(1.0 / 0.0)"), "inf\n");
    assert_eq!(run("print(5.0 % 0.0)"), "NaN\n");
}

#[test]
fn float_div_mod_by_zero_is_ieee() {
    assert_eq!(run("print(1.0 / 0.0)"), "inf\n");
    assert_eq!(run("print(-1.0 / 0.0)"), "-inf\n");
    assert_eq!(run("print(0.0 / 0.0)"), "NaN\n");
    assert_eq!(run("print(5.0 % 0.0)"), "NaN\n");
}

#[test]
fn int_div_mod_still_fault() {
    // Regression tripwire: INTEGER arithmetic stays total-faulting.
    assert_eq!(run_err("print(1 / 0)"), "division by zero");
    assert_eq!(run_err("print(1 % 0)"), "modulo by zero");
}

#[test]
fn string_concatenation() {
    assert_eq!(run(r#"print("a" + "b" + "c")"#), "abc\n");
}

#[test]
fn comparison_and_equality_across_numeric_types() {
    assert_eq!(run("print(1 < 2.0)"), "true\n");
    assert_eq!(run("print(2 == 2.0)"), "true\n");
    assert_eq!(run("print(2 != 3)"), "true\n");
    assert_eq!(run(r#"print("a" < "b")"#), "true\n");
    // Cross-type equality is false, never an error.
    assert_eq!(run(r#"print(1 == "1")"#), "false\n");
}

#[test]
fn arithmetic_type_error_message() {
    assert!(run_err(r#"print(1 + "x")"#).contains("cannot apply Add to int and str"));
}

// ----- M19 superinstructions: operands are LOCALS (inside `fn`), so the fused ops actually
// execute (top-level `:=` is a global → `GetGlobal`, never fused). -----

#[test]
fn superinstruction_loop_sum_correct() {
    // `i < 5` → BinLocalConst{Lt}; `total += i` → BinLocalLocal{Add}+SetLocal; `i += 1` → IncLocal.
    let src = "fn main():\n    total := 0\n    i := 0\n    while i < 5:\n        total += i\n        i += 1\n    print(total)\nmain()";
    assert_eq!(run(src), "10\n");
}

#[test]
fn superinstruction_div_mod_by_zero_via_locals() {
    // BinLocalLocal fast path must raise the same message as `arith`.
    assert_eq!(
        run_err("fn main():\n    x := 1\n    y := 0\n    print(x / y)\nmain()"),
        "division by zero"
    );
    assert_eq!(
        run_err("fn main():\n    x := 1\n    y := 0\n    print(x % y)\nmain()"),
        "modulo by zero"
    );
}

#[test]
fn superinstruction_overflow_via_inc_and_mul() {
    // IncLocal overflow.
    assert!(
        run_err("fn main():\n    i := 9223372036854775807\n    i += 1\n    print(i)\nmain()")
            .contains("integer overflow in Add")
    );
    // BinLocalConst Mul overflow.
    assert!(
        run_err("fn main():\n    x := 9223372036854775807\n    print(x * 2)\nmain()")
            .contains("integer overflow in Mul")
    );
}

#[test]
fn superinstructions_run_under_parallel_engine() {
    // Drives BinLocalConst / BinLocalLocal / IncLocal through the M:N engine (reduction path).
    let out = run_capture_parallel("fn main():\n    total := 0\n    i := 0\n    while i < 1000:\n        total += i\n        i += 1\n    print(total)\nmain()").unwrap();
    assert_eq!(out, "499500\n");
}

// ----- and / or short-circuit -----

#[test]
fn and_short_circuits_rhs() {
    // If `and` did not short-circuit, the `1/0` would raise a div-by-zero error.
    assert_eq!(run("print(false and (1 / 0 == 0))"), "false\n");
}

#[test]
fn or_short_circuits_rhs() {
    assert_eq!(run("print(true or (1 / 0 == 0))"), "true\n");
}

#[test]
fn logical_operand_must_be_bool() {
    assert_eq!(run_err("print(1 and true)"), "expected bool, found int");
}

// ----- display formatting -----

#[test]
fn float_display_keeps_one_decimal_for_integral() {
    assert_eq!(run("print(5.0)"), "5.0\n");
    assert_eq!(run("print(5.5)"), "5.5\n");
    assert_eq!(run("print(2.5 * 2.0)"), "5.0\n");
}

#[test]
fn list_display() {
    assert_eq!(run("print([1, 2, 3])"), "[1, 2, 3]\n");
    assert_eq!(run("print([])"), "[]\n");
    assert_eq!(run(r#"print(["a", "b"])"#), "['a', 'b']\n");
}

#[test]
fn struct_display_in_declaration_order() {
    let src = "\
struct Point:
    x: int
    y: int
print(Point(3, 4))";
    assert_eq!(run(src), "Point(x=3, y=4)\n");
}

#[test]
fn enum_display_nullary_and_payload() {
    let src = "\
enum Shape:
    Circle(int)
    Dot
print(Shape.Circle(2))
print(Shape.Dot)";
    assert_eq!(run(src), "Circle(2)\nDot\n");
}

#[test]
fn print_joins_args_with_space() {
    assert_eq!(run(r#"print("a", 1, true)"#), "a 1 true\n");
}

#[test]
fn print_end_suppresses_newline() {
    assert_eq!(run("print(\"a\", end=\"\")\n"), "a");
}

#[test]
fn print_sep_joins_args() {
    assert_eq!(run("print(\"a\", \"b\", sep=\"-\")\n"), "a-b\n");
}

#[test]
fn print_sep_and_end_together() {
    assert_eq!(run("print(\"a\", \"b\", sep=\"-\", end=\"!\")\n"), "a-b!");
}

#[test]
fn print_default_unchanged_with_no_kwargs() {
    // The no-kwarg path must stay byte-identical: space-joined, newline-terminated.
    assert_eq!(run("print(\"a\", \"b\")\n"), "a b\n");
    assert_eq!(run("print(\"a\")\n"), "a\n");
}

#[test]
fn print_end_with_runtime_str_expr() {
    // `end` can be a runtime str expression, not just a literal.
    assert_eq!(run("e := \"?\"\nprint(\"a\", end=e)\n"), "a?");
}

#[test]
fn print_only_end_keeps_default_space_sep() {
    assert_eq!(run("print(\"a\", \"b\", end=\"\")\n"), "a b");
}

// ----- functions / control flow -----

#[test]
fn nested_calls_and_return() {
    let src = "\
fn add(a: int, b: int) -> int:
    return a + b
fn main():
    print(add(add(1, 2), 3))
main()";
    assert_eq!(run(src), "6\n");
}

#[test]
fn forward_reference_between_top_level_fns() {
    // `main` is defined before `helper`; hoisting must make the forward ref resolve.
    let src = "\
fn main():
    print(helper(21))
fn helper(n: int) -> int:
    return n * 2
main()";
    assert_eq!(run(src), "42\n");
}

#[test]
fn infinite_recursion_hits_depth_limit() {
    let src = "\
fn loop(n: int) -> int:
    return loop(n + 1)
fn main():
    print(loop(0))
main()";
    assert!(run_err(src).contains("maximum call depth"));
}

/// M10-G1: a self-referential `Stringable` `str` must hit the depth guard, not loop forever.
#[test]
fn self_referential_stringable_hits_depth_limit() {
    let src = "struct Loop:\n    n: int\n    fn str(self) -> str:\n        return str(self)\nprint(Loop(1))\n";
    assert!(run_err(src).contains("maximum call depth"));
}

#[test]
fn if_elif_else() {
    let src = "\
fn classify(n: int) -> str:
    if n < 0:
        return \"neg\"
    elif n == 0:
        return \"zero\"
    else:
        return \"pos\"
fn main():
    print(classify(-1))
    print(classify(0))
    print(classify(5))
main()";
    assert_eq!(run(src), "neg\nzero\npos\n");
}

#[test]
fn while_loop_with_compound_assign() {
    let src = "\
fn main():
    i := 0
    total := 0
    while i < 5:
        total += i
        i += 1
    print(total)
main()";
    assert_eq!(run(src), "10\n");
}

#[test]
fn unary_neg_and_not() {
    assert_eq!(run("print(-5)"), "-5\n");
    assert_eq!(run("print(not true)"), "false\n");
    assert_eq!(run_err("print(-true)"), "cannot apply Neg to bool");
}

// ----- closures -----

#[test]
fn closure_shares_captured_binding() {
    // Uniform by-reference capture: the closure shares the binding of `n`, so a reassignment made
    // after the closure was created IS visible when the closure later runs (`n = 20` → `x + 20`).
    // (Under the old value-semantics rule this snapshotted `n = 10` and printed `15`.)
    let src = "\
fn make():
    n := 10
    f := fn(x: int) -> int: x + n
    n = 20
    return f
fn main():
    g := make()
    print(g(5))
main()";
    assert_eq!(run(src), "25\n");
}

#[test]
fn closure_captures_distinct_environments() {
    let src = "\
fn adder(n: int):
    return fn(x: int) -> int: x + n
fn main():
    add10 := adder(10)
    add100 := adder(100)
    print(add10(1))
    print(add100(1))
main()";
    assert_eq!(run(src), "11\n101\n");
}

// ----- ? operator -----

#[test]
fn try_unwraps_ok() {
    let src = "\
fn safe_div(a: int, b: int) -> Result[int]:
    if b == 0:
        return Err(\"divide by zero\")
    return Ok(a / b)
fn main():
    r := safe_div(10, 2)?
    print(r)
main()";
    assert_eq!(run(src), "5\n");
}

#[test]
fn try_propagates_err_to_caller() {
    let src = "\
fn safe_div(a: int, b: int) -> Result[int]:
    if b == 0:
        return Err(\"zero\")
    return Ok(a / b)
fn use() -> Result[int]:
    r := safe_div(1, 0)?
    return Ok(r + 1)
fn main():
    match use():
        Ok(v): print(\"ok {v}\")
        Err(e): print(\"err {e}\")
main()";
    assert_eq!(run(src), "err zero\n");
}

#[test]
fn try_on_non_result_is_error() {
    let src = "\
fn f() -> int:
    x := (5)?
    return x";
    // Reaching `?` on an int is a runtime error.
    assert!(
        run_err(&format!("{src}\nfn main():\n    print(f())\nmain()"))
            .contains("'?' expects Result or Option, found int")
    );
}

#[test]
fn top_level_try_err_is_unhandled_error() {
    // A `?` at the top level whose Err reaches the top is an unhandled error (no main needed).
    assert_eq!(run_err(r#"x := Err("oops")?"#), "unhandled error: oops");
}

#[test]
fn top_level_try_err_reports_real_line() {
    // The `?` is on line 3 — report there, not at a hard-coded line 1 (parity with the interp).
    let e = run_capture("fn d() -> Result[int]:\n    return Err(\"x\")\nx := d()?\n").unwrap_err();
    assert_eq!(e.message, "unhandled error: x");
    assert_eq!(e.span.line, 3, "expected the `?` line, got {}", e.span.line);
}

/// L3-1: a pure-panic assertion helper with NO return annotation (`fn boom(): panic(...)`) now
/// type-checks (infers `-> nil`) — verify the CALLER runs and FAULTS with the panic message on
/// BOTH engines, and that a `recover:`-wrapped call yields `Err("x")`.
#[test]
fn inline_panic_body_faults_both_engines() {
    let src = "fn boom(): panic(\"x\")\nfn main():\n    print(\"start\")\n    boom()\nmain()\n";
    let (s_out, s_res) = run_program(src);
    let (m_out, m_res) = run_program_parallel(src);
    assert_eq!(s_out, "start\n");
    assert_eq!(m_out, "start\n");
    assert_eq!(s_res.unwrap_err().message, "x");
    assert_eq!(m_res.unwrap_err().message, "x");
    // A `recover:` around the call catches it as `Err("x")` — recoverable, both engines.
    let rec = "fn boom(): panic(\"x\")\nfn main():\n    r := recover:\n        boom()\n        0\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"caught {e.message()}\")\nmain()\n";
    assert_eq!(run_capture(rec).unwrap(), "caught x\n");
    assert_eq!(run_capture_parallel(rec).unwrap(), "caught x\n");
}

// ----- for loops -----

#[test]
fn for_range_sums() {
    let src = "\
fn main():
    total := 0
    for i in 0..1000:
        total += i
    print(total)
main()";
    assert_eq!(run(src), "499500\n");
}

#[test]
fn for_range_is_lazy_not_materialized() {
    // A billion-element range would exhaust memory if materialized; the lazy counting loop
    // returns on the first iteration instantly.
    let src = "\
fn first() -> int:
    for i in 0..1000000000:
        return i
    return -1
fn main():
    print(first())
main()";
    assert_eq!(run(src), "0\n");
}

#[test]
fn for_over_list() {
    let src = "\
fn main():
    total := 0
    for x in [10, 20, 30]:
        total += x
    print(total)
main()";
    assert_eq!(run(src), "60\n");
}

#[test]
fn for_over_non_iterable_errors() {
    assert!(run_err("for x in 5:\n    print(x)").contains("cannot iterate over int"));
}

// ----- match -----

#[test]
fn match_binds_payload() {
    let src = "\
enum Shape:
    Circle(int)
    Square(int)
fn area(s: Shape) -> int:
    match s:
        Shape.Circle(r): return r * r * 3
        Shape.Square(n): return n * n
fn main():
    print(area(Shape.Circle(2)))
    print(area(Shape.Square(3)))
main()";
    assert_eq!(run(src), "12\n9\n");
}

#[test]
fn match_no_arm_is_error() {
    let src = "\
enum Color:
    Red
    Green
    Blue
fn name(c: Color) -> str:
    match c:
        Color.Red: return \"r\"
        Color.Green: return \"g\"
fn main():
    print(name(Color.Blue))
main()";
    assert_eq!(run_err(src), "no match arm for variant 'Blue'");
}

#[test]
fn match_on_non_enum_is_error() {
    // A *payload* variant pattern unambiguously needs an enum scrutinee; matching it on an int is
    // a clean runtime error (the `EnsureEnum` guard) rather than a panic.
    let src = "\
fn main():
    match 5:
        Some(x): print(x)
main()";
    assert!(run_err(src).contains("cannot match on int"));
}

#[test]
fn match_bare_ident_on_non_enum_binds_value() {
    // A bare top-level identifier against a non-enum value is a binding capturing the whole
    // value (the checker permits this only for literal scrutinees) — not an enum-match error.
    let src = "\
fn main():
    match 5:
        x: print(x)
main()";
    assert_eq!(run(src), "5\n");
}

// ----- struct patterns in `match` (L2) -----

#[test]
fn struct_match_binds_fields() {
    let src = "\
struct Point:
    x: int
    y: int
fn main():
    p := Point(1, 2)
    match p:
        Point(a, b):
            print(a)
            print(b)
main()
";
    assert_mc_parity(src, "1\n2\n");
}

#[test]
fn struct_match_generic_and_nested() {
    // Generic field (`Box[T]` → `v:int`) + nested struct field (`Line(Point(x,y), _)`) both bind.
    let src = "\
struct Box[T]:
    v: T
struct Point:
    x: int
    y: int
struct Line:
    a: Point
    b: Point
fn main():
    match Box(7):
        Box(v): print(v)
    l := Line(Point(3, 4), Point(5, 6))
    match l:
        Line(Point(x, y), _): print(x + y)
main()
";
    assert_mc_parity(src, "7\n7\n");
}

#[test]
fn struct_match_literal_field_refutable() {
    // A literal-field arm `Point(0, y)` is refutable → needs a trailing `_`.
    let src = "\
struct Point:
    x: int
    y: int
fn describe(p: Point) -> str:
    match p:
        Point(0, y): return \"on-y-axis\"
        _: return \"off-axis\"
fn main():
    print(describe(Point(0, 5)))
    print(describe(Point(3, 5)))
main()
";
    assert_mc_parity(src, "on-y-axis\noff-axis\n");
}

#[test]
fn struct_match_generic_catchall_keeps_targs() {
    // Regression (bug #1): a generic-struct scrutinee `Box[int]` with a refutable field arm plus a
    // whole-value catch-all `rest:` — the catch-all binding must keep the scrutinee's type args so
    // `rest.v` resolves to the INSTANTIATED field type `int`, not the unsubstituted param `T`.
    let src = "\
struct Box[T]:
    v: T
fn f(b: Box[int]) -> int:
    match b:
        Box(0): return 100
        rest: return rest.v + 1
fn main():
    print(f(Box(0)))
    print(f(Box(41)))
main()
";
    assert_mc_parity(src, "100\n42\n");
}

#[test]
fn struct_match_catchall_name_shadows_struct() {
    // Regression (bug #2): a whole-value catch-all binding whose NAME happens to resolve to another
    // in-scope struct (`Node`) must bind the whole scrutinee — not be mis-lowered as a zero-field
    // struct destructure that binds nothing (which panicked the compiler: `global has no slot`).
    let src = "\
struct Point:
    x: int
    y: int
struct Node:
    v: int
fn describe(p: Point) -> str:
    match p:
        Point(1, y): return \"one-{y}\"
        Node: return \"other-{Node.x}\"
    return \"\"
fn main():
    print(describe(Point(1, 2)))
    print(describe(Point(3, 4)))
main()
";
    assert_mc_parity(src, "one-2\nother-3\n");
}

// ----- field / index -----

#[test]
fn index_list_and_out_of_bounds() {
    assert_eq!(run("print([10, 20, 30][1])"), "20\n");
    assert_eq!(run_err("print([1, 2][5])"), "index 5 out of bounds (len 2)");
}

#[test]
fn index_string_returns_char() {
    assert_eq!(run(r#"print("hello"[1])"#), "e\n");
}

#[test]
fn index_assign_mutates_in_place() {
    assert_eq!(
        run("xs := [1, 2, 3]\nxs[1] = 9\nprint(xs)\n"),
        "[1, 9, 3]\n"
    );
}

#[test]
fn index_compound_assign() {
    assert_eq!(
        run("xs := [1, 2, 3]\nxs[0] += 5\nxs[2] -= 1\nprint(xs)\n"),
        "[6, 2, 2]\n"
    );
}

#[test]
fn compound_assign_all_ops_all_targets_parity() {
    // Every compound op across ident / index / field / map-key targets; VM == interp == --parallel.
    let src = "\
struct P:
    f: int
fn main():
    x := 100
    x *= 3
    x /= 2
    x %= 40
    x &= 12
    x |= 1
    x ^= 5
    x <<= 2
    x >>= 1
    print(x)
    xs := [8]
    xs[0] *= 2
    xs[0] <<= 1
    print(xs)
    p := P(7)
    p.f *= 6
    p.f %= 5
    print(p.f)
    m := {\"k\": 10}
    m[\"k\"] |= 4
    m[\"k\"] *= 2
    print(m[\"k\"])
main()";
    let vm_out = run_capture(src).expect("vm");
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel"));
}

#[test]
fn membership_in_runtime_parity() {
    // list / set / map-key / substring, true + false; VM == interp == --parallel.
    let src = "\
fn main():
    print(2 in [1, 2, 3])
    print(9 in [1, 2, 3])
    s := {10, 20, 30}
    print(20 in s)
    print(99 in s)
    m := {\"a\": 1, \"b\": 2}
    print(\"a\" in m)
    print(\"z\" in m)
    print(\"ell\" in \"hello\")
    print(\"xyz\" in \"hello\")
main()";
    let vm_out = run_capture(src).expect("vm");
    assert_eq!(
        vm_out,
        "true\nfalse\ntrue\nfalse\ntrue\nfalse\ntrue\nfalse\n"
    );
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel"));
}

#[test]
fn tuple_swap_runtime_parity() {
    // vars, list elements (same indices appear on both sides → proves RHS-first eval),
    // and struct fields; VM == interp == --parallel.
    let src = "\
struct P:
    x: int
    y: int
fn main():
    a := 1
    b := 2
    a, b = b, a
    print(a)
    print(b)
    data := [10, 20, 30]
    data[0], data[2] = data[2], data[0]
    print(data)
    p := P(7, 9)
    p.x, p.y = p.y, p.x
    print(p.x)
    print(p.y)
main()";
    let vm_out = run_capture(src).expect("vm");
    assert_eq!(vm_out, "2\n1\n[30, 20, 10]\n9\n7\n");
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel"));
}

#[test]
fn index_assign_out_of_bounds_errors() {
    assert_eq!(
        run_err("xs := [1, 2, 3]\nxs[5] = 0\n"),
        "index 5 out of bounds (len 3)"
    );
}

#[test]
fn field_assign_mutates_in_place() {
    let src = "\
struct P:
    x: int
    y: int
fn main():
    p := P(1, 2)
    p.x = 9
    print(p.x)
    print(p.y)
main()";
    assert_eq!(run(src), "9\n2\n");
}

#[test]
fn field_compound_assign() {
    let src = "\
struct P:
    x: int
fn main():
    p := P(10)
    p.x += 5
    p.x -= 3
    print(p.x)
main()";
    assert_eq!(run(src), "12\n");
}

#[test]
fn field_access_and_unknown_field() {
    let src = "\
struct P:
    x: int
    y: int
fn main():
    p := P(1, 2)
    print(p.x)
    print(p.y)
main()";
    assert_eq!(run(src), "1\n2\n");
}

#[test]
fn struct_method_call_binds_self() {
    let src = "\
struct Counter:
    n: int
    fn doubled(self) -> int:
        return self.n * 2
fn main():
    c := Counter(21)
    print(c.doubled())
main()";
    assert_eq!(run(src), "42\n");
}

// ----- builtins -----

#[test]
fn method_len() {
    assert_eq!(run("print([1, 2, 3].len())"), "3\n");
    assert_eq!(run(r#"print("hello".len())"#), "5\n");
}

#[test]
fn bytes_len_method() {
    assert_eq!(
        run("fn main():\n    b := b\"\\x01\\x02\\x03\"\n    print(b.len())\nmain()\n"),
        "3\n"
    );
}

#[test]
fn builtin_range_and_cap() {
    assert_eq!(run("print(range(3))"), "[0, 1, 2]\n");
    assert_eq!(run("print(range(2, 5))"), "[2, 3, 4]\n");
    assert!(run_err("print(range(20000000))").contains("exceeds the maximum"));
}

#[test]
fn range_three_arg_up_vm() {
    assert_eq!(run("print(range(0, 10, 2))"), "[0, 2, 4, 6, 8]\n");
    assert_eq!(run("print(range(1, 7, 3))"), "[1, 4]\n");
}

#[test]
fn range_three_arg_down_vm() {
    assert_eq!(
        run("print(range(10, 0, -1))"),
        "[10, 9, 8, 7, 6, 5, 4, 3, 2, 1]\n"
    );
    assert_eq!(run("print(range(10, 2, -3))"), "[10, 7, 4]\n");
}

#[test]
fn range_step_zero_faults_vm() {
    let msg = run_err("print(range(0, 5, 0))");
    assert!(msg.contains("range"), "msg: {msg}");
    assert!(msg.contains("step"), "msg: {msg}");
    assert!(msg.contains("zero"), "msg: {msg}");
}

#[test]
fn range_empty_cases_vm() {
    assert_eq!(run("print(range(5, 5, 1))"), "[]\n");
    assert_eq!(run("print(range(5, 5, -1))"), "[]\n");
    assert_eq!(run("print(range(0, 10, -1))"), "[]\n");
    assert_eq!(run("print(range(10, 0, 1))"), "[]\n");
}

#[test]
fn range_slice_step_parity_vm() {
    assert_eq!(run("print((0..10)[::2])"), "[0, 2, 4, 6, 8]\n");
    assert_eq!(run("print((0..10)[1:8:3])"), "[1, 4, 7]\n");
    assert_eq!(run("print((0..5)[::-1])"), "[4, 3, 2, 1, 0]\n");
}

#[test]
fn builtin_casts() {
    assert_eq!(run(r#"print(int("42"))"#), "42\n");
    assert_eq!(run("print(float(3))"), "3.0\n");
    assert_eq!(run("print(str(5))"), "5\n");
    assert!(run_err(r#"print(int("notnum"))"#).contains("cannot parse 'notnum'"));
}

// ----- construction arity / nullary variant -----

#[test]
fn struct_arity_error() {
    let src = "\
struct Point:
    x: int
    y: int
fn main():
    p := Point(1)
main()";
    assert!(run_err(src).contains("struct 'Point' expects 2 field(s), got 1"));
}

#[test]
fn variant_arity_error() {
    assert!(
        run_err("fn main():\n    x := Ok(1, 2)\nmain()")
            .contains("variant 'Ok' expects 1 value(s), got 2")
    );
}

#[test]
fn nullary_variant_used_as_value() {
    assert_eq!(run("print(None)"), "None\n");
    let src = "\
enum Light:
    On
    Off
fn main():
    print(Light.Off)
main()";
    assert_eq!(run(src), "Off\n");
}

// ----- string interpolation -----

#[test]
fn interpolation_and_literal_braces() {
    let src = "\
fn main():
    name := \"chezzi\"
    print(\"hi {name}, {{not interpolated}}\")
main()";
    assert_eq!(run(src), "hi chezzi, {not interpolated}\n");
}

// ----- or-patterns + nested nullary (VM execution + interp parity) -----

/// A literal or-pattern (`1 | 2 | 3`) routes any alternative to the body; the interp agrees.
#[test]
fn vm_or_pattern_literals() {
    let src = "fn f(n: int) -> str:\n    return match n:\n        1 | 2 | 3: \"low\"\n        _: \"high\"\nprint(f(2))\nprint(f(5))\n";
    let out = run(src);
    assert_eq!(out, "low\nhigh\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp"));
}

/// A 3-variant enum or-pattern is exhaustive and matches each alternative; the interp agrees.
#[test]
fn vm_or_pattern_enum_variants() {
    let src = "enum Color:\n    Red\n    Green\n    Blue\nfn name(c: Color) -> str:\n    return match c:\n        Color.Red | Color.Green | Color.Blue: \"primary\"\nprint(name(Color.Green))\nprint(name(Color.Blue))\n";
    let out = run(src);
    assert_eq!(out, "primary\nprimary\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp"));
}

/// A binding or-pattern (`A(a) | B(a)`) writes `a` into the same slot regardless of alternative.
#[test]
fn vm_or_pattern_binding() {
    let src = "enum E:\n    A(int)\n    B(int)\nfn val(e: E) -> int:\n    return match e:\n        E.A(a) | E.B(a): a\nprint(val(E.A(7)))\nprint(val(E.B(9)))\n";
    let out = run(src);
    assert_eq!(out, "7\n9\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp"));
}

/// A guard on an or-pattern: `p | q if cond:` falls through to the next arm when the guard fails.
#[test]
fn vm_or_pattern_with_guard() {
    let src = "fn f(n: int) -> str:\n    return match n:\n        1 | 2 | 3 if n == 2: \"two\"\n        _: \"other\"\nprint(f(2))\nprint(f(1))\n";
    let out = run(src);
    assert_eq!(out, "two\nother\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp"));
}

/// A nested nullary variant (`Some(None)`) is a refutable variant match; the interp agrees.
#[test]
fn vm_nested_nullary_variant() {
    // Nested nullary `None` inside `Some(...)` is a refutable variant match: `Some(None)` matches
    // only the inner-none case; everything else falls to `_`. A single outer `Some` arm + `_` keeps
    // this CLI-valid (the checker allows one arm per outer variant), so the test reflects a program
    // a user can actually run, not just the checker-skipping runtime harness.
    let src = "fn f(oo: Option[Option[int]]) -> str:\n    return match oo:\n        Some(None): \"inner-none\"\n        _: \"other\"\nx: Option[Option[int]] = Some(None)\ny: Option[Option[int]] = Some(Some(5))\nprint(f(x))\nprint(f(y))\n";
    let out = run(src);
    assert_eq!(out, "inner-none\nother\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp"));
}

// ----- experimental generators (VM-only) -----

/// Milestone: a 3-yield counting generator drives a `for` loop, printing each value.
#[test]
fn vm_generator_basic_for_loop() {
    let src = "fn count() -> Iterator[int]:\n    yield 1\n    yield 2\n    yield 3\nfn main():\n    for x in count():\n        print(x)\nmain()\n";
    assert_eq!(run(src), "1\n2\n3\n");
}

/// Golden: the `examples/generators.chz` showcase (free-fn + struct-method generators, `for`,
/// explicit `.next()`, and an `Iterator[T]`-bounded generic) produces exactly this output.
#[test]
fn golden_generators_chz() {
    let out = run(include_str!("../../examples/generators.chz"));
    assert_eq!(out, "0\n1\n2\n10\n11\n12\nSome(0)\nSome(1)\nNone\n5\n");
}

/// Driving a generator by explicit `.next()` yields `Some(v)` per yield, then `None` forever.
#[test]
fn vm_generator_explicit_next() {
    let src = "fn two() -> Iterator[int]:\n    yield 10\n    yield 20\nfn main():\n    g := two()\n    print(g.next())\n    print(g.next())\n    print(g.next())\n    print(g.next())\nmain()\n";
    assert_eq!(run(src), "Some(10)\nSome(20)\nNone\nNone\n");
}

/// A generator whose `yield` is never reached drains immediately: the `for` body never runs.
#[test]
fn vm_generator_never_yields() {
    let src = "fn empty() -> Iterator[int]:\n    if false:\n        yield 1\nfn main():\n    print(\"before\")\n    for x in empty():\n        print(x)\n    print(\"after\")\nmain()\n";
    assert_eq!(run(src), "before\nafter\n");
}

/// Q1: a generator with NO `-> Iterator[T]` annotation — element type inferred by strict-first-yield
/// — drives a `for` loop and RUNS identically on BOTH engines (serial + M:N), not just type-checks.
#[test]
fn vm_generator_inferred_no_annotation() {
    let src = "fn count():\n    yield 1\n    yield 2\n    yield 3\nfn main():\n    for x in count():\n        print(x)\nmain()\n";
    assert_eq!(run(src), "1\n2\n3\n");
    assert_eq!(run_capture_parallel(src).unwrap(), "1\n2\n3\n");
}

/// Q1: the struct-method arm of generator inference also RUNS on both engines — an un-annotated
/// `each` generator method infers `Iterator[int]` and drives a `for`.
#[test]
fn vm_generator_inferred_struct_method() {
    let src = "struct Box:\n    n: int\n    fn each(self):\n        i := 0\n        while i < self.n:\n            yield i\n            i = i + 1\nfn main():\n    b := Box(3)\n    for x in b.each():\n        print(x)\nmain()\n";
    assert_eq!(run(src), "0\n1\n2\n");
    assert_eq!(run_capture_parallel(src).unwrap(), "0\n1\n2\n");
}

/// Q1 golden: the inference-only `examples/generators_inferred.chz` showcase (free-fn + struct-method
/// generators, no `-> Iterator[T]` anywhere) produces exactly this output on BOTH engines.
#[test]
fn golden_generators_inferred_chz() {
    let src = include_str!("../../examples/generators_inferred.chz");
    let expect = "0\n1\n2\n3\n4\n2\n4\n6\n";
    assert_eq!(run(src), expect);
    assert_eq!(run_capture_parallel(src).unwrap(), expect);
}

/// F3 path C: a PENDING generator passed DIRECTLY as a spawn arg now CROSSES the airlock BY VALUE
/// (deep copy) — the callee runs on its own copy instead of faulting. serial == M:N.
#[test]
fn generator_passed_to_spawn_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn work(g: Iterator[int]):\n",
        "    print(\"hi\")\n",
        "fn main():\n",
        "    parallel:\n",
        "        spawn work(gen())\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("serial"), "hi\n");
    assert_eq!(run_capture_parallel(src).expect("M:N"), "hi\n");
}

/// F3 path C: a generator nested inside a LIST spawn arg crosses BY VALUE too (the container recursion
/// in `to_wire` serializes the leaf generator instead of faulting). serial == M:N.
#[test]
fn generator_in_list_arg_to_spawn_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn work(g: List[Iterator[int]]):\n",
        "    print(\"hi\")\n",
        "fn main():\n",
        "    parallel:\n",
        "        spawn work([gen()])\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("serial"), "hi\n");
    assert_eq!(run_capture_parallel(src).expect("M:N"), "hi\n");
}

/// F3 path C: a generator stored into `Shared(...)` now crosses BY VALUE (deep copy into the box)
/// instead of faulting; the program runs to completion. serial == M:N.
#[test]
fn generator_into_shared_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn main():\n",
        "    s := Shared(gen())\n",
        "    print(\"stored\")\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("serial"), "stored\n");
    assert_eq!(run_capture_parallel(src).expect("M:N"), "stored\n");
}

/// F3 path C: a generator stored into `Atomic(...)` now crosses BY VALUE (deep copy into the box).
/// serial == M:N.
#[test]
fn generator_into_atomic_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn main():\n",
        "    a := Atomic(gen())\n",
        "    print(\"stored\")\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("serial"), "stored\n");
    assert_eq!(run_capture_parallel(src).expect("M:N"), "stored\n");
}

/// F3 path C: a closure CAPTURING a local generator submitted to an `Executor` under `--parallel` now
/// crosses BY VALUE — the submitted task drives its own copy instead of faulting. Runs to completion.
#[test]
fn generator_captured_in_executor_submit_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn main():\n",
        "    g := gen()\n",
        "    ex := Executor()\n",
        "    ex.submit(fn(): print(g.next()))\n",
        "    ex.shutdown()\n",
        "main()\n"
    );
    assert_eq!(run_capture_parallel(src).expect("M:N"), "Some(1)\n");
}

/// F3 path C: an `Executor` task that RETURNS a generator under `--parallel` now crosses the return
/// value BY VALUE (deep copy back to the parent). The Executor discards task returns, so no output.
#[test]
fn generator_returned_from_executor_task_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn make() -> Iterator[int]:\n",
        "    return gen()\n",
        "fn main():\n",
        "    ex := Executor()\n",
        "    ex.submit(make)\n",
        "    ex.shutdown()\n",
        "main()\n"
    );
    assert_eq!(run_capture_parallel(src).expect("M:N"), "");
}

/// F3 path C: a generator into `Channel.send(...)` now crosses BY VALUE (deep copy into the queue)
/// instead of faulting; the program runs to completion. serial == M:N.
#[test]
fn generator_into_channel_crosses_by_value() {
    let src = concat!(
        "fn gen() -> Iterator[int]:\n",
        "    yield 1\n",
        "fn main():\n",
        "    ch := Channel[Iterator[int]]()\n",
        "    ch.send(gen())\n",
        "    print(\"sent\")\n",
        "main()\n"
    );
    assert_eq!(run_capture(src).expect("serial"), "sent\n");
    assert_eq!(run_capture_parallel(src).expect("M:N"), "sent\n");
}

/// Captured args mutate across yields; an infinite generator terminates via `break`.
#[test]
fn vm_generator_captured_args_and_break() {
    let src = "fn count_from(n: int) -> Iterator[int]:\n    while true:\n        yield n\n        n = n + 1\nfn main():\n    for x in count_from(10):\n        if x > 13:\n            break\n        print(x)\nmain()\n";
    assert_eq!(run(src), "10\n11\n12\n13\n");
}

/// Nested generators: an outer generator's body drives an inner generator, each with its own
/// suspended context. Proves the private-context swap + host-rooting compose under recursion.
#[test]
fn vm_generator_nested() {
    let src = "fn inner(n: int) -> Iterator[int]:\n    yield n\n    yield n * 10\nfn outer() -> Iterator[int]:\n    for a in inner(1):\n        yield a\n    for b in inner(2):\n        yield b\nfn main():\n    for x in outer():\n        print(x)\nmain()\n";
    assert_eq!(run(src), "1\n10\n2\n20\n");
}

/// A generator declared as a STRUCT METHOD must also allocate-not-run: dispatched through
/// `do_method_call`, it returns a suspendable generator (regression — the method path once ran
/// the body inline, poisoning the host with a stray `gen_yielding`).
#[test]
fn vm_generator_struct_method() {
    let src = "struct C:\n    n: int\n    fn items(self) -> Iterator[int]:\n        yield self.n\n        yield self.n + 1\nfn main():\n    c := C(5)\n    for x in c.items():\n        print(x)\nmain()\n";
    assert_eq!(run(src), "5\n6\n");
}

/// A generator yielding heap values (strings) survives GC stress between/within `.next()` calls:
/// the suspended frames + yielded objects must stay rooted across collections.
#[test]
fn vm_generator_survives_gc_stress() {
    let src = "fn words() -> Iterator[str]:\n    yield \"alpha\"\n    yield \"beta\"\n    yield \"gamma\"\nfn main():\n    for w in words():\n        print(w)\nmain()\n";
    assert_eq!(run_capture_stress(src), "alpha\nbeta\ngamma\n");
}

// ----- golden parity -----

#[test]
fn golden_hello_chz_matches_expected() {
    let expected = include_str!("../../examples/hello.expected");
    assert_eq!(run(include_str!("../../examples/hello.chz")), expected);
}

/// `pass` no-op-statement golden: a lone-`pass` fn body (== a `return`-only body: runs, falls off
/// the end → nil) plus `pass` as a no-op inside `if`/`for`/`while`. `pass` compiles to no bytecode,
/// so VM and interp are byte-identical.
#[test]
fn golden_pass_noop_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/pass_noop.chz");
    let expected = include_str!("../../examples/pass_noop.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from pass_noop.expected"
    );
    assert_eq!(vm_out, interp_out, "vm/interp divergence on pass_noop");
}

/// Empty-protocol golden: `protocol Foo:\n    pass` is an accept-all top type (structural over
/// zero methods ⇒ every type satisfies it), byte-identical to the reserved `Any` — accepts
/// int/str/bool/struct params and types a heterogeneous `List[Foo]`. Erased at runtime, so VM ==
/// interp.
#[test]
fn golden_empty_protocol_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/empty_protocol.chz");
    let expected = include_str!("../../examples/empty_protocol.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from empty_protocol.expected"
    );
    assert_eq!(vm_out, interp_out, "vm/interp divergence on empty_protocol");
}

/// Empty-struct golden: `struct S:\n    pass` has zero fields — `S()` constructs, prints `S()`,
/// two `S()` compare equal, a distinct empty struct `T()` is not equal, and `S` is usable as a Set
/// element / Map key (zero-field structs are intrinsically Hashable, both engines returning the
/// same constant hash). VM == interp.
#[test]
fn golden_empty_struct_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/empty_struct.chz");
    let expected = include_str!("../../examples/empty_struct.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from empty_struct.expected"
    );
    assert_eq!(vm_out, interp_out, "vm/interp divergence on empty_struct");
}

/// Container-constructor golden: `examples/container_ctor.chz` exercises `List[T]()` / `Map[K,V]()`
/// / `Set[T]()` turbofish + bare 0-arg `List()`/`Map()`. Type args erase at runtime (an empty
/// `List[int]()` is just an empty list), so VM and interp are byte-identical — this is the
/// two-engine parity gate for the constructor-turbofish feature.
#[test]
fn golden_container_ctor_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/container_ctor.chz");
    let expected = include_str!("../../examples/container_ctor.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from container_ctor.expected"
    );
    assert_eq!(vm_out, interp_out, "vm/interp divergence on container_ctor");
}

/// First-class native-type golden: `examples/native_qualified.chz` exercises the ADDITIVE
/// qualified / aliased module-member path for import-gated native types — `concurrency.Shared(0)`,
/// aliased `c.Shared(0)`, qualified RwShared/Atomic/Executor, `time.timer(0)`, plus a type-alias
/// and a newtype over a qualified `concurrency.Shared`. These lower to the SAME opcodes as the
/// bare-after-import names, so output is byte-identical on interp, the cooperative VM, AND the M:N
/// engine (`run_capture_parallel`) — the three-engine parity gate for the qualified-ctor lowering.
#[test]
fn golden_native_qualified_chz_matches_expected_and_interp() {
    // Imports (`std.concurrency` / `std.time`) require the module-graph path, so drive `run_file`
    // (not `run_capture`, which skips resolution and never populates `imported_modules`). Three
    // engines must agree byte-for-byte: VM(serial), interp, and the M:N OS-thread engine.
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join("examples/native_qualified.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/native_qualified.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("native_qualified.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from native_qualified.expected"
    );
    let (ip_out, _e2, ip_res, _) = run_file_p(&path);
    ip_res.expect("native_qualified.chz should run on the interp");
    assert_eq!(vm_out, ip_out, "vm/interp divergence on native_qualified");
    let (mn_out, _e3, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("native_qualified.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on native_qualified");
}

/// `where`-clause generic bounds golden: `examples/where_sort_sum.chz` exercises the file-backed
/// List `sort` (`native fn sort(self) -> nil where T: Comparable`) on int/float/struct-with-
/// `compare` lists and `sum` (`where T: Add`) on int/float lists. A `where` clause lowers to
/// NOTHING at runtime (checker-only), so the three engines — VM(serial), interp, and the M:N
/// OS-thread engine — must agree byte-for-byte with `.expected`.
#[test]
fn golden_conditional_method_chz_matches_expected_and_interp() {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join("examples/conditional_method.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/conditional_method.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("conditional_method.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from conditional_method.expected"
    );
    let (ip_out, _e2, ip_res, _) = run_file_p(&path);
    ip_res.expect("conditional_method.chz should run on the interp");
    assert_eq!(vm_out, ip_out, "vm/interp divergence on conditional_method");
    let (mn_out, _e3, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("conditional_method.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on conditional_method");
}

#[test]
fn golden_where_sort_sum_chz_matches_expected_and_interp() {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join("examples/where_sort_sum.chz");
    let expected = std::fs::read_to_string(base.join("examples/where_sort_sum.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("where_sort_sum.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from where_sort_sum.expected"
    );
    let (ip_out, _e2, ip_res, _) = run_file_p(&path);
    ip_res.expect("where_sort_sum.chz should run on the interp");
    assert_eq!(vm_out, ip_out, "vm/interp divergence on where_sort_sum");
    let (mn_out, _e3, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("where_sort_sum.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on where_sort_sum");
}

/// Variadic-parameter + `Any`-top-type golden: `examples/variadic.chz` exercises a variadic user
/// fn (`...xs: int`), a zero-arg variadic call, a pre-variadic positional + keyword-only-default
/// combination, and an `Any` parameter slot. The variadic collapse happens in the desugar pass (a
/// synthesized `List` literal), so the call is an ordinary positional call to both engines — output
/// must be byte-identical on VM(serial), interp, and the M:N OS-thread engine, plus the `.expected`.
#[test]
fn golden_variadic_chz_matches_expected_and_interp() {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join("examples/variadic.chz");
    let expected = std::fs::read_to_string(base.join("examples/variadic.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("variadic.chz should run on the VM");
    assert_eq!(vm_out, expected, "vm output drifted from variadic.expected");
    let (ip_out, _e2, ip_res, _) = run_file_p(&path);
    ip_res.expect("variadic.chz should run on the interp");
    assert_eq!(vm_out, ip_out, "vm/interp divergence on variadic");
    let (mn_out, _e3, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("variadic.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on variadic");
}

/// Phase-4d file-backed native-module golden: `examples/std_native_4d.chz` exercises the five
/// migrated pure-function modules (`std.math`/`io`/`os`/`rand`/`fs`) whose signatures now come from
/// real `std/<M>.chz` files (bodyless `native fn` decls) instead of a hand-built `native_module_sig`
/// arm. Dispatch is UNCHANGED (name-keyed `native_members`), so output must be byte-identical on all
/// three engines: VM(serial), interp, and the M:N OS-thread engine. `rand` is seeded and `getcwd` is
/// matched (not printed), so the sequence is deterministic — but ONLY while serialized against the
/// other rand tests: `std_native_4d.chz` does `rand.seed(1)` then `rand.int(0,100)` as two separate
/// native calls on the shared process-global RNG, so the seed→draw sequence must hold
/// `TEST_RNG_LOCK` across the whole run (like `golden_rand_via_run_file`) or the parallel harness
/// interleaves a sibling test's reseed between them and the draw drifts off `65`.
#[test]
fn golden_std_native_4d_chz_matches_expected_and_interp() {
    // Serialize against the rand unit tests / rand goldens (shared process-global RNG); see
    // TEST_RNG_LOCK. Hold it across the whole seed→draw run so the parallel harness cannot
    // interleave another test's reseed between this program's rand.seed(1) and rand.int(0,100).
    let _g = crate::native::rand::TEST_RNG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join("examples/std_native_4d.chz");
    let expected = std::fs::read_to_string(base.join("examples/std_native_4d.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("std_native_4d.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from std_native_4d.expected"
    );
    let (ip_out, _e2, ip_res, _) = run_file_p(&path);
    ip_res.expect("std_native_4d.chz should run on the interp");
    assert_eq!(vm_out, ip_out, "vm/interp divergence on std_native_4d");
    let (mn_out, _e3, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("std_native_4d.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on std_native_4d");
}

/// Phase-4c file-backed native-module golden: `examples/std_native_4c.chz` exercises the migrated
/// std.ffi C-buffer surface (`alloc`/`alloc_zeroed`/`store_int64_at`/`load_int64_at`/
/// `store_int32_at`/`load_int32_at`/`is_null`/`null`/`free`) whose 59 signatures now come from the
/// real `std/ffi.chz` (bodyless `native fn` decls) instead of a hand-built `native_module_sig` arm.
/// Dispatch is UNCHANGED (name-keyed `native_members("std.ffi")`), so output must be byte-identical
/// on all three engines: VM(serial), interp, and the M:N OS-thread engine. FFI is layout-dependent
/// UB, so this drives a REAL alloc/store/load round-trip (never printing a nondeterministic pointer
/// address, only the round-tripped payload) — a stronger parity guard than a pure-checker test.
#[test]
fn golden_std_native_4c_chz_matches_expected_and_interp() {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join("examples/std_native_4c.chz");
    let expected = std::fs::read_to_string(base.join("examples/std_native_4c.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("std_native_4c.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from std_native_4c.expected"
    );
    let (ip_out, _e2, ip_res, _) = run_file_p(&path);
    ip_res.expect("std_native_4c.chz should run on the interp");
    assert_eq!(vm_out, ip_out, "vm/interp divergence on std_native_4c");
    let (mn_out, _e3, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("std_native_4c.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on std_native_4c");
}

/// Phase-5b file-backed native-enum golden: `examples/native_enum_smoke.chz` exercises the reserved
/// `Option`/`Result` surface whose variant SHAPE is now declared in `std/prelude.chz` as
/// `native enum Option[T]` / `native enum Result[T, E]` — Some/None/Ok/Err CONSTRUCTION, `?` on a
/// Result-returning AND an Option-returning fn, and exhaustive `match`. The port is SHAPE-ONLY (the
/// `?`/match/construction wiring stays Rust-inline), so output must be byte-identical on all three
/// engines: VM(serial), interp, and the M:N OS-thread engine. This is the phase-5b behavior-
/// preservation gate — any drift means the file-backed shape decoupled from the Rust wiring.
#[test]
fn golden_native_enum_smoke_chz_matches_expected_and_interp() {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join("examples/native_enum_smoke.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/native_enum_smoke.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("native_enum_smoke.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from native_enum_smoke.expected"
    );
    let (ip_out, _e2, ip_res, _) = run_file_p(&path);
    ip_res.expect("native_enum_smoke.chz should run on the interp");
    assert_eq!(vm_out, ip_out, "vm/interp divergence on native_enum_smoke");
    let (mn_out, _e3, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("native_enum_smoke.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on native_enum_smoke");
}

/// Qualified-type-as-static-method-receiver golden: `examples/qualified_static/main.chz` imports a
/// sibling module and calls `counter.Counter.zero()` / `counter.Counter.of(42)` (struct statics)
/// and `counter.Color.first()` (enum static) through a QUALIFIED type. These lower to the SAME
/// `Op::CallStatic` the bare `Counter.zero()` form emits, so output is byte-identical across all
/// three engines (VM serial, interp, M:N) — the three-engine parity gate for the qualified-static
/// lowering. Multi-file (sibling import) needs the module-graph path, so drive `run_file`.
#[test]
fn golden_qualified_static_chz_matches_expected_and_interp() {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join("examples/qualified_static/main.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/qualified_static/main.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("qualified_static/main.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from qualified_static/main.expected"
    );
    let (ip_out, _e2, ip_res, _) = run_file_p(&path);
    ip_res.expect("qualified_static/main.chz should run on the interp");
    assert_eq!(vm_out, ip_out, "vm/interp divergence on qualified_static");
    let (mn_out, _e3, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("qualified_static/main.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on qualified_static");
}

/// Qualified-FFI-width golden: `examples/ffi_qualified.chz` declares `extern fn abs(ffi.int32) ->
/// ffi.int32` — a QUALIFIED width name in an extern signature. `resolve_ctype_d` maps it to the
/// SAME `CType::Int32` the bare `int32` resolves to, so the C ABI marshalling is identical. Linux-
/// only (needs libc.so.6); drives `run_file` (extern decls need the module-graph) and asserts VM +
/// interp parity. FFI is layout-dependent, hence a real C call rather than a unit assert.
#[test]
#[cfg(target_os = "linux")]
fn golden_ffi_qualified_chz_matches_expected_and_interp() {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join("examples/ffi_qualified.chz");
    let expected = std::fs::read_to_string(base.join("examples/ffi_qualified.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("ffi_qualified.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from ffi_qualified.expected"
    );
    let (ip_out, _e2, ip_res, _) = run_file_p(&path);
    ip_res.expect("ffi_qualified.chz should run on the interp");
    assert_eq!(vm_out, ip_out, "vm/interp divergence on ffi_qualified");
}

/// Inline-expr fn body golden: `examples/inline_fn.chz` exercises Option A (inline-only) — a
/// `fn a(): <expr>` (and the annotated `fn a() -> int: <expr>`) implicitly returns its single
/// expression, usable as a value and a `.map` argument. Byte-identical on VM, interp, and the
/// checked-in `.expected` is the parity gate.
#[test]
fn golden_inline_fn_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/inline_fn.chz");
    let expected = include_str!("../../examples/inline_fn.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from inline_fn.expected"
    );
    assert_eq!(vm_out, interp_out, "vm/interp divergence on inline_fn");
}

/// Swift-style keyword arguments through a function VALUE golden: `examples/keyword_value.chz`
/// exercises by-label / reordered / mixed positional+label / HOF-param-labels / closure-value /
/// slot-order-eval keyword calls. The checker resolves each to a positional slot permutation and
/// both engines lower it to the SAME positional `Op::Call`, so output is byte-identical across all
/// three engines (VM serial, interp, M:N) — the three-engine parity gate for the feature.
#[test]
fn golden_keyword_value_chz_matches_expected_and_interp() {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join("examples/keyword_value.chz");
    let expected = std::fs::read_to_string(base.join("examples/keyword_value.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("keyword_value.chz should run on the VM");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from keyword_value.expected"
    );
    let (ip_out, _e2, ip_res, _) = run_file_p(&path);
    ip_res.expect("keyword_value.chz should run on the interp");
    assert_eq!(vm_out, ip_out, "vm/interp divergence on keyword_value");
    let (mn_out, _e3, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("keyword_value.chz should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on keyword_value");
}

/// Cross-module value+keyword parity: `examples/keyword_value_xmod/main.chz` calls an IMPORTED fn
/// through a value with reordered keyword args. The permutation is keyed by the CALL-SITE module
/// index, so this locks that module-scoped keying (the extern-sig precedent) resolves correctly
/// across module boundaries on all three engines.
#[test]
fn golden_keyword_value_xmod_matches_expected_and_interp() {
    let base = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = base.join("examples/keyword_value_xmod/main.chz");
    let expected =
        std::fs::read_to_string(base.join("examples/keyword_value_xmod/main.expected")).unwrap();
    let (vm_out, _e1, vm_res, _) = run_file(&path);
    vm_res.expect("keyword_value_xmod should run on the VM");
    assert_eq!(vm_out, expected, "vm output drifted on keyword_value_xmod");
    let (ip_out, _e2, ip_res, _) = run_file_p(&path);
    ip_res.expect("keyword_value_xmod should run on the interp");
    assert_eq!(vm_out, ip_out, "vm/interp divergence on keyword_value_xmod");
    let (mn_out, _e3, mn_res, _) = run_file_parallel(&path, crate::native::HostConfig::default());
    mn_res.expect("keyword_value_xmod should run on the M:N engine");
    assert_eq!(mn_out, expected, "M:N output drifted on keyword_value_xmod");
}

/// Chained `elif` in expression position: `examples/expr_else_if.chz` exercises a multi-arm
/// `if p: a elif q: b else: c` chain (right-associative nesting) selecting each branch.
/// Byte-identical on VM, interp, and the checked-in `.expected` is the parity gate.
#[test]
fn golden_expr_else_if_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/expr_else_if.chz");
    let expected = include_str!("../../examples/expr_else_if.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from expr_else_if.expected"
    );
    assert_eq!(vm_out, interp_out, "vm/interp divergence on expr_else_if");
}

#[test]
fn golden_hello_chz_matches_interpreter() {
    let src = include_str!("../../examples/hello.chz");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(vm_out, interp_out);
}

/// M21 newtype golden: `examples/newtype.chz` exercises construct/unwrap, same-type
/// arithmetic + compare, a `str(self)` Stringable override, a runtime-dispatched `hash(self)`
/// for map/set keys, a generic `[T: Add]` bound over a newtype, and a str-newtype unwrap.
/// Byte-identical on the VM, interp, and the checked-in `.expected`.
#[test]
fn golden_newtype_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/newtype.chz");
    let expected = include_str!("../../examples/newtype.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(vm_out, expected, "vm output drifted from newtype.expected");
    assert_eq!(vm_out, interp_out, "vm/interp divergence on newtype");
}

/// Golden: a user struct/enum method whose name collides with a built-in method name (`add`,
/// `map`) still gets named/default-arg support when the receiver's struct type is statically
/// known (typed local / inline ctor / struct-returning fn), while a genuine builtin receiver
/// (List) routes to the builtin untouched. Byte-identical on the VM, interp, and `.expected`.
#[test]
fn golden_builtin_named_method_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/builtin_named_method.chz");
    let expected = include_str!("../../examples/builtin_named_method.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from builtin_named_method.expected"
    );
    assert_eq!(
        vm_out, interp_out,
        "vm/interp divergence on builtin_named_method"
    );
}

/// M22 golden: operator protocols (`div`/`mod`/`neg`), protocol embedding (super-protocols), and
/// the builtin `Arithmetic` bundle — byte-identical on the VM, interp, the M:N parallel engine,
/// and the checked-in `.expected`.
#[test]
fn golden_arithmetic_protocol_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/arithmetic_protocol.chz");
    let expected = include_str!("../../examples/arithmetic_protocol.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    let par_out = run_capture_parallel(src).expect("parallel run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from arithmetic_protocol.expected"
    );
    assert_eq!(
        vm_out, interp_out,
        "vm/interp divergence on arithmetic_protocol"
    );
    assert_eq!(
        vm_out, par_out,
        "vm/parallel divergence on arithmetic_protocol"
    );
}

/// Generic operator-overload golden: `examples/generic_operator_overload.chz` — a generic struct
/// `Box[T]` and generic enum `Num[T]` whose `add`/`neg`/`compare` methods overload `+`/`-`/`<`,
/// satisfy `Add`/`Comparable`, and flow into `twice[T: Add]`. Byte-identical on the VM, the interp
/// (parity oracle), and the M:N parallel engine, plus the checked-in `.expected`.
#[test]
fn golden_generic_operator_overload_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/generic_operator_overload.chz");
    let expected = include_str!("../../examples/generic_operator_overload.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    let par_out = run_capture_parallel(src).expect("parallel run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from generic_operator_overload.expected"
    );
    assert_eq!(
        vm_out, interp_out,
        "vm/interp divergence on generic_operator_overload"
    );
    assert_eq!(
        vm_out, par_out,
        "vm/parallel divergence on generic_operator_overload"
    );
}

/// M22: a struct defining `div`/`mod`/`neg` overloads `/`, `%`, and unary `-`. Runs byte-identical
/// on the VM, the cooperative interp (parity oracle), and the M:N parallel engine.
#[test]
fn struct_div_mod_neg_runs() {
    let src = "struct V:\n    n: int\n    fn div(self, o: V) -> V:\n        return V(self.n / o.n)\n    fn mod(self, o: V) -> V:\n        return V(self.n % o.n)\n    fn neg(self) -> V:\n        return V(-self.n)\nfn main():\n    a := V(7)\n    b := V(2)\n    print((a / b).n)\n    print((a % b).n)\n    print((-a).n)\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "3\n1\n-7\n");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(vm_out, interp_out, "vm/interp divergence on div/mod/neg");
    let par_out = run_capture_parallel(src).expect("parallel run");
    assert_eq!(vm_out, par_out, "vm/parallel divergence on div/mod/neg");
}

/// Static (associated) methods golden: `examples/static_methods.chz` — a named/alternative struct
/// ctor (`Rect.square`), a validating ctor returning `Result` (`Email.parse`), an enum static
/// ctor returning `Option` (`Color.from_str`) alongside a variant that wins over a static name,
/// and a generic static via the type-level turbofish (`Box[int].empty()`). Static dispatch pushes
/// no receiver, so it is behavior-preserving; byte-identical on the VM, interp, and the M:N
/// parallel engine, plus the checked-in `.expected`.
#[test]
fn golden_static_methods_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/static_methods.chz");
    let expected = include_str!("../../examples/static_methods.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    let par_out = run_capture_parallel(src).expect("parallel run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from static_methods.expected"
    );
    assert_eq!(vm_out, interp_out, "vm/interp divergence on static_methods");
    assert_eq!(
        vm_out, par_out,
        "parallel engine diverged on static_methods"
    );
}

/// M24 static-witness golden: `examples/static_witness.chz` — a protocol's STATIC (no-`self`)
/// requirement called THROUGH a generic bound (`T.default()` inside `fn reset[T: Default]`), on a
/// struct, an enum and the reserved `Convert[int]`; `T` pinned by turbofish and by an annotated
/// result; witness FORWARDING (`twice`); a MEMBER's own `[T]` (instance + static); and the witness
/// reaching an escaping closure, a nested `fn`, a `defer:` block and a `spawn:` block. The witness
/// is a hidden trailing argument, so nothing is monomorphized and the runtime stays erased —
/// byte-identical on the serial VM, the M:N engine, and the checked-in `.expected`.
#[test]
fn golden_static_witness_chz_matches_expected_and_parallel() {
    let src = include_str!("../../examples/static_witness.chz");
    let expected = include_str!("../../examples/static_witness.expected");
    let vm_out = run_capture(src).expect("vm run");
    let par_out = run_capture_parallel(src).expect("parallel run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from static_witness.expected"
    );
    assert_eq!(
        vm_out, par_out,
        "parallel engine diverged on static_witness"
    );
}

/// M21 generic-newtype golden: `examples/newtype_generic.chz` exercises type-parameterized
/// newtypes — `Stack[T] = List[T]` / `Tally[T: Hashable] = Map[T, int]` with methods that
/// reference `T`, ctor inference + turbofish construction (`Stack[str]([])`), method dispatch with
/// the type args substituted (`Option[int]`), and cast-unwrap propagation (`List(s)`→`List[int]`,
/// `Map(t)`→the inner map). Runtime is type-erased, so byte-identical on the VM, interp, and the
/// checked-in `.expected`.
#[test]
fn golden_newtype_generic_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/newtype_generic.chz");
    let expected = include_str!("../../examples/newtype_generic.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from newtype_generic.expected"
    );
    assert_eq!(
        vm_out, interp_out,
        "vm/interp divergence on newtype_generic"
    );
    // 3-engine bar (M21/M19): the M:N --parallel engine must agree too.
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("parallel run"),
        "parallel drifted from vm on newtype_generic"
    );
}

/// A raw string is an ordinary `str` end-to-end: usable as a match-arm literal pattern
/// (regression — the pattern parser previously rejected it) with its braces kept literal,
/// byte-identical on both engines.
#[test]
fn raw_string_as_match_pattern_runs() {
    let src = "fn classify(s: str) -> str:\n    return match s:\n        r\"{}\": \"braces\"\n        _: \"other\"\nfn main():\n    print(classify(r\"{}\"))\n    print(classify(\"x\"))\nmain()\n";
    assert_eq!(run_parity(src), "braces\nother\n");
}

/// Raw-string golden: `examples/raw_string.chz` exercises the verbatim `r"..."` forms —
/// literal braces (`r"{}"` → `{}`, the thing that needed `{{}}` before), literal backslashes
/// (`r"\d+\s"`), the single-quote + Windows-path form, the triple form embedding quotes+braces
/// (JSON), and a side-by-side proving a NORMAL string still interpolates (`"{x}"` → 5) while the
/// raw form keeps `{x}` literal. NO interpolation + NO escapes, so VM and interp emit the same
/// compile-time literal — byte-identical on the VM, interp, and the checked-in `.expected`.
#[test]
fn golden_raw_string_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/raw_string.chz");
    let expected = include_str!("../../examples/raw_string.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from raw_string.expected"
    );
    assert_eq!(vm_out, interp_out, "vm/interp divergence on raw_string");
}

/// Same numeric newtype `/` and `%` auto-flow the underlying op and re-wrap, just like `+ - *`.
/// Regression: the checker's `Div|Mod` arm previously rejected these even though the runtime
/// handled them (dead runtime path) — now checker + both engines agree.
#[test]
fn newtype_div_mod_same_type_flows() {
    let src = "newtype Meters = float\nnewtype Count = int\nfn main():\n    a := Meters(8.0) / Meters(2.0)\n    print(int(a))\n    c := Count(7) % Count(3)\n    print(int(c))\nmain()\n";
    assert_eq!(run_parity(src), "4\n1\n");
}

/// M19 memory-layout lever #1 golden: `examples/struct_layout.chz` exercises the positional
/// struct-field storage across every observable surface the layout change touches — field
/// read/write, struct `==` (equal / unequal-by-value / unequal-by-type), Display + `{}`
/// interpolation (names recovered from the type in declaration order), nested structs, a generic
/// `Box[T]` with two monomorphs sharing ONE type-erased layout, and a reordered named-field
/// constructor. Byte-identical on the VM, interp, and the checked-in `.expected` is the parity
/// gate: the layout change is behavior-preserving, so any divergence turns this RED.
#[test]
fn golden_struct_layout_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/struct_layout.chz");
    let expected = include_str!("../../examples/struct_layout.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from struct_layout.expected"
    );
    assert_eq!(
        vm_out, interp_out,
        "vm/interp divergence on struct_layout (layout must be behavior-preserving)"
    );
}

/// M19 memory-layout lever #1 — assert the VM stores struct fields POSITIONALLY (a flat
/// `Vec<Value>`, hidden-class / `__slots__` layout) with NO per-instance field-name strings.
/// Compiles a program with `struct Point(x, y)`, drives the real `new_struct` construction path
/// (push args, call `new_struct`), then pattern-matches the heap object and verifies the field
/// Vec is `[Value::int(1), Value::int(2)]` in declaration order — names live only in the
/// StructDef. This is the type-level guard the layout change is built around (the destructure of
/// `fields` as `&Vec<Value>` would not compile against the old `Vec<(Box<str>,Value)>`).
#[test]
fn struct_positional_layout_no_per_instance_names() {
    let tokens = lexer::tokenize(
        "struct Point:\n    x: int\n    y: int\nfn main():\n    print(0)\nmain()\n",
    )
    .expect("lex");
    let module = parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    let mut vm = Vm::new(Arc::new(program));
    let span = Span::RUNTIME;
    vm.push(Value::int(1));
    vm.push(Value::int(2));
    // ROOT REDESIGN — structs are keyed by the qualified IDENTITY KEY (`<main>::Point` standalone).
    vm.new_struct("<main>::Point", 2, span).expect("new_struct");
    let Some(h) = vm.pop().as_obj() else {
        panic!("expected struct obj")
    };
    match vm.heap.get(h) {
        Obj::Struct { tid, fields, .. } => {
            let tid = *tid;
            // positional: NOT Vec<(Box<str>, Value)>
            assert_eq!(
                fields.as_slice(),
                &[Value::int(1), Value::int(2)],
                "fields must be positional in declaration order, no per-instance names"
            );
            // Name is resolved from tid (no per-instance `name` field), not stored on the instance.
            assert_eq!(vm.struct_name_of_tid(tid), "<main>::Point");
        }
        _ => panic!("expected Obj::Struct"),
    }
}

/// M19 memory-layout lever #2 — assert the VM stores an enum's identity as a dense `variant_id:
/// u32` (NO per-instance `ty`/`variant` Box<str>), the enum analogue of struct `tid` (lever #1).
/// Compiles `enum Color: Red, Green, Blue`, drives the real `new_enum` construction path, then
/// pattern-matches the heap object and verifies it carries only `{ variant_id, payload }` — the
/// destructure of `payload` as `&Vec<Value>` with `variant_id` would not compile against the old
/// `{ ty, variant, payload }` shape (the type-level guard). The id is dense and stable per
/// (enum-type, variant) pair.
#[test]
fn enum_variant_id_stamped_at_construction() {
    let tokens = lexer::tokenize(
        "enum Color:\n    Red\n    Green\n    Blue\nfn main():\n    print(0)\nmain()\n",
    )
    .expect("lex");
    let module = parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    // `Green`'s dense id from the program table (resolved on the cold path). ROOT REDESIGN — the
    // enum is keyed by its qualified IDENTITY KEY (`<main>::Color` on the standalone path).
    let green_id = program
        .variants
        .get(&("<main>::Color".to_string(), "Green".to_string()))
        .expect("Green registered")
        .variant_id;
    let mut vm = Vm::new(Arc::new(program));
    let span = Span::RUNTIME;
    vm.new_enum("Green", green_id, 0, span).expect("new_enum");
    let Some(h) = vm.pop().as_obj() else {
        panic!("expected enum obj")
    };
    match vm.heap.get(h) {
        Obj::Enum {
            variant_id,
            payload,
        } => {
            assert_eq!(
                *variant_id, green_id,
                "variant_id must be the dense id stamped at construction"
            );
            let payload: &Vec<Value> = payload; // no per-instance ty/variant Box<str>
            assert!(payload.is_empty(), "nullary variant has empty payload");
        }
        _ => panic!("expected Obj::Enum"),
    }
}

/// M19 lever #2 — native `Result`/`Option` variants get FIXED low ids assigned first
/// (`Ok`=VID_OK, `Err`=VID_ERR, `Some`=VID_SOME, `None`=VID_NONE_VARIANT) so `?`/error-gating
/// can compare against compile-time constants (JIT-jump-table groundwork). User variants follow.
#[test]
fn native_result_option_have_fixed_variant_ids() {
    use crate::vm::op::{VID_ERR, VID_NONE_VARIANT, VID_OK, VID_SOME};
    let tokens = lexer::tokenize("fn main():\n    print(0)\nmain()\n").expect("lex");
    let module = parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    let vid = |e: &str, v: &str| {
        program
            .variants
            .get(&(e.to_string(), v.to_string()))
            .unwrap()
            .variant_id
    };
    assert_eq!(vid("Result", "Ok"), VID_OK);
    assert_eq!(vid("Result", "Err"), VID_ERR);
    assert_eq!(vid("Option", "Some"), VID_SOME);
    assert_eq!(vid("Option", "None"), VID_NONE_VARIANT);
    // A native-built enum (alloc_enum ⇒ Option::Some) must carry the right id.
    let mut vm = Vm::new(Arc::new(program));
    let v = vm.alloc_enum("Option", "Some", vec![Value::int(7)]);
    let Some(h) = v.as_obj() else { panic!() };
    let Obj::Enum { variant_id, .. } = vm.heap.get(h) else {
        panic!("expected enum")
    };
    assert_eq!(
        *variant_id, VID_SOME,
        "native alloc_enum must stamp the fixed Some id"
    );
}

/// M19 lever #2 regression guard — a user enum declaring a variant named `Some` must NOT collapse
/// native Option identity. A native Option::Some (from `pop()`) and a user `Foo::Some` carry
/// DISJOINT ids (native VID_SOME=2 stamped directly by `alloc_enum`; user id at 4..), so `==` is
/// `false` — byte-matching the interp oracle. Before the reserved-id fix the VM printed `true`
/// (user variant shadowed `variants["Some"]`, so native construction stamped the user's id).
#[test]
fn user_variant_shadow_does_not_collapse_native_option_equality() {
    let src = "enum Foo:\n    Some(int)\n    Bar\nfn opt() -> int?:\n    return [5].pop()\nfn main():\n    a := opt()\n    b := Foo.Some(5)\n    print(a == b)\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, "false\n",
        "native Option::Some must not equal user Foo::Some (distinct enums)"
    );
    assert_eq!(
        vm_out, interp_out,
        "vm/interp divergence on shadowed-Some equality"
    );
}

/// Scoped variants — two enums may now reuse a variant name (`Color.Red` / `Light.Red`). Each
/// qualified constructor and match arm must dispatch to the right enum's variant (distinct dense
/// `variant_id`s); the interp's `try_bind` enum check must agree with the VM's int compare.
#[test]
fn shared_variant_name_dispatches_per_enum() {
    let src = "enum Color:\n    Red\n    Blue\nenum Light:\n    Red\n    Green\nfn cname(c: Color) -> str:\n    return match c:\n        Color.Red: \"c-red\"\n        Color.Blue: \"c-blue\"\nfn lname(l: Light) -> str:\n    return match l:\n        Light.Red: \"l-red\"\n        Light.Green: \"l-green\"\nfn main():\n    print(cname(Color.Red))\n    print(lname(Light.Red))\n    print(cname(Color.Blue))\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(vm_out, "c-red\nl-red\nc-blue\n");
    assert_eq!(
        vm_out, interp_out,
        "vm/interp divergence on shared variant name dispatch"
    );
}

/// M19 lever #2 regression guard — `?` on a GENUINE native Option must still work when a user enum
/// shadows the `Some` name. `pop()` stamps the fixed VID_SOME directly, so `?`'s `variant_id ==
/// VID_SOME` gate hits even though `variants["Some"]` now resolves to the user variant. Before the
/// fix the VM faulted with `'?' expects Result or Option, found enum`.
#[test]
fn try_operator_works_on_native_option_under_variant_shadow() {
    let src = "enum Foo:\n    Some(int)\n    Bar\nfn first(xs: List[int]) -> int?:\n    v := xs.pop()?\n    return Some(v)\nfn main():\n    print(\"first\", first([10, 20]))\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, "first Some(20)\n",
        "? must unwrap a genuine native Option even under Some shadowing"
    );
    assert_eq!(
        vm_out, interp_out,
        "vm/interp divergence on ? under shadowed Some"
    );
}

/// M19 lever #2 — `Op::MatchArm` carries a compile-time `variant_id: u32` so match dispatch is a
/// pure-int compare (no per-arm variant-name string compare; the JIT jump-table groundwork). Asserts
/// the emitted op for a `match` on a user enum carries the dense id, not VID_NONE.
#[test]
fn match_arm_dispatches_by_variant_id() {
    let src = "enum Color:\n    Red\n    Green\n    Blue\nfn pick(c: Color) -> int:\n    match c:\n        Color.Red: return 0\n        Color.Green: return 1\n        Color.Blue: return 2\nfn main():\n    print(pick(Color.Green))\nmain()\n";
    let tokens = lexer::tokenize(src).expect("lex");
    let module = parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    // ROOT REDESIGN — the enum is keyed by its qualified IDENTITY KEY (`<main>::Color`).
    let green_id = program
        .variants
        .get(&("<main>::Color".to_string(), "Green".to_string()))
        .expect("Green registered")
        .variant_id;
    // The emitted MatchArm for the `Green` arm must carry Green's dense id.
    let found = program.protos.iter().flat_map(|p| p.code.iter()).any(|op| {
            matches!(op, Op::MatchArm { variant, variant_id, .. } if variant == "Green" && *variant_id == green_id)
        });
    assert!(
        found,
        "MatchArm for 'Green' must carry its dense variant_id"
    );
    // And it must select the right arm at runtime.
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, "1\n");
}

/// M19 memory-layout lever #2 golden: `examples/enum_layout.chz` exercises every observable
/// surface the variant-id change touches — nullary + payload variants, a generic `Option[T]`/
/// `Result[T,E]`, exhaustive `match`, match-with-binding, a guard, enum `==` (equal /
/// unequal-by-variant / unequal-by-enum-type), Display + `${}` interpolation, nested enums, and
/// `Result`/`Option` + the `?` operator. Byte-identical on the VM, interp, the `--parallel`
/// engine (an enum crosses a `spawn`+`Channel`), and the checked-in `.expected`.
#[test]
fn golden_enum_layout_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/enum_layout.chz");
    let expected = include_str!("../../examples/enum_layout.expected");
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from enum_layout.expected"
    );
    assert_eq!(
        vm_out, interp_out,
        "vm/interp divergence on enum_layout (variant-id must be behavior-preserving)"
    );
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("parallel run"),
        "parallel engine diverged on enum_layout (wire/snap variant-id rebuild)"
    );
}

/// Enum-methods golden: `examples/enum_methods.chz` (a `match self` method, a method returning a
/// new variant, a generic enum method using `T`, an enum `str(self)` satisfying Stringable, and an
/// enum satisfying `Add`/`Comparable` through both a generic bound and direct `+`/`<`/`==`)
/// byte-identical on the VM, the interpreter, the parallel engine, and its `.expected`.
#[test]
fn enum_with_hash_is_usable_as_map_set_key() {
    // Regression: an enum defining `hash(self) -> int` satisfies Hashable at the CHECKER, so
    // `Set[E]`/`Map[E,V]` type-check — but both engines must also DISPATCH the enum's hash at
    // runtime (previously they raised "enum is not hashable", crashing a check-clean program).
    let src = "enum Color:\n    Red\n    Green\n    Blue\n\
                   \n    fn hash(self) -> int:\n        match self:\n            Color.Red: return 1\n            Color.Green: return 2\n            Color.Blue: return 3\n\
                   s := {Color.Red, Color.Green, Color.Red}\n\
                   print(s.len())\n\
                   m := {Color.Blue: \"b\"}\n\
                   print(m[Color.Blue])\n";
    assert_eq!(run_parity(src), "2\nb\n");
}

#[test]
fn golden_enum_methods_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/enum_methods.chz");
    let expected = include_str!("../../examples/enum_methods.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out, expected,
        "vm output drifted from enum_methods.expected"
    );
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("interp run"),
        "vm/interp divergence on enum_methods (enum methods must be behavior-preserving)"
    );
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("parallel run"),
        "parallel engine diverged on enum_methods"
    );
}

/// Literals golden: `examples/literals.chz` (scientific-notation floats, `\u{…}` unicode
/// escapes, single-quote strings incl. interpolation) byte-identical on the VM, interp, and
/// `.expected`. Proves the new lexer forms lex the same for both engines (parity by construction).
#[test]
fn golden_literals_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/literals.chz");
    let expected = include_str!("../../examples/literals.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Large-integral float golden: `examples/float_large_integral.chz`. Floats print with CPython
/// `repr()`/`str()` parity (docs/syntax.md contract): scientific notation once the decimal exponent
/// is `>= 16` (`1.5e23` → `1.5e+23`), fixed with a `.0` below that. Exercises BOTH the engine
/// `format_float` path (bare interpolation) and `fmtspec::repr_float` (bare format-spec, no type
/// char) — now the same `repr_float` helper — and asserts VM==interp==`.expected` (parity is the M19 bar).
#[test]
fn golden_float_large_integral_matches_expected_and_interp() {
    let src = include_str!("../../examples/float_large_integral.chz");
    let expected = include_str!("../../examples/float_large_integral.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// bytes golden: `examples/bytes.chz` (the full operation table — `b"..."` literal with `\xHH`
/// escapes, index→int, slice→bytes incl. reverse/step/negative, for-loop→int, len, ==/!=, a map
/// key, an out-of-range index under `recover:`, and the `b'...'` Display/str()/interp repr)
/// byte-identical on the VM, the interpreter, and its `.expected` (three-engine parity gate).
#[test]
fn golden_bytes_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/bytes.chz");
    let expected = include_str!("../../examples/bytes.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// conversions golden: `examples/conversions.chz` (str.encode()/bytes.decode()/bytearray.decode()
/// UTF-8 round-trip incl. a multi-byte char, an invalid-UTF-8 decode under `recover:`, List() over
/// every for-iterable shape — list/set/str/bytes/bytearray/range/user-iterator — Set() dedup, and
/// Map() from 2-tuples with a last-wins dup key) byte-identical on the VM, the interpreter, and its
/// `.expected` (three-engine parity gate).
#[test]
fn golden_conversions_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/conversions.chz");
    let expected = include_str!("../../examples/conversions.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// encode/decode UTF-8 round-trip (multi-byte char): `str.encode()` then `bytes.decode()` returns
/// the original string, byte-identical on the VM + interp.
#[test]
fn encode_decode_roundtrip_multibyte() {
    let src = "fn main():\n    s := \"héllo\"\n    print(s.encode().decode())\n    print(s.encode().decode() == s)\nmain()\n";
    let out = run(src);
    assert_eq!(out, "héllo\ntrue\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp run"));
}

/// `bytearray.decode()` mirrors `bytes.decode()` exactly (decode the current buffer).
#[test]
fn bytearray_decode_matches_bytes() {
    let src = "fn main():\n    print(bytearray([104, 105]).decode())\n    print(b\"hi\".decode())\nmain()\n";
    let out = run(src);
    assert_eq!(out, "hi\nhi\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp run"));
}

/// Invalid UTF-8 `decode()` is a RECOVERABLE fault (catchable by `recover:`), not a panic — and the
/// error message is byte-identical between the engines (parity).
#[test]
fn invalid_utf8_decode_recoverable() {
    let src = "fn main():\n    r := recover:\n        b\"\\xff\\xfe\".decode()\n    match r:\n        Ok(v): print(v)\n        Err(e): print(\"caught\")\nmain()\n";
    let out = run(src);
    assert_eq!(out, "caught\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp run"));
    // Uncaught, the same fault propagates as a recoverable RuntimeError with the same message.
    let bare = "fn main():\n    print(b\"\\xff\".decode())\nmain()\n";
    let (_, vm_res) = run_program(bare);
    let (_, it_res) = run_program_parallel(bare);
    assert_eq!(vm_res.unwrap_err().message, "invalid UTF-8 in decode()");
    assert_eq!(it_res.unwrap_err().message, "invalid UTF-8 in decode()");
}

/// `List()`/`Set()`/`Map()` over a user `.next()` iterator + dedup + last-wins, byte-identical
/// VM/interp. The map last-wins on a duplicate key mirrors the `{k: v}` literal.
#[test]
fn constructors_over_user_iterator_and_dupkey() {
    let src = "struct C:\n    n: int\n    limit: int\n    fn next(self) -> Option[int]:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return Some(v)\nfn main():\n    print(List(C(0, 4)).sum())\n    print(Set(C(0, 4)).len())\n    m := Map([(1, \"a\"), (1, \"b\")])\n    print(m.len())\n    print(m[1])\nmain()\n";
    let out = run(src);
    assert_eq!(out, "6\n4\n1\nb\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp run"));
}

/// bytearray golden: `examples/bytearray.chz` (the full mutable-buffer table — all 4 constructor
/// forms, index read + WRITE, out-of-range write under `recover:`, slice→bytearray incl.
/// reverse/step, for-loop→int, push/pop/extend, len, ==/cross bytes==bytearray, shared-mutation
/// through two bindings, the bytes<->bytearray conversion round-trip, and the `bytearray(b'...')`
/// repr) byte-identical on the VM, the interpreter, and its `.expected` (three-engine parity gate).
#[test]
fn golden_bytearray_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/bytearray.chz");
    let expected = include_str!("../../examples/bytearray.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Qualified enum-variant access golden: `examples/enum_qualified.chz` (the dotted `Enum.Variant`
/// spelling, nullary + payload, in construction AND `match` arms, interleaved with the bare form
/// and a generic enum) byte-identical on the VM, the interpreter, and its `.expected`. Pins that
/// the qualifier is a pure spelling aid resolving to the same variant on both engines.
#[test]
fn golden_enum_qualified_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/enum_qualified.chz");
    let expected = include_str!("../../examples/enum_qualified.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Parity edge: a top-level `fn` named like an enum must NOT shadow qualified-variant access
/// `Enum.Variant` (the checker and VM ignore functions in the precedence gate; the interp must
/// too). Both engines must agree — previously the interp diverged ("cannot read field of
/// function") while the VM constructed the variant.
#[test]
fn qualified_variant_not_shadowed_by_function_parity() {
    let src =
        "enum Color:\n    Red\n    Green\nfn Color() -> int:\n    return 5\nprint(Color.Red)\n";
    let vm_out = run_capture(src).expect("vm run");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        vm_out, interp_out,
        "engines diverged on fn-vs-qualified-variant"
    );
    assert_eq!(vm_out, "Red\n");
}

/// Polymorphic method-IC golden: `examples/poly_method.chz` (a `List[Shape]` walked at one
/// `.area()` call site across FIVE distinct struct types — four fill the N-way method-call IC,
/// the fifth overflows it to the sticky-generic slow path) byte-identical on the VM, the
/// interpreter, and its `.expected`. Pins that the N-way IC is behavior-preserving (the interp,
/// which has no IC, is the oracle).
#[test]
fn golden_poly_method_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/poly_method.chz");
    let expected = include_str!("../../examples/poly_method.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// or-pattern golden: `examples/match_or.chz` (or-patterns with + without bindings, a 3-variant
/// enum or-pattern that is exhaustive without `_`, a guard on an or-pattern, and nested nullary
/// variants `Some(None)` / `Ok(Err(e))`) byte-identical on the VM, the interpreter, the
/// `--parallel` engine, and its `.expected`.
#[test]
fn golden_match_or_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/match_or.chz");
    let expected = include_str!("../../examples/match_or.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel run"));
}

/// QoL golden: `examples/membership.chz` (the `in` operator across list/set/map-key/substring,
/// true + false cases) byte-identical on the VM, the interpreter, the `--parallel` engine, and
/// its `.expected`.
#[test]
fn golden_membership_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/membership.chz");
    let expected = include_str!("../../examples/membership.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel run"));
}

/// QoL golden: `examples/compound_assign.chz` (the 8 compound-assign ops across var/index/field/
/// map-value targets) byte-identical on the VM, the interpreter, the `--parallel` engine, and
/// its `.expected`.
#[test]
fn golden_compound_assign_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/compound_assign.chz");
    let expected = include_str!("../../examples/compound_assign.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel run"));
}

/// Concurrency demo golden: `examples/demo_spawn.chz` (`spawn` in a `parallel:` nursery, results
/// collected over a `Channel` and summed — order-independent, so byte-identical on the VM, the
/// interpreter, the `--parallel` engine, and its `.expected`). Its twin is `demo_executor`.
#[test]
fn golden_demo_spawn_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/demo_spawn.chz");
    let expected = include_str!("../../examples/demo_spawn.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel run"));
}

/// Concurrency demo golden: `examples/demo_executor.chz` (the `Executor` twin of `demo_spawn` —
/// detached `submit` + `shutdown` drain producing the same summed result). Byte-identical on the
/// VM, the interpreter, the `--parallel` engine, and its `.expected`.
#[test]
fn golden_demo_executor_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/demo_executor.chz");
    let expected = include_str!("../../examples/demo_executor.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel run"));
}

/// QoL golden: `examples/multiline_str.chz` (triple-quoted strings — unescaped quotes, `\n`,
/// literal newlines, interpolation) byte-identical on the VM, the interpreter, the `--parallel`
/// engine, and its `.expected`. (Lexer-only feature; parity is by construction.)
#[test]
fn golden_multiline_str_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/multiline_str.chz");
    let expected = include_str!("../../examples/multiline_str.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel run"));
}

/// QoL golden: `examples/tuple_swap.chz` (multi-target assignment — vars, list elements, struct
/// fields, three-way rotation, and `a, b = f()` tuple destructuring; RHS-first eval proven by
/// same-index swaps) byte-identical on the VM, the interpreter, the `--parallel` engine, and its
/// `.expected`.
#[test]
fn golden_tuple_swap_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/tuple_swap.chz");
    let expected = include_str!("../../examples/tuple_swap.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel run"));
}

/// M8-M4 golden: `examples/set.chz` (the set type — literals, membership, algebra, iteration)
/// byte-identical on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_set_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/set.chz");
    let expected = include_str!("../../examples/set.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `timer(ms)` golden: a one-shot timeout channel delivers `true`. Byte-identical on the
/// cooperative VM, the interpreter, and `.expected` (both inline-sleep to the deadline). `--parallel`
/// §6d golden: `examples/wait_select.chz` (Chezzi's `select` — source-order priority, `else:`,
/// a `timer` arm, `=` assignment, and a skipped closed+empty arm). Uses only non-blocking arms so
/// the VM, the interpreter, AND `--parallel` are byte-identical (a truly-blocking `wait` is a
/// VM/cooperative capability tested separately in `vm_wait_blocks_then_wakes_on_second_channel`).
#[test]
fn golden_wait_select_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/wait_select.chz");
    let expected = include_str!("../../examples/wait_select.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(run_capture_parallel(src).expect("parallel"), expected);
}

/// delivers the same value via the background timer `send` (asserted separately).
#[test]
fn golden_timer_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/timer.chz");
    let expected = include_str!("../../examples/timer.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(run_capture_parallel(src).expect("parallel"), expected);
}

/// Phase 4e — the selective `import timer from std.time` form still licenses the bare `timer(ms)`
/// call (opcode-backed, kept via the minimal std.time `native_module_sig` arm) after std.time went
/// file-backed. Byte-identical on the VM, the interpreter, and the M:N `--parallel` engine.
#[test]
fn golden_timer_selective_import_three_engine() {
    let src = "import timer from std.time\nfn main():\n    print(timer(20).recv())\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "true\n");
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(run_capture_parallel(src).expect("parallel"), "true\n");
}

/// `timer(ms)` under `--parallel`: a spawned fiber recv-blocks on the timeout channel, PARKS, and
/// is woken by the background timer `send` at the deadline — not a false deadlock (the pending
/// timer is accounted as `inflight`, vetoing the predicate while the lone fiber waits). Proves the
/// async-delivery path + the deadlock veto + the park/wake on the timer channel's key.
#[test]
fn parallel_timer_wakes_blocked_recv() {
    let src = "fn waiter(t: Channel[bool]):\n    print(t.recv())\n\
                   fn main():\n    parallel:\n        spawn waiter(timer(20))\nmain()\n";
    assert_eq!(run_capture_parallel(src).expect("parallel"), "true\n");
}

/// `Atomic[T]` golden: single-thread load/store/add/sub/exchange/cas sequence, byte-identical on
/// the VM, the interpreter, and `.expected`.
#[test]
fn golden_atomic_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/atomic.chz");
    let expected = include_str!("../../examples/atomic.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Multi-line literals golden: `examples/multiline_literals.chz` (newline/indent suppression
/// inside `[]`/`{}`/`()` + optional trailing commas on list/map/tuple/call/params) byte-identical
/// on the VM, interp, and `.expected`.
#[test]
fn golden_multiline_literals_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/multiline_literals.chz");
    let expected = include_str!("../../examples/multiline_literals.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Format-spec parity: every supported `{expr:spec}` case (align/fill/width/zero-pad/precision/
/// each type char/percent/sign/string-truncate, plus a bare float and a `:`-inside-index) must
/// be byte-identical across the VM, the interpreter, and `--parallel`.
#[test]
fn fmt_specs_parity() {
    let src = "\
m := {\"a:b\": 7}
print(\"[{m[\\\"a:b\\\"]:>4}]\")
print(\"|{42:>6}|{42:<6}|{42:^6}|\")
print(\"z={42:06}|{-7:06}\")
print(\"f={3.14159:.2f}|e={2.5:e}|p={0.1357:.1%}\")
print(\"x={255:x}|X={255:X}|b={255:b}|o={255:o}\")
print(\"s={5:+d}|{-5:+d}\")
print(\"bare={5.0}|fmt={5.0:.2f}|w={5.0:>8}\")
";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("interp run"),
        "interp parity"
    );
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("parallel run"),
        "parallel parity"
    );
}

/// Regression: an interpolated ternary `{if b: a else: b}` has top-level colons that are NOT a
/// format-spec separator — it must run (not be mis-split), and a parenthesized ternary CAN carry
/// a spec. Byte-identical across the VM, the interpreter, and `--parallel`.
#[test]
fn fmt_interpolated_ternary_parity() {
    let src = "\
b := true
print(\"val={if b: 10 else: 20}\")
print(\"fmt={(if b: 1 else: 2):>5}\")
";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "val=10\nfmt=    1\n");
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("interp run"),
        "interp parity"
    );
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("parallel run"),
        "parallel parity"
    );
}

/// A pathological field width is rejected (with the cap message) BEFORE any allocation — the
/// fix for the prior OOM. Must error identically on both engines (it is a compile-time error on
/// the VM path, a runtime error on the interpreter; the message string is the same).
#[test]
fn pathological_width_rejected() {
    let src = "x := 1\nprint(\"{x:>100000000}\")\n";
    let vm_err = run_capture(src).expect_err("vm must reject pathological width");
    assert!(
        vm_err.to_string().contains("exceeds maximum 4096"),
        "vm: {vm_err}"
    );
    let interp_err = run_capture_parallel(src).expect_err("interp must reject");
    assert!(
        interp_err.to_string().contains("exceeds maximum 4096"),
        "interp: {interp_err}"
    );
}

/// Golden: `examples/format_specs.chz` (the full format mini-language) byte-identical on the VM,
/// the interpreter, `--parallel`, and its `.expected`.
#[test]
fn golden_format_specs_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/format_specs.chz");
    let expected = include_str!("../../examples/format_specs.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("parallel run"));
}

/// Slicing golden: `examples/slicing.chz` (list/str slicing + the `Index`/`IndexSet`/`Slice`
/// protocols on a struct + a generic over both) byte-identical on the VM, interp, and `.expected`.
#[test]
fn golden_slicing_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/slicing.chz");
    let expected = include_str!("../../examples/slicing.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `range` golden: `examples/range_step.chz` (3-arg up/down/by-N, empty / wrong-direction cases,
/// the unchanged 1/2-arg forms, and slicing a `..` range literal with a `::step`) byte-identical
/// on the VM, the interpreter, and `.expected` — the two-engine parity guard for stepped ranges.
#[test]
fn golden_range_step_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/range_step.chz");
    let expected = include_str!("../../examples/range_step.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `print` kwargs golden: `examples/print_kwargs.chz` (default form, `end=""`, `sep=`, both, and
/// runtime str exprs) byte-identical on the VM, the interpreter, and `.expected` — the two-engine
/// parity guard for `print`'s `sep=`/`end=`.
#[test]
fn golden_print_kwargs_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/print_kwargs.chz");
    let expected = include_str!("../../examples/print_kwargs.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// First-class universe builtin fn golden: `examples/defer_builtin_value.chz` — `defer print(...)`
/// as a bare first-class call, plus value-position use (`f := ord`/`chr`, `p := panic` raising
/// through the value call path). Byte-identical on the cooperative VM, the M:N OS-thread engine,
/// the interpreter, and `.expected` — the parity guard across all engines.
#[test]
fn golden_defer_builtin_value_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/defer_builtin_value.chz");
    let expected = include_str!("../../examples/defer_builtin_value.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("mn run"));
}

/// Phase 3a golden: `examples/native_prelude.chz` exercises the eight universe builtins now
/// DECLARED in std/prelude.chz (int/float/str/bytes/bytearray ctors, ord/chr fns, panic in a
/// recover:) plus synthetic `print` with sep=/end=. Byte-identical on the cooperative VM, the M:N
/// OS-thread engine, the interpreter, and `.expected` — the migration must change no output.
#[test]
fn golden_native_prelude_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/native_prelude.chz");
    let expected = include_str!("../../examples/native_prelude.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("mn run"));
}

/// `panic` AS A VALUE (`p := panic; p("boom")`) raises the recoverable `RuntimeError` through the
/// value call path — NOT a returned value / NOT silent nil — byte-identical on VM + interp. If the
/// value path returned `Ok`, an uncaught `p(...)` would fall through instead of faulting.
#[test]
fn panic_as_value_uncaught_raises_both_engines() {
    let src = "fn main():\n    p := panic\n    p(\"boom\")\nmain()\n";
    let vm_err = run_capture(src).expect_err("vm: panic value must fault");
    let it_err = run_capture_parallel(src).expect_err("interp: panic value must fault");
    assert_eq!(vm_err.message, "boom");
    assert_eq!(it_err.message, "boom");
}

/// `ord`/`chr` as VALUES (`f := ord`) called back yield the same result as a direct call, VM +
/// interp identical.
#[test]
fn ord_chr_as_value_both_engines() {
    let src =
        "fn main():\n    f := ord\n    g := chr\n    print(f(\"a\"))\n    print(g(66))\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "97\nB\n");
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Regression (bugs 1 & 3): a user binding named like a first-class builtin fn SHADOWS the builtin
/// in value position on BOTH engines — the compiler resolved `LoadBuiltin` before local/global
/// binding lookup (and the interp returned `Value::Builtin` before `env.get`), so a shadowed name
/// type-checked as the binding but printed `<builtin fn …>` at runtime. Param, global (`:=`), and
/// loop-var shadows must each resolve to the BINDING, byte-identical VM == interp.
#[test]
fn user_binding_shadows_firstclass_builtin_both_engines() {
    // parameter shadow
    let a = "fn f(ord: int):\n    print(ord)\nf(42)\n";
    assert_eq!(run_capture(a).expect("vm"), "42\n");
    assert_eq!(
        run_capture(a).expect("vm"),
        run_capture_parallel(a).expect("interp")
    );
    // top-level global (`:=`) shadow, read in value position
    let b = "chr := \"hello\"\nx := chr\nprint(x)\n";
    assert_eq!(run_capture(b).expect("vm"), "hello\n");
    assert_eq!(
        run_capture(b).expect("vm"),
        run_capture_parallel(b).expect("interp")
    );
    // loop-variable shadow
    let c = "for chr in [\"a\", \"b\"]:\n    print(chr)\n";
    assert_eq!(run_capture(c).expect("vm"), "a\nb\n");
    assert_eq!(
        run_capture(c).expect("vm"),
        run_capture_parallel(c).expect("interp")
    );
}

/// Bug 2 / bug 4: two first-class builtin-fn VALUES compare by NAME, byte-identical VM == interp.
/// Each value-position use emits a fresh `Op::LoadBuiltin` → a distinct `Obj::Builtin` handle, so
/// the VM's `values_equal_guarded` (which short-circuits only on `ha == hb`) must have a dedicated
/// `(Obj::Builtin, Obj::Builtin)` name-compare arm; otherwise it falls to the `_ => Ok(false)`
/// catch-all and returns `false` while the interp (derived `PartialEq` on the name) returns
/// `true`. Covers bound values, the bare-name compare, a mismatch, and list-element recursion.
#[test]
fn builtin_value_equality_both_engines() {
    let src = "fn main():\n    f := ord\n    g := ord\n    print(f == g)\n    print(ord == ord)\n    print(chr == ord)\n    print([print] == [print])\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "true\ntrue\nfalse\ntrue\n");
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("mn run"));
}

/// Bug 3: all four first-class builtins are SENDABLE — a builtin bound to a local and CAPTURED
/// into a spawned task crosses the airlock (the `SnapValue::Builtin` path) and runs. Typing them
/// as the dedicated `Ty::BuiltinFn` (sendable), not a plain `Ty::Func` (conservatively
/// non-sendable), removes the asymmetry where only `print` (once typed `Unknown`) could cross.
/// Byte-identical on the cooperative VM, the interp, and the M:N engine.
#[test]
fn builtin_value_sendable_across_airlock_both_engines() {
    let src = "import std.concurrency\nfn main():\n    f := ord\n    p := print\n    parallel:\n        spawn:\n            p(f(\"a\"))\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "97\n");
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("mn run"));
}

/// A first-class builtin fn value spawned as a DIRECT CALL callee (`f := print; spawn f(x)`,
/// distinct from the `spawn:` block form above) must lower + run on all three engines. The
/// callee reaches `prepare_worker`'s `PendingCall::Call` arm, which only handled Closure/Func —
/// a raw `Obj::Builtin` hit the reject `_` on the M:N engine only (`spawn: 'function' is not an
/// isolable task`) while serial/interp dispatched it fine: a three-engine parity divergence on a
/// checker-accepted program. Fixed by the `Lowered::Builtin` arm (cross by name, worker re-allocs).
#[test]
fn spawn_builtin_fn_value_as_call_callee_both_engines() {
    let src = "import std.concurrency\nfn main():\n    g := print\n    parallel:\n        spawn g(\"from-task\")\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "from-task\n");
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("mn run"));
}

/// `spawn print("hi")` — a bare first-class builtin spawned DIRECTLY (no intermediate binding),
/// symmetric with `defer print(...)`. `print` lowers to `Op::LoadBuiltin` (value position) then
/// `SpawnCall`, so the callee is an `Obj::Builtin` crossing the airlock by name on all three
/// engines. The checker gate accepts it (parity-perf-0 fix); runtime prints identically.
#[test]
fn spawn_bare_builtin_print_both_engines() {
    let src = "fn main():\n    parallel:\n        spawn print(\"hi\")\nmain()\n";
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, "hi\n");
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(vm_out, run_capture_parallel(src).expect("mn run"));
}

/// Value-form `print` (`f := print; f(x)`) keeps its (single) arg GC-ROOTED while stringifying —
/// a `Stringable` `str` method runs user code that can `collect()` at a safepoint and would sweep
/// the off-stack arg, a use-after-free. Under `gc_stress` (collect before every instruction) the
/// output must still be correct and match the non-stress run + the interp. (The value form is a
/// fixed 1-arg function; the direct call keeps the variadic/`sep=`/`end=` surface.)
#[test]
fn print_as_value_arg_rooted_under_gc_stress() {
    let src = "struct Loud:\n    n: int\n    fn str(self) -> str:\n        return \"L{self.n}\"\n\
                   fn main():\n    f := print\n    f(Loud(7))\nmain()\n";
    let stressed = run_capture_stress(src);
    assert_eq!(stressed, "L7\n");
    assert_eq!(stressed, run_capture(src).expect("vm run"));
    assert_eq!(stressed, run_capture_parallel(src).expect("interp run"));
}

/// `assert` golden: `examples/assert.chz` (bare + message forms, all passing) byte-identical on
/// the VM, the interpreter, and `.expected` — the two-engine parity guard for the primitive.
#[test]
fn golden_assert_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/assert.chz");
    let expected = include_str!("../../examples/assert.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

// ---- user-callable panic(msg) builtin (VM half + cross-engine parity) ----

/// `panic(msg)` beneath `recover:` materializes as `Err(e)` whose `.message()` == msg on the VM
/// too — byte-identical to the interp (the parity oracle).
#[test]
fn vm_panic_under_recover_yields_err_with_message() {
    let src = "fn main():\n    r := recover:\n        panic(\"boom\")\n    match r:\n        Ok(v): print(\"ok: {v}\")\n        Err(e): print(\"recovered: {e.message()}\")\nmain()\n";
    assert_eq!(run(src), "recovered: boom\n");
    assert_eq!(
        run_capture(src).expect("vm run"),
        run_capture_parallel(src).expect("interp run")
    );
}

/// An uncaught `panic(msg)` aborts with that message (run_capture returns Err), same as overflow.
#[test]
fn vm_panic_uncaught_returns_runtime_error() {
    let src = "fn main():\n    panic(\"kaboom\")\nmain()\n";
    let err = run_capture(src).expect_err("uncaught panic should fault");
    assert_eq!(err.message, "kaboom");
}

/// `defer`s run during the VM panic unwind, identically to the interp.
#[test]
fn vm_panic_runs_defers_during_unwind() {
    let src = "fn log(m: str):\n    print(m)\nfn risky():\n    defer log(\"cleanup ran\")\n    panic(\"kaboom\")\nfn main():\n    r := recover:\n        risky()\n    match r:\n        Ok(v): print(\"ok\")\n        Err(e): print(\"recovered: {e.message()}\")\nmain()\n";
    assert_eq!(run(src), "cleanup ran\nrecovered: kaboom\n");
    assert_eq!(
        run_capture(src).expect("vm run"),
        run_capture_parallel(src).expect("interp run")
    );
}

/// `panic` golden: a `recover:` catching a bare `panic(msg)` as `Err`, a `defer` running during
/// the panic unwind, and the bottom-typed `panic` in if-expression value position. Byte-identical
/// on the VM, the interpreter (parity oracle), and its `.expected`.
#[test]
fn golden_panic_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/panic.chz");
    let expected = include_str!("../../examples/panic.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `defer` golden: LIFO order, method + free-fn calls, the `?` short-circuit path, args evaluated
/// at the defer statement (per-iteration snapshot), the `defer:` block form (in-block order,
/// LIFO-as-a-unit, by-value snapshot at the defer point, `?`-path), defers running before a
/// `recover:` catch, and a fault inside a deferred call. Byte-identical on the VM, the
/// interpreter, and its `.expected`.
#[test]
fn golden_defer_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/defer.chz");
    let expected = include_str!("../../examples/defer.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

// ----- concurrency C4 (VM parity for spawn / parallel: / Channel / Shared) -----

/// C1 golden: `parallel:` nursery + both `spawn` forms run to completion at the dedent (FIFO),
/// the parent resuming only after the join. Byte-identical on the VM, the interpreter, and the
/// `.expected` file.
#[test]
fn golden_parallel_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/parallel.chz");
    let expected = include_str!("../../examples/parallel.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// B3.3-threads sub-step 1: the `--parallel` engine is selectable (`run_capture_parallel` sets
/// `Vm::parallel`). A `parallel:` with task-order output (no cross-task blocking) yields the same
/// result as the cooperative engine — proving the flag plumbs through without changing
/// well-ordered output.
#[test]
fn parallel_engine_runs_simple_program() {
    let src = include_str!("../../examples/parallel.chz");
    let expected = include_str!("../../examples/parallel.expected");
    assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
}

/// M-C golden: implicit nurseries — bare `spawn` at function scope joins at the body's
/// `return`/end; an inner `parallel:` joins earlier at its dedent. Byte-identical on all three
/// engines (cooperative VM, frozen interp, `--parallel`).
#[test]
fn golden_implicit_nursery_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/implicit_nursery.chz");
    let expected = include_str!("../../examples/implicit_nursery.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
    assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
}

/// B3.3-threads golden: N real OS-thread tasks `update` one `Shared[int]` concurrently; the box
/// serialises every write, so the count is exactly the spawn count (lost-update race fixed by the
/// `update_lock`). Deterministic-by-construction (order-independent) — proves the bounded pool +
/// `Shared` cross-thread atomicity. The default cooperative engine runs it too (still `5`).
#[test]
fn golden_parallel_shared_chz_matches_expected() {
    let src = include_str!("../../examples/parallel_shared.chz");
    let expected = include_str!("../../examples/parallel_shared.expected");
    assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
    // Same program on the cooperative default engine is identical (decision A oracle).
    assert_eq!(run_capture(src).expect("vm run"), expected);
}

/// `RwShared[T]` golden: N tasks each `write` a distinct key into one shared `RwShared[map]`
/// (exclusive write lock; the whole RMW is serialised under `--parallel` so no update is lost),
/// the nursery joins, then the parent `read`s the whole map back. Order-independent → identical on
/// the cooperative default engine, the M:N `--parallel` engine, AND the interpreter oracle.
#[test]
fn golden_rwshared_concurrent_matches_expected() {
    let src = include_str!("../../examples/rwshared.chz");
    let expected = include_str!("../../examples/rwshared.expected");
    assert_eq!(run_capture(src).expect("vm run"), expected);
    assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
    assert_eq!(run_capture_parallel(src).expect("interp run"), expected);
}

/// Airlock identity: a `RwShared` mutated from a spawned task is observed by the parent — the
/// handle crosses as a SHARED `Arc` core (not a deep-copy), so both reach the one box.
#[test]
fn rwshared_mutation_from_spawn_is_observed_by_parent() {
    let src = "fn bump(r: RwShared[int]):\n    r.write(fn(x): x + 1)\nfn main():\n    r := RwShared(0)\n    parallel:\n        spawn bump(r)\n    print(r.get())\nmain()\n";
    assert_eq!(run_capture(src).expect("vm run"), "1\n");
    assert_eq!(run_capture_parallel(src).expect("parallel run"), "1\n");
    assert_eq!(run_capture_parallel(src).expect("interp"), "1\n");
}

/// B3.3-threads golden: the collector task `recv`s before any producer runs, so on the real-thread
/// engine it BLOCKS on the channel condvar and is woken by producer `send`s from pool threads.
/// It sorts what it gathers → the printed order is fixed however threads interleave
/// (deterministic-by-construction). Exercises condvar `recv` + flush-on-join.
#[test]
fn golden_parallel_channel_chz_matches_expected() {
    let src = include_str!("../../examples/parallel_channel.chz");
    let expected = include_str!("../../examples/parallel_channel.expected");
    assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
}

/// The "correct pattern" companion to the cross-nursery deadlock: two mutually-dependent
/// blocking tasks as SIBLINGS in one nursery interleave fine (nesting them would deadlock — see
/// `docs/cross-nursery-flat-scheduler.md`). VM-only blocking, so checked on the VM + `--parallel`
/// engines (the frozen interp cannot suspend a cross-fiber `recv`).
#[test]
fn golden_parallel_cross_nursery_ok_chz_matches_expected() {
    let src = include_str!("../../examples/parallel_cross_nursery_ok.chz");
    let expected = include_str!("../../examples/parallel_cross_nursery_ok.expected");
    assert_eq!(run_capture(src).expect("vm run"), expected);
    assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
}

// ---- Channel.close() + closed semantics (both engines) ----

#[test]
fn vm_channel_send_after_close_faults() {
    let err =
        run_err("fn main():\n    ch := Channel[int]()\n    ch.close()\n    ch.send(1)\nmain()\n");
    assert!(err.contains("send on a closed channel"), "{err}");
}

// ----- §6d: `wait` (select) — VM, parity twins of the interp tests + VM-only blocking -----

#[test]
fn vm_wait_picks_first_ready_arm_in_source_order() {
    let src = "fn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    a.send(1)\n    b.send(2)\n    wait:\n        v := a.recv(): print(10 + v)\n        w := b.recv(): print(20 + w)\nmain()\n";
    assert_eq!(run(src), "11\n");
}

#[test]
fn vm_wait_skips_closed_empty_arm() {
    let src = "fn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    a.close()\n    b.send(9)\n    wait:\n        v := a.recv(): print(100)\n        w := b.recv(): print(w)\nmain()\n";
    assert_eq!(run(src), "9\n");
}

#[test]
fn vm_wait_runs_else_when_nothing_ready() {
    let src = "fn main():\n    ch := Channel[int]()\n    wait:\n        v := ch.recv(): print(v)\n        else: print(0)\nmain()\n";
    assert_eq!(run(src), "0\n");
}

#[test]
fn vm_wait_assign_arm_mutates_outer_lvalue() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(5)\n    n := 0\n    wait:\n        n = ch.recv(): print(n)\n    print(n)\nmain()\n";
    assert_eq!(run(src), "5\n5\n");
}

#[test]
fn vm_wait_timer_arm_fires() {
    let src = "fn main():\n    t := timer(1)\n    wait:\n        _ := t.recv(): print(\"tick\")\nmain()\n";
    assert_eq!(run(src), "tick\n");
}

#[test]
fn vm_wait_all_closed_no_else_faults() {
    let err = run_err(
        "fn main():\n    ch := Channel[int]()\n    ch.close()\n    wait:\n        v := ch.recv(): print(v)\nmain()\n",
    );
    assert!(err.contains("all channels closed"), "{err}");
}

/// W7-2 contract fence — the observable an ALL-DEAD `park_wait` requeue lands on is this fault, and
/// it must be byte-identical on both engines for a MULTI-arm wait (the single-arm case above only
/// exercises the poll, never the park-gap re-check).
#[test]
fn vm_wait_all_closed_multi_arm_faults_both_engines() {
    let src = "fn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    a.close()\n    b.close()\n    wait:\n        v := a.recv(): print(v)\n        w := b.recv(): print(w)\nmain()\n";
    let serial = run_err(src);
    assert!(serial.contains("all channels closed"), "{serial}");
    let mn = match run_capture_parallel(src) {
        Ok(o) => panic!("expected a fault, got {o:?}"),
        Err(e) => e.message,
    };
    assert_eq!(serial, mn, "serial vs M:N wait all-closed fault divergence");
}

#[test]
fn vm_wait_live_empty_no_else_top_level_deadlocks() {
    let err = run_err(
        "fn main():\n    ch := Channel[int]()\n    wait:\n        v := ch.recv(): print(v)\nmain()\n",
    );
    assert!(err.contains("deadlock"), "{err}");
}

/// W7-2 anti-detector-weakening fence: a GENUINE all-parked nursery (a `wait:` on live, empty
/// channels with no possible waker) must still be reported promptly as a deadlock on M:N. The W7-2
/// fix must deliver the missing wakeup, never blunt the detector.
#[test]
fn vm_wait_real_deadlock_still_reported_parallel() {
    let src = "fn w(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(v)\n        u := b.recv(): print(u)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn w(a, b)\nmain()\n";
    let mn = match run_capture_parallel(src) {
        Ok(o) => panic!("expected a deadlock fault, got {o:?}"),
        Err(e) => e.message,
    };
    assert!(mn.contains("deadlock"), "{mn}");
}

/// VM-only (the interp would deadlock): a spawned consumer `wait`s on two empty channels and
/// blocks (cooperative multi-channel park); a sibling `send` to the SECOND channel wakes it, and
/// the re-poll takes that arm. Exercises the multi-key park + wake on a non-first channel.
#[test]
fn vm_wait_blocks_then_wakes_on_second_channel() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(\"a {v}\")\n        w := b.recv(): print(\"b {w}\")\nfn producer(b: Channel[int]):\n    b.send(99)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\n        spawn producer(b)\nmain()\n";
    assert_eq!(run(src), "b 99\n");
}

/// The multi-channel park SWEEP: a consumer parks on {a, b}, a `send` to `a` wakes it and it
/// finishes — then a later `send` to `b` must NOT re-wake the now-done fiber (its stale `b`
/// registration was swept on resume). Without the sweep this re-schedules a `Done` fiber → panic.
#[test]
fn vm_wait_sweeps_other_buckets_after_waking() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(v)\n        w := b.recv(): print(w)\nfn p_a(a: Channel[int]):\n    a.send(1)\nfn p_b(b: Channel[int]):\n    b.send(2)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\n        spawn p_a(a)\n        spawn p_b(b)\nmain()\n";
    assert_eq!(run(src), "1\n");
}

/// A `wait` `=` arm to a Field/Index lvalue (the custom `emit_wait_assign` stash-and-reload path):
/// the received value must land in the struct field / list slot, identical to the interp.
#[test]
fn vm_wait_assign_to_field_and_index_matches_interp() {
    let src = "struct Box:\n    v: int\nfn main():\n    ch := Channel[int]()\n    ch.send(7)\n    b := Box(0)\n    wait:\n        b.v = ch.recv(): print(b.v)\n    xs := [0, 0]\n    ch.send(9)\n    wait:\n        xs[1] = ch.recv(): print(xs[1])\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "7\n9\n");
    assert_eq!(vm, run_capture_parallel(src).expect("interp run"));
}

/// A single-arm `wait` reduces to a plain `recv`: ready value taken (the recv-park 1-key special
/// case stays correct — plain `recv` is covered by its own tests, unchanged).
#[test]
fn vm_wait_single_arm_takes_ready_value() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(42)\n    wait:\n        v := ch.recv(): print(v)\nmain()\n";
    assert_eq!(run(src), "42\n");
}

/// A lone blocking `wait` under `--parallel` with NO possible sender is a genuine deadlock — it
/// must fault `deadlock` (NOT hang, NOT "not yet supported"). Proves the M:N wait-park accounts
/// the parked fiber via `parked_n` so the deadlock predicate fires.
#[test]
fn vm_wait_lone_blocked_parallel_deadlocks() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(v)\n        w := b.recv(): print(w)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\nmain()\n";
    let err =
        run_capture_parallel(src).expect_err("lone blocked wait under --parallel must deadlock");
    assert!(err.message.contains("deadlock"), "{}", err.message);
}

/// M:N blocking wait-park (TDD step 4): a spawned consumer `wait`s on two empty channels and parks
/// under `--parallel`; a sibling `send` to the SECOND channel wakes it and the re-poll takes that
/// arm. Asserts the parallel output AND parity with the cooperative VM.
#[test]
fn vm_wait_blocks_then_wakes_on_second_channel_parallel() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(\"a {v}\")\n        w := b.recv(): print(\"b {w}\")\nfn producer(b: Channel[int]):\n    b.send(99)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\n        spawn producer(b)\nmain()\n";
    assert_eq!(
        run_capture_parallel(src).expect("parallel wait park"),
        "b 99\n"
    );
    assert_eq!(run(src), "b 99\n");
}

/// M:N wait-park cross-bucket SWEEP (TDD step 5): a consumer parks on {a,b}; a `send` to one wakes
/// it and it finishes — a later `send` to the OTHER must NOT re-wake the now-Done fiber (its stale
/// token was swept under the sched lock). Without the sweep this re-schedules a Done fiber → panic.
/// Two real-thread producers race, so EITHER arm may win — the sweep's guarantee is structural (no
/// panic, no hang), NOT a specific value; assert the run completes cleanly with a valid value, and
/// loop to exercise both orderings + the post-done send. The cooperative engine is deterministic
/// (source order ⇒ "1\n"); only `--parallel` races.
#[test]
fn vm_wait_sweeps_other_buckets_after_waking_parallel() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(v)\n        w := b.recv(): print(w)\nfn p_a(a: Channel[int]):\n    a.send(1)\nfn p_b(b: Channel[int]):\n    b.send(2)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\n        spawn p_a(a)\n        spawn p_b(b)\nmain()\n";
    for _ in 0..20 {
        // `.expect` catches a panic-to-fault ("scheduled a Done fiber") or a hang-then-deadlock; the
        // membership check tolerates the genuine producer race.
        let out = run_capture_parallel(src).expect("parallel wait sweep must not panic or hang");
        assert!(
            out == "1\n" || out == "2\n",
            "unexpected sweep output: {out:?}"
        );
    }
    assert_eq!(run(src), "1\n"); // cooperative engine is deterministic (source-order poll)
}

/// M:N wait-park: a wait-parked fiber with a LIVE sibling that will send must take the arm and
/// print — NOT fault deadlock (the live sibling vetoes the predicate). Parity with cooperative VM.
#[test]
fn vm_wait_sibling_send_vetoes_deadlock_parallel() {
    let src = "fn consumer(a: Channel[int], b: Channel[int]):\n    wait:\n        v := a.recv(): print(\"got {v}\")\n        w := b.recv(): print(\"got {w}\")\nfn producer(a: Channel[int]):\n    a.send(5)\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn consumer(a, b)\n        spawn producer(a)\nmain()\n";
    assert_eq!(
        run_capture_parallel(src).expect("live sibling vetoes deadlock"),
        "got 5\n"
    );
    assert_eq!(run(src), "got 5\n");
}

/// WAIT-1 (HIGH) regression — a `wait` over a long timer arm + a data channel under `--parallel`
/// must NOT pin the worker on the inline-sleep and unconditionally take the timer. A sibling
/// `send` landing mid-window (after a tiny `sleep_ms`) must win the channel arm. Pre-fix the
/// 2000ms inline-sleep strands the send and the timer arm fires (prints "timeout"). Looped to
/// catch any straggler ordering. `--parallel`-only (timer+wait is observably nondeterministic, so
/// no two-engine golden).
#[test]
fn vm_wait_timer_loses_to_midwindow_send_parallel() {
    let src = "fn consumer(ch: Channel[int], t: Channel[bool]):\n    wait:\n        v := ch.recv(): print(\"got {v}\")\n        _ := t.recv(): print(\"timeout\")\nfn producer(ch: Channel[int]):\n    d := timer(5)\n    _ := d.recv()\n    ch.send(7)\nfn main():\n    ch := Channel[int]()\n    t := timer(2000)\n    parallel:\n        spawn consumer(ch, t)\n        spawn producer(ch)\nmain()\n";
    for _ in 0..20 {
        assert_eq!(
            run_capture_parallel(src).expect("mid-window send must win the channel arm"),
            "got 7\n",
            "timer arm took the wait instead of the mid-window send (WAIT-1)"
        );
    }
}

/// Companion to the WAIT-1 fix: a short timer with NO sender must still fire its arm and the run
/// must COMPLETE (no hang). Proves the timed-park files the timer channel in the park set and the
/// deadline `send_wake` claims the fiber. A fix that forgot the timer arm in the park set would
/// hang; the `inflight` veto keeps the deadlock predicate quiet while the lone fiber waits.
#[test]
fn vm_wait_short_timer_fires_when_no_send_parallel() {
    let src = "fn consumer(ch: Channel[int], t: Channel[bool]):\n    wait:\n        v := ch.recv(): print(\"got {v}\")\n        _ := t.recv(): print(\"timeout\")\nfn main():\n    ch := Channel[int]()\n    t := timer(5)\n    parallel:\n        spawn consumer(ch, t)\nmain()\n";
    assert_eq!(
        run_capture_parallel(src).expect("short timer must fire its arm, not hang"),
        "timeout\n"
    );
}

/// Re-park with a live timer arm: a sibling `close`s the channel arm mid-window, which wakes the
/// waiting fiber WITHOUT a consumable value, so it re-runs WaitPoll and re-enters the snapshot-park
/// block while the timer is still pending. The timer must still fire correctly ("timeout") and the
/// run must COMPLETE — no hang. This path (a timer-armed wait that re-parks) was previously
/// untested; it also guards the arm-once `timer_armed` CAS latch against ever wedging the wake.
#[test]
fn vm_wait_timer_arm_survives_channel_close_repark_parallel() {
    let src = "fn consumer(ch: Channel[int], t: Channel[bool]):\n    wait:\n        v := ch.recv(): print(\"got {v}\")\n        _ := t.recv(): print(\"timeout\")\nfn closer(ch: Channel[int]):\n    d := timer(5)\n    _ := d.recv()\n    ch.close()\nfn main():\n    ch := Channel[int]()\n    t := timer(40)\n    parallel:\n        spawn consumer(ch, t)\n        spawn closer(ch)\nmain()\n";
    for _ in 0..20 {
        assert_eq!(
            run_capture_parallel(src)
                .expect("timer arm must fire after a mid-window close; no hang"),
            "timeout\n",
            "a channel close that re-parks the wait stranded the timer arm"
        );
    }
}

// ----- §6d edge cases: control flow + nesting + spawn inside `wait` arm bodies (VM == interp) -----

/// A `wait` arm body that `break`s out of an enclosing loop (compiled via `compile_defer_scoped_arm`,
/// the same path as a `match` arm). VM == interp.
#[test]
fn vm_wait_arm_break_in_loop() {
    let src = "fn main():\n    found := -1\n    i := 0\n    while i < 3:\n        a := Channel[int]()\n        a.send(i * 10)\n        wait:\n            v := a.recv():\n                if v == 10:\n                    found = v\n                    break\n        i += 1\n    print(found)\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "10\n");
    assert_eq!(vm, run_capture_parallel(src).expect("interp"));
}

/// A `wait` arm body that `continue`s the enclosing loop.
#[test]
fn vm_wait_arm_continue_in_loop() {
    let src = "fn main():\n    total := 0\n    i := 0\n    while i < 3:\n        a := Channel[int]()\n        a.send(i)\n        i += 1\n        wait:\n            v := a.recv():\n                if v == 1:\n                    continue\n                total += v\n    print(total)\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "2\n");
    assert_eq!(vm, run_capture_parallel(src).expect("interp"));
}

/// A `wait` arm body that `return`s a value from the enclosing function.
#[test]
fn vm_wait_arm_return() {
    let src = "fn pick(a: Channel[int]) -> int:\n    wait:\n        v := a.recv():\n            return v * 100\nfn main():\n    a := Channel[int]()\n    a.send(7)\n    print(pick(a))\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "700\n");
    assert_eq!(vm, run_capture_parallel(src).expect("interp"));
}

/// A `wait` arm body containing ANOTHER `wait` (nested select).
#[test]
fn vm_wait_nested() {
    let src = "fn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    a.send(1)\n    b.send(2)\n    wait:\n        v := a.recv():\n            wait:\n                w := b.recv(): print(v + w)\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "3\n");
    assert_eq!(vm, run_capture_parallel(src).expect("interp"));
}

/// A `wait` inside a loop that blocks, wakes, re-iterates, and blocks again under `--parallel`: the
/// consumer parks on each iteration until the producer feeds it. VM == interp == --parallel.
#[test]
fn vm_wait_in_loop_reparks_parallel() {
    let src = "fn consumer(ch: Channel[int], done: Channel[int]):\n    sum := 0\n    i := 0\n    while i < 3:\n        wait:\n            v := ch.recv(): sum += v\n        i += 1\n    done.send(sum)\nfn producer(ch: Channel[int]):\n    ch.send(10)\n    ch.send(20)\n    ch.send(30)\nfn main():\n    ch := Channel[int]()\n    done := Channel[int]()\n    parallel:\n        spawn consumer(ch, done)\n        spawn producer(ch)\n    print(done.recv())\nmain()\n";
    assert_eq!(run(src), "60\n");
    assert_eq!(
        run_capture_parallel(src).expect("parallel repark loop"),
        "60\n"
    );
}

/// A `wait` arm body containing a bare `spawn` (exercises `block_has_bare_spawn`'s recursion into
/// wait arms — the body opens an implicit nursery that joins at function return). VM == interp.
#[test]
fn vm_wait_arm_bare_spawn() {
    let src = "fn worker():\n    print(\"worker ran\")\nfn main():\n    a := Channel[int]()\n    a.send(1)\n    wait:\n        v := a.recv():\n            spawn worker()\n    print(\"after wait\")\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "after wait\nworker ran\n");
    assert_eq!(vm, run_capture_parallel(src).expect("interp"));
}

/// §6d regression — a multi-arm `wait` whose arm body references an OUTER local inside a fused
/// binop (`x + w`): the peephole fuses `GetLocal+GetLocal+Add` → `BinLocalLocal`, shifting the
/// arm-body indices, so `WaitPoll.arm_targets` must be relocated (else the wake jumps PAST the
/// bind prologue → VM 65, interp 66). Pins VM == interp on every arm.
#[test]
fn vm_wait_arm_body_outer_local_in_binop_matches_interp() {
    let src = "fn pick(a: Channel[int], b: Channel[int], x: int) -> int:\n    wait:\n        v := a.recv(): return x + v\n        w := b.recv(): return x + w\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    b.send(65)\n    print(pick(a, b, 1))\nmain()\n";
    let vm = run(src);
    assert_eq!(vm, "66\n");
    assert_eq!(vm, run_capture_parallel(src).expect("interp"));
}

/// 1-key regression PIN (TDD step 1): an ordinary blocking `recv` (NOT a `wait`) under
/// `--parallel`, woken by a sibling `send`, still works byte-identically — this guards the
/// `SchedCore.parked` refactor (`Vec<Fiber>` → `Vec<ParkedEntry>`) from regressing the recv park.
/// Asserts both the parallel output AND parity with the cooperative VM.
#[test]
fn vm_wait_single_arm_recv_park_unchanged_under_parallel() {
    let src = "fn consumer(a: Channel[int]):\n    v := a.recv()\n    print(\"got {v}\")\nfn producer(a: Channel[int]):\n    a.send(7)\nfn main():\n    a := Channel[int]()\n    parallel:\n        spawn consumer(a)\n        spawn producer(a)\nmain()\n";
    assert_eq!(
        run_capture_parallel(src).expect("parallel recv park"),
        "got 7\n"
    );
    assert_eq!(run(src), "got 7\n");
}

#[test]
fn vm_channel_recv_on_closed_empty_faults() {
    let err = run_err(
        "fn main():\n    ch := Channel[int]()\n    ch.close()\n    print(ch.recv())\nmain()\n",
    );
    assert!(err.contains("receive on a closed channel"), "{err}");
}

#[test]
fn vm_channel_drains_buffered_after_close() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    ch.close()\n    print(ch.recv())\n    print(ch.recv())\nmain()\n";
    assert_eq!(run(src), "1\n2\n");
    assert_eq!(run(src), run_capture_parallel(src).expect("interp"));
}

#[test]
fn vm_channel_try_send_false_when_closed() {
    let src = "fn main():\n    ch := Channel[int]()\n    print(ch.try_send(1))\n    ch.close()\n    print(ch.try_send(2))\nmain()\n";
    assert_eq!(run(src), "true\nfalse\n");
    assert_eq!(run(src), run_capture_parallel(src).expect("interp"));
}

#[test]
fn vm_channel_double_close_ok() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.close()\n    ch.close()\n    print(1)\nmain()\n";
    assert_eq!(run(src), "1\n");
}

#[test]
fn vm_channel_close_then_len_zero() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.close()\n    print(ch.len())\nmain()\n";
    assert_eq!(run(src), "0\n");
    assert_eq!(run(src), run_capture_parallel(src).expect("interp"));
}

#[test]
fn vm_channel_try_recv_closed_empty_is_none() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.close()\n    match ch.try_recv():\n        Some(v): print(v)\n        None: print(\"none\")\nmain()\n";
    assert_eq!(run(src), "none\n");
    assert_eq!(run(src), run_capture_parallel(src).expect("interp"));
}

#[test]
fn vm_for_over_channel_drains_then_exits() {
    // Producer-first (no concurrency needed): the channel is closed+full before the `for` runs.
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    ch.send(3)\n    ch.close()\n    total := 0\n    for v in ch:\n        total = total + v\n    print(total)\nmain()\n";
    assert_eq!(run(src), "6\n");
    assert_eq!(run(src), run_capture_parallel(src).expect("interp"));
}

/// `--parallel`: a `for v in ch:` consumer that runs ahead of the producer PARKS on the empty
/// channel; a sibling `close()` (no value sent) wakes it and the loop ends cleanly (0 iterations).
#[test]
fn parallel_close_wakes_parked_receiver() {
    let src = "\
fn produce(ch: Channel[int]):
    ch.close()
fn consume(ch: Channel[int], out: Channel[int]):
    n := 0
    for v in ch:
        n = n + 1
    out.send(n)
fn main():
    ch := Channel[int]()
    out := Channel[int]()
    parallel:
        spawn consume(ch, out)
        spawn produce(ch)
    print(out.recv())
main()
";
    assert_eq!(run_capture_parallel(src).expect("parallel run"), "0\n");
}

/// `--parallel`: a single `close()` must wake EVERY receiver parked on the channel (not just one,
/// as a `send` would). Three consumers each loop-then-report; all three exit and report.
#[test]
fn parallel_close_wakes_multiple_receivers() {
    let src = "\
fn consume(ch: Channel[int], done: Channel[int]):
    for v in ch:
        n := v
    done.send(1)
fn main():
    ch := Channel[int]()
    done := Channel[int]()
    parallel:
        spawn consume(ch, done)
        spawn consume(ch, done)
        spawn consume(ch, done)
        spawn:
            ch.close()
    total := 0
    for i in 0..3:
        total = total + done.recv()
    print(total)
main()
";
    assert_eq!(run_capture_parallel(src).expect("parallel run"), "3\n");
}

/// `--parallel`: a consumer that loops `recv` past the producer's last value used to deadlock-fault;
/// with `for v in ch:` + a producer `close()` it drains the buffered values and exits cleanly.
#[test]
fn parallel_consumer_loop_terminates_on_close() {
    let src = "\
fn produce(ch: Channel[int]):
    for i in 1..6:
        ch.send(i)
    ch.close()
fn consume(ch: Channel[int], out: Channel[int]):
    total := 0
    for v in ch:
        total = total + v
    out.send(total)
fn main():
    ch := Channel[int]()
    out := Channel[int]()
    parallel:
        spawn produce(ch)
        spawn consume(ch, out)
    print(out.recv())
main()
";
    // 1+2+3+4+5 = 15, however the producer/consumer interleave.
    assert_eq!(run_capture_parallel(src).expect("parallel run"), "15\n");
}

/// Channel.close() golden: producer sends a run + `close()`s; consumer `for v in ch:` drains then
/// ends cleanly. VM cooperative == interp == expected (decision A oracle), and the `--parallel`
/// engine (consumer parks on the empty channel, woken by the producer's send/close) prints the
/// same total. Pins the headline `for`-over-channel + `try_send`-after-close surface on all engines.
#[test]
fn golden_parallel_channel_close_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/parallel_channel_close.chz");
    let expected = include_str!("../../examples/parallel_channel_close.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

#[test]
fn golden_parallel_channel_close_chz_parallel_engine() {
    let src = include_str!("../../examples/parallel_channel_close.chz");
    let expected = include_str!("../../examples/parallel_channel_close.expected");
    assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
}

#[test]
fn parallel_send_after_close_faults() {
    let src = "\
fn main():
    ch := Channel[int]()
    parallel:
        spawn:
            ch.close()
            ch.send(1)
main()
";
    let err = run_capture_parallel(src).expect_err("send after close should fault");
    assert!(
        err.message.contains("send on a closed channel"),
        "{}",
        err.message
    );
}

/// B3.6 golden: `Executor` tasks run on the bounded pool. Three submitted closures capture the
/// result `Channel` (sendable → crosses as a shared `Arc`) and `send` from pool threads; `shutdown`
/// drains them onto the pool and joins, then main sorts what it gathered → fixed printed order
/// however threads interleave. The cooperative default engine runs it too (decision A oracle: same
/// output, inline drain), proving the `submit`-by-value / pool-drain change is observationally inert.
#[test]
fn golden_executor_pool_chz_matches_expected() {
    let src = include_str!("../../examples/executor_pool.chz");
    let expected = include_str!("../../examples/executor_pool.expected");
    assert_eq!(run_capture_parallel(src).expect("parallel run"), expected);
    assert_eq!(run_capture(src).expect("vm run"), expected);
}

/// D1 (lazy module snapshot): a `--parallel` task that calls a sibling free function which in
/// turn reads a module-level global must resolve both against the worker's *own* home module —
/// exercising lazy fault-in of that module into the worker heap on first global access. The
/// cooperative default engine is the equivalence oracle (same output). Characterization test:
/// green before D1 (eager `build_worker_modules`) and after (lazy snapshot).
#[test]
fn parallel_task_resolves_sibling_fn_and_global() {
    let src = "\
G := 100
fn helper(x: int) -> int:
    return x + G
fn send_one(ch: Channel[int], x: int):
    ch.send(helper(x))
fn main():
    ch := Channel[int]()
    parallel:
        spawn send_one(ch, 1)
        spawn send_one(ch, 2)
    a := ch.recv()
    b := ch.recv()
    print(a + b)
main()
";
    assert_eq!(run_capture_parallel(src).expect("parallel run"), "203\n");
    assert_eq!(run_capture(src).expect("vm run"), "203\n");
}

/// D1 (lazy module snapshot): many trivial `--parallel` spawns no longer pay a full
/// per-task module-graph rebuild. Correctness gate — every one of N tasks reaches the same
/// `Shared` box, so the serialised count is exactly N. A *loose* wall-clock ceiling is a smoke
/// guard that the per-task O(graph) reconstruction is gone (kept generous to avoid CI flake; the
/// real perf delta is shown via `primes_parallel` timing in the milestone verification).
#[test]
fn parallel_many_spawns_cheap_and_correct() {
    const N: usize = 2000;
    let mut src = String::from(
        "fn bump(s: Shared[int]):\n    s.update(fn(x): x + 1)\nfn main():\n    s := Shared(0)\n    parallel:\n",
    );
    for _ in 0..N {
        src.push_str("        spawn bump(s)\n");
    }
    src.push_str("    print(s.get())\nmain()\n");
    let start = std::time::Instant::now();
    let out = run_capture_parallel(&src).expect("parallel run");
    let elapsed = start.elapsed();
    assert_eq!(out, format!("{N}\n"));
    assert!(
        elapsed < std::time::Duration::from_secs(30),
        "{N} spawns took {elapsed:?} (>30s ceiling)"
    );
}

/// B3.6: a submitted closure capturing a plain value (`int`) observes it **by value** across the
/// airlock — exercises the `WireValue::Closure` capture round-trip (not just the shared-`Arc` handle
/// path the golden's `Channel` capture takes). Auto-drained at program exit on both engines; the
/// `--parallel` drain reconstructs and runs the closure on the pool.
#[test]
fn executor_submitted_closure_captures_by_value() {
    let src =
        "fn main():\n    n := 7\n    ex := Executor()\n    ex.submit(fn(): print(n))\nmain()\n";
    assert_eq!(run_capture(src).expect("vm run"), "7\n");
    assert_eq!(run_capture_parallel(src).expect("parallel run"), "7\n");
}

/// A submitted closure crosses the airlock **by value** on BOTH engines: it isolates its captures at
/// `submit` time (`wire_callable` → `to_wire` deep-copies), so a mutation of a captured collection
/// after the `submit` is NOT observed by the job. Eager execution shrank that window to nothing on the
/// default engine (the job is already running), but the isolation is what makes the two engines agree
/// here regardless — it has always been keyed to the `submit`, never to when the job runs. The cooperative engine
/// used to share captures by reference (queuing the closure's own `Handle`) to mirror the tree-walk
/// `interp` oracle; that oracle has been removed and serial==M:N is now the sole invariant, so coop
/// isolates identically to M:N — both print `[1]` here (not `[1, 2]`).
#[test]
fn executor_cooperative_submit_isolates_captures_by_value() {
    let src = "fn main():\n    xs := [1]\n    ex := Executor()\n    ex.submit(fn(): print(xs))\n    xs.push(2)\nmain()\n";
    assert_eq!(
        run_capture(src).expect("vm run"),
        "[1]\n",
        "cooperative submit isolates the captured list by value at submit time"
    );
    assert_eq!(
        run_capture_parallel(src).expect("parallel run"),
        "[1]\n",
        "M:N submit isolates the captured list by value — parity with serial"
    );
}

/// B3.3-threads: a nested `parallel:` runs on the same bounded pool without exploding the thread
/// count (each join level adds only its own participating thread). Two outer tasks each spawn two
/// inner tasks, all bumping one `Shared` → `4`, deterministic.
#[test]
fn parallel_nested_nursery_on_pool() {
    let src = "fn inner(s: Shared[int]):\n    s.update(fn(x): x + 1)\n\
                   fn outer(s: Shared[int]):\n    parallel:\n        spawn inner(s)\n        spawn inner(s)\n\
                   fn main():\n    s := Shared(0)\n    parallel:\n        spawn outer(s)\n        spawn outer(s)\n    print(s.get())\nmain()\n";
    assert_eq!(run_capture_parallel(src).expect("parallel run"), "4\n");
}

/// Eager `Executor` — the successor to B3.3-threads' `done_signal_bumps_counter_on_panic`. That test
/// constructed the retired `DoneSignal` guard directly and asserted its `Drop` bumped the batch join's
/// counter on unwind. Eager execution replaced the batch farm/join (`run_workers_on_pool`) with a
/// per-job dispatch, so the invariant is now `EagerState::outstanding` always reaching 0 — and it is
/// testable on the REAL path instead of on a guard in isolation: a job that ends abnormally must still
/// record its outcome, or `shutdown`'s condvar wait never wakes and the program hangs forever.
///
/// The watchdog is the point: without it a regression here HANGS the test binary instead of failing it
/// (the failure mode of the whole eager milestone), so assert on a bounded `recv_timeout`.
#[test]
fn executor_faulting_job_does_not_hang_shutdown() {
    let src = "import std.concurrency\n\
               ex := Executor()\n\
               ex.submit(fn(): panic(\"boom\"))\n\
               r := recover: ex.shutdown()\n\
               match r:\n    \
                   Ok(_): print(\"no fault\")\n    \
                   Err(e): print(\"caught: {e.message()}\")\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    let got = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("shutdown() must not hang when a job ends abnormally");
    assert_eq!(got.unwrap(), "caught: boom\n");
}

/// B3.3-threads (decision F, review coverage gap): each worker buffers its own stdout and the join
/// flushes them **in task order** — so three concurrently-run tasks that each `print` produce a
/// deterministic, task-ordered transcript regardless of thread interleaving.
#[test]
fn parallel_output_flushes_in_task_order() {
    let src = "fn emit(s: str):\n    print(s)\n\
                   fn main():\n    parallel:\n        spawn emit(\"alpha\")\n        spawn emit(\"beta\")\n        spawn emit(\"gamma\")\nmain()\n";
    assert_eq!(
        run_capture_parallel(src).expect("parallel run"),
        "alpha\nbeta\ngamma\n"
    );
}

/// B3.3-threads: a fault in a **pool** task (not the inline task[0]) propagates out of the join as
/// the nursery's error after all siblings finish (sibling-abort is B3.4; here we join-then-report
/// the first fault). The ok task and the faulting task are independent (no channel) so there is no
/// deadlock without cancellation.
#[test]
fn parallel_pool_task_fault_propagates() {
    let src = "fn ok_task(s: Shared[int]):\n    s.update(fn(x): x + 1)\n\
                   fn boom():\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    s := Shared(0)\n    parallel:\n        spawn ok_task(s)\n        spawn boom()\n    print(\"unreached\")\nmain()\n";
    let err = run_capture_parallel(src).expect_err("expected the pool task fault to propagate");
    assert!(
        err.message.contains("out of bounds"),
        "got: {}",
        err.message
    );
}

/// gap #2: a plain `return` inside a `parallel:` body jumps past `JoinNursery`, so the nursery is
/// never popped at the dedent. Both engines truncate `self.nurseries` back to the frame's entry
/// depth (VM via `do_return`/`drain_escaped_nursery`; interp via `exec_parallel`'s unconditional
/// pop), or the stale nursery leaks. TASK B: the escape now also CANCELS-AND-REPORTS the unstarted
/// `noop()` — one report line precedes the early-return value. White-box residual-depth check +
/// VM/interp parity. A subsequent `parallel:` runs on a clean stack (its empty join is silent).
#[test]
fn parallel_return_escape_leaves_clean_nursery_stack() {
    let src = "fn noop():\n    0\n\
                   fn worker() -> int:\n    parallel:\n        spawn noop()\n        return 5\n    99\n\
                   fn main():\n    print(worker())\n    parallel:\n        spawn noop()\nmain()\n";
    let (vm_out, nursery_depth) = run_capture_nursery_len(src);
    let vm_out = vm_out.expect("vm run");
    // `worker`'s parallel: escapes via `return` with one pending task → cancel+report, then `5`.
    // `main`'s trailing parallel: dedents normally to its join (NOT an escape), so it runs `noop()`
    // silently — no report, proving a later parallel: still works on the reclaimed stack.
    let report = crate::runtime::pending_cancel_report(1);
    assert_eq!(
        vm_out,
        format!("{report}5\n"),
        "early return wins; only the escaped nursery reports"
    );
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("interp run"),
        "VM/interp parity"
    );
    assert_eq!(
        nursery_depth, 0,
        "the return-escaped nursery must be reclaimed, not leaked"
    );
}

/// gap #2, second escape form: an uncaught `?` that propagates out of the frame (no `recover:`
/// between the `parallel:` and the program top) must also reclaim the skipped nursery via
/// `do_return`. The whole program faults, but the run-so-far must leave no leaked nursery. TASK B:
/// the unstarted `noop()` is cancelled-and-reported — one report line is on stdout before the fault
/// (interp already dropped on `?`; both engines now emit the report identically).
#[test]
fn parallel_try_escape_leaves_clean_nursery_stack() {
    let src = "fn noop():\n    0\n\
                   fn boom() -> int!:\n    return Err(\"x\")\n\
                   fn main() -> int!:\n    parallel:\n        spawn noop()\n        y := boom()?\n        print(y)\n    Ok(0)\nmain()\n";
    let (vm_out, nursery_depth) = run_capture_nursery_len(src);
    assert!(vm_out.is_err(), "the uncaught ? faults the program");
    assert_eq!(
        nursery_depth, 0,
        "the ?-escaped nursery must be reclaimed, not leaked"
    );
    // Cancel-and-report: stdout-so-far is exactly the report line, identical across engines.
    let report = crate::runtime::pending_cancel_report(1);
    let (vm_so_far, vm_res) = run_program(src);
    assert!(vm_res.is_err());
    assert_eq!(vm_so_far, report, "VM: one report line before the fault");
    let (interp_so_far, interp_res) = run_program_parallel(src);
    assert!(interp_res.is_err());
    assert_eq!(interp_so_far, vm_so_far, "interp/VM stdout-so-far parity");
}

/// gap #2, boundary: a `?` inside a `parallel:` that IS caught by a same-frame `recover:` must
/// stay on the **existing** handler-catch reclaim (`Handler::nursery_len`), NOT the new
/// `do_return` truncate — the two paths are mutually exclusive in `do_try` (recover-scoped `?`
/// jumps to the handler and never calls `do_return`). TASK B: that recover-catch reclaim site now
/// routes through `drain_escaped_nursery`, so a recover-caught `?` cancels-and-reports the unstarted
/// `noop()` IDENTICALLY to an uncaught `?` — one report line precedes "recovered". Asserts the two
/// reclaim paths don't fight: the recovered program continues, a later `parallel:` runs, stack clean.
#[test]
fn parallel_try_caught_by_recover_leaves_clean_nursery_stack() {
    let src = "fn noop():\n    0\n\
                   fn boom() -> int!:\n    return Err(\"x\")\n\
                   fn main():\n    r := recover:\n        parallel:\n            spawn noop()\n            y := boom()?\n            print(y)\n        0\n    print(\"recovered\")\n    parallel:\n        spawn noop()\nmain()\n";
    let (vm_out, nursery_depth) = run_capture_nursery_len(src);
    let vm_out = vm_out.expect("the ? is caught by recover, so the program completes");
    // The recover-caught `?` cancels its one pending task and reports, THEN the recover continues.
    // `main`'s trailing parallel: joins normally (not an escape) → silent.
    let report = crate::runtime::pending_cancel_report(1);
    assert_eq!(
        vm_out,
        format!("{report}recovered\n"),
        "recover swallows the fault; cancel+report precedes it"
    );
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("interp run"),
        "VM/interp parity"
    );
    assert_eq!(
        nursery_depth, 0,
        "the recover-caught nursery is reclaimed via the handler path"
    );
}

/// gap #2, ordering boundary: a recover-scoped `?` escaping a `parallel:` whose BODY has a
/// `defer` must order the cancel-report AFTER the parallel-body defer and BEFORE the recover
/// continues — matching the interp oracle, whose `exec_parallel` reports only after the body's
/// `exec_scoped_block` has drained its defers. Regression for the do_try report-before-body-defer
/// divergence (the report previously trailed the parallel-body defer on the VM). Body-defer →
/// report → recovered, byte-identical across interp / VM-cooperative / VM-`--parallel`.
#[test]
fn parallel_recover_scoped_try_orders_report_after_body_defer() {
    let src = "fn noop():\n    0\n\
                   fn pdefer():\n    print(\"PDEFER\")\n\
                   fn boom() -> int!:\n    return Err(\"x\")\n\
                   fn main():\n    r := recover:\n        parallel:\n            defer pdefer()\n            spawn noop()\n            y := boom()?\n            print(y)\n        0\n    print(\"recovered\")\nmain()\n";
    let report = crate::runtime::pending_cancel_report(1);
    let expected = format!("PDEFER\n{report}recovered\n");
    let interp_out = run_capture_parallel(src).expect("interp run");
    assert_eq!(
        interp_out, expected,
        "interp oracle: body-defer precedes report precedes recover"
    );
    let (vm_out, nursery_depth) = run_capture_nursery_len(src);
    let vm_out = vm_out.expect("the ? is caught by recover, so the program completes");
    assert_eq!(
        vm_out, expected,
        "VM cooperative: report ordered after the parallel-body defer"
    );
    assert_eq!(
        nursery_depth, 0,
        "the recover-caught nursery is reclaimed, not leaked"
    );
    assert_eq!(
        run_capture_parallel(src).expect("--parallel run"),
        expected,
        "VM --parallel parity"
    );
}

// ----- TASK B: pending-spawn-drop on early `parallel:` escape → cancel-and-report -----
// Policy: an UNSTARTED spawn task on a `parallel:` that escapes early (`?`/`return`/`break`)
// before its join is CANCELLED (not run), and ONE report line is written to stdout (`out`,
// the stream every `run_capture*` harness reads), byte-identical across interp / VM-cooperative
// / VM-`--parallel`. The escape propagates unchanged; the nursery stack stays leak-free (depth 0).
//
// NB: the spawned task's side effect is observed via `print` (a true cross-airlock observable),
// NOT a `Shared[int]` counter — `spawn` DEEP-CLONES the box across the airlock, so a run task
// mutates a COPY and the parent's `s.get()` stays 0 whether or not the task ran. A `print` in the
// spawned body is the only reliable run-vs-cancelled signal.

/// `?` escape: the spawned `side()` MUST NOT run (no "SIDE RAN") and the cancellation report IS
/// emitted before the fault unwinds. White-box: nursery depth returns to 0 (no leak). The interp
/// already DROPPED on `?` (it never diverged on this kind), so here all three only gain the report.
#[test]
fn parallel_try_escape_cancels_pending_and_reports() {
    let src = "fn side():\n    print(\"SIDE RAN\")\n\
                   fn boom() -> int!:\n    return Err(\"x\")\n\
                   fn main() -> int!:\n    parallel:\n        spawn side()\n        y := boom()?\n        print(y)\n    Ok(0)\nmain()\n";
    let report = crate::runtime::pending_cancel_report(1);
    // The `?` faults the whole program, but the report is on stdout captured so far.
    let (vm_out, depth) = run_capture_nursery_len(src);
    assert!(vm_out.is_err(), "the uncaught ? faults the program");
    assert_eq!(
        depth, 0,
        "the ?-escaped nursery must be reclaimed, not leaked"
    );
    // Stdout captured up to the fault: exactly the cancellation report, no `side()` output.
    let (vm_so_far, vm_res) = run_program(src);
    assert!(vm_res.is_err());
    assert_eq!(
        vm_so_far, report,
        "VM cooperative: report present, task NOT run"
    );
    // Interp parity (oracle): identical stdout-so-far + identical error class.
    let (interp_so_far, interp_res) = run_program_parallel(src);
    assert!(interp_res.is_err());
    assert_eq!(interp_so_far, vm_so_far, "interp/VM stdout-so-far parity");
    // --parallel parity: same fault.
    assert!(run_capture_parallel(src).is_err(), "--parallel also faults");
}

/// `return` escape: the spawned `side()` is CANCELLED (not run) and the report is emitted; the
/// early `return` value still wins. Pre-fix the interp RAN the task here (printed "SIDE RAN") while
/// the VM dropped it — the live divergence this fixes. Identical text across engines, depth 0.
#[test]
fn parallel_return_escape_cancels_pending_and_reports() {
    let src = "fn side():\n    print(\"SIDE RAN\")\n\
                   fn worker() -> int:\n    parallel:\n        spawn side()\n        return 5\n    99\n\
                   fn main():\n    print(worker())\nmain()\n";
    let report = crate::runtime::pending_cancel_report(1);
    // The early return wins (5); `side()` never runs (no "SIDE RAN"); the report is emitted at the
    // escape (inside `worker`, before its caller prints the result).
    let expected = format!("{report}5\n");
    let (vm_out, depth) = run_capture_nursery_len(src);
    assert_eq!(
        vm_out.as_deref().map(str::to_string),
        Ok(expected.clone()),
        "VM cooperative"
    );
    assert_eq!(
        depth, 0,
        "the return-escaped nursery must be reclaimed, not leaked"
    );
    assert_eq!(
        run_capture_parallel(src).expect("interp run"),
        expected,
        "interp parity"
    );
    assert_eq!(
        run_capture_parallel(src).expect("parallel run"),
        expected,
        "--parallel parity"
    );
}

/// `break`-in-loop escape: the NET-NEW VM site (a `break` that leaves a `parallel:` scope via the
/// in-frame loop-exit Jump, NOT via `do_return`). The spawned `side()` is cancelled + reported on
/// the iteration that breaks; the loop exits, the function continues. Pre-fix the interp RAN the
/// task ("SIDE RAN") while the VM dropped it. Identical across engines, depth 0.
#[test]
fn parallel_break_escape_cancels_pending_and_reports() {
    let src = "fn side():\n    print(\"SIDE RAN\")\n\
                   fn main():\n    for i in 0..3:\n        parallel:\n            spawn side()\n            if i == 0:\n                break\n            print(\"unreached\")\n    print(\"done\")\nmain()\n";
    let report = crate::runtime::pending_cancel_report(1);
    // i==0: spawn side(), then break out of the `parallel:` scope before the join → cancel+report,
    // exit the loop. `side()` never runs (no "SIDE RAN"). "unreached" never prints.
    let expected = format!("{report}done\n");
    let (vm_out, depth) = run_capture_nursery_len(src);
    assert_eq!(
        vm_out.as_deref().map(str::to_string),
        Ok(expected.clone()),
        "VM cooperative (net-new break site)"
    );
    assert_eq!(
        depth, 0,
        "the break-escaped nursery must be reclaimed, not leaked"
    );
    assert_eq!(
        run_capture_parallel(src).expect("interp run"),
        expected,
        "interp parity"
    );
    assert_eq!(
        run_capture_parallel(src).expect("parallel run"),
        expected,
        "--parallel parity"
    );
}

/// B3.4: a `recv`-blocked sibling must ABORT when a sibling faults before sending, instead of
/// hanging the join forever. `boom` is spawned first so it runs inline on the joining thread —
/// it faults immediately and trips the nursery cancel flag without depending on pool scheduling
/// (avoids the G3 pool-starvation hazard on low-core CI). `consumer` runs on the pool, blocks on
/// the empty channel, and its re-checking `recv` wait observes the cancel and unwinds — so the
/// join completes and reports the producer's fault rather than deadlocking.
#[test]
fn parallel_recv_blocked_sibling_aborts_on_sibling_fault() {
    let src = "fn boom(ch: Channel[int]):\n    xs := [1]\n    print(xs[9])\n\
                   fn consumer(ch: Channel[int]):\n    ch.recv()\n    print(\"consumed\")\n\
                   fn main():\n    ch := Channel[int]()\n    parallel:\n        spawn boom(ch)\n        spawn consumer(ch)\nmain()\n";
    let err =
        run_capture_parallel(src).expect_err("expected the producer fault to propagate, not hang");
    assert!(
        err.message.contains("out of bounds"),
        "got: {}",
        err.message
    );
}

/// B3.4: a CPU-bound sibling aborts mid-flight when a sibling faults, observing the cancel flag
/// at a dispatch back-edge. `looper` runs inline (task[0]); it writes `1`, hands `trigger` a
/// channel token (so the fault happens-after `looper` has started — no timing race), then spins
/// and would write `99` only after the (huge) loop. `trigger` (pool) waits for the token, then
/// faults → trips cancel → `looper` aborts mid-loop. Asserting `1` proves `looper` started AND
/// was cancelled before completing; without the back-edge cancel check it would print `99`.
#[test]
fn parallel_cpu_sibling_aborts_on_sibling_fault() {
    let src = "fn looper(go: Channel[int], s: Shared[int]):\n    s.set(1)\n    go.send(0)\n    i := 0\n    while i < 1000000000:\n        i = i + 1\n    s.set(99)\n\
                   fn trigger(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn looper(go, s)\n            spawn trigger(go)\n        0\n    print(s.get())\nmain()\n";
    let out = run_capture_parallel(src).expect("the fault is recovered, so the program completes");
    assert_eq!(
        out, "1\n",
        "looper started (1) but was cancelled before completing (never wrote 99)"
    );
}

/// B3.4: `defer` still composes with cancellation. The blocked consumer is aborted when the
/// producer faults; its `defer cleanup(s)` must still run on the cancel unwind (writing the
/// shared sentinel), proving deferred calls fire even on a cancelled task.
///
/// Synchronized like [`parallel_cpu_sibling_aborts_on_sibling_fault`]: `boom` waits for a token
/// `consumer` sends only AFTER it has registered its `defer` and is about to block, so the fault
/// happens-after the defer is registered. (Under the M:N engine task start order is not
/// deterministic, so a fault that races a not-yet-started sibling would legitimately skip its
/// not-yet-registered defer, per Go semantics; this token makes the intended "blocked consumer is
/// aborted" scenario the one actually exercised.)
///
/// The token closes ONLY the defer-REGISTRATION race — it does NOT make this test race-free (an
/// earlier version of this comment wrongly claimed "no timing race"). The surviving race was
/// scheduler-side and AFTER the fault: the cancel trip and its `cancel_drain` sit two core-lock
/// acquisitions apart, and an idle worker's deadlock check landing in that gap used to reap the
/// still-parked consumer as `Deadlocked`, dropping it WITHOUT `unwind_deferred` — so this printed
/// `0` roughly 35/200 runs under CPU contention. That is closed by the cancel-teardown veto in
/// `is_deadlocked` (see `SchedCore::any_incomplete_scope_cancelled`, src/vm/mod.rs). This test is
/// the REGRESSION guard for it: a failure here means the veto is gone, not that the token is flaky.
#[test]
fn parallel_defer_runs_on_cancelled_sibling() {
    let src = "fn cleanup(s: Shared[int]):\n    s.set(42)\n\
                   fn consumer(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
                   fn boom(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    s := Shared(0)\n    r := recover:\n        ch := Channel[int]()\n        go := Channel[int]()\n        parallel:\n            spawn consumer(ch, go, s)\n            spawn boom(go)\n        0\n    print(s.get())\nmain()\n";
    let out = run_capture_parallel(src)
        .expect("the producer fault is recovered, so the program completes");
    assert_eq!(
        out, "42\n",
        "the cancelled consumer's defer ran on the unwind"
    );
}

/// Cancellation points: a task spinning in an UNBOUNDED `while true:` loop must still be cancelled
/// promptly when a sibling faults — that is exactly what the loop BACK-EDGE checkpoint is for
/// (`Vm::jump_checked`). A regression (e.g. only one of the two `Op::Jump` dispatch sites wired)
/// HANGS the nursery here instead of failing an assert.
#[test]
fn parallel_spinning_sibling_does_not_hang_the_nursery_under_cancel() {
    let src = "fn spinner(go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    i := 0\n    while true:\n        i = i + 1\n\
                   fn cleanup(s: Shared[int]):\n    s.set(42)\n\
                   fn trigger(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn spinner(go, s)\n            spawn trigger(go)\n        0\n    print(s.get())\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the spinning sibling must be cancelled at a loop back-edge, not hang the nursery")
        .expect("the trigger fault is recovered, so the program completes");
    assert_eq!(out, "42\n", "the spinner was cancelled and ran its defer");
}

/// N4 (second seam — `abort_enlisted_scope`): a cancelled task's `defer` must run when the cancel
/// comes from an EARLY-ENLISTED outer nursery whose body ESCAPES past its join (here: a nested
/// `parallel:` inside the outer body → `early_enlist_outer` seeds the outer scope's tasks as live
/// fibers and marks it `awaiting_builder`; the `return` then escapes → `drain_escaped_nursery` →
/// `abort_enlisted_scope`). That seam used to CLEAR the `awaiting_builder` deadlock veto before it
/// tripped the scope cancel (which arms the cancel-teardown veto), leaving a window with NEITHER: an
/// idle worker's `take_runnable` in that window saw the quiesce (the inline builder is not counted in
/// `running`), declared a spurious DEADLOCK, and `flag_deadlock` DROPPED the parked `task` fiber
/// without `unwind_deferred` — its `defer` never ran, invisibly (`abort_enlisted_scope` discards the
/// reduce, so not even the bogus `Deadlocked` surfaced). Fixed by tripping the cancel FIRST (a gapless
/// veto handoff — `MnSched::trip_scope_cancel`, under the core lock).
///
/// `gate` (the nested nursery's task) consumes the token `task` sends only AFTER registering its
/// `defer`, so the inner join — and therefore the escape — happens-after the defer is registered.
/// Natural window: `cleanup=0` ~1/100 under CPU contention; with a 30ms sleep probing the gap it was
/// 20/20 pre-fix and 0/20 after. M:N only (`run_capture_parallel`): the serial engine never STARTS a
/// lazy nursery's pending task before the escape, so no `defer` is registered there at all and it
/// prints `0` — a pre-existing engine divergence in what a not-yet-started task does (gaps.md N6), not
/// this bug.
#[test]
fn parallel_defer_runs_when_enlisted_nursery_escapes() {
    let src = "fn cleanup(s: Shared[int]):\n    s.set(42)\n\
                   fn task(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    ch.recv()\n\
                   fn gate(go: Channel[int]):\n    go.recv()\n\
                   fn outer(ch: Channel[int], go: Channel[int], s: Shared[int]):\n    parallel:\n        spawn task(ch, go, s)\n        parallel:\n            spawn gate(go)\n        return\n\
                   fn main():\n    ch := Channel[int]()\n    go := Channel[int]()\n    s := Shared(0)\n    outer(ch, go, s)\n    print(s.get())\nmain()\n";
    let out = run_capture_parallel(src).expect("the escape is not a fault — the program completes");
    assert_eq!(
        out, "42\n",
        "the enlisted scope's cancelled task unwound through its defer (no spurious deadlock reaped it)"
    );
}

/// N4 (panic-fault seam): a worker-VM PANIC (a VM bug, a panicking native/FFI callback) becomes a task
/// `Fault` via `run_one_fiber`'s `catch_unwind` fallback — which NEVER reaches `classify_mn_outcome`,
/// the only other place that trips the scope cancel. Without a trip here the scope aborts with
/// `cancel == false`: `cancel_drain` requeues the parked siblings, they re-run `recv`, `park`'s gap
/// re-check sees no cancel and PARKS THEM AGAIN, the scope quiesces uncancelled, `is_deadlocked` fires
/// (by its own rules — nothing is cancelled), and `flag_deadlock` drops them without `unwind_deferred`
/// — their `defer`s silently skipped, hidden behind the panic-fault (`reduce_task_slots` ranks Fault >
/// Deadlocked). Unit-level because there is no Chezzi-source way to panic a worker VM (every runtime
/// error is an `Err(RuntimeError)`; even integer overflow is a clean fault).
#[test]
fn panic_fault_trips_the_scope_cancel() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let cancel = Arc::new(AtomicBool::new(false));
    vm.cancel = Some(Arc::clone(&cancel)); // as `run_one_fiber`'s swap-in re-points it, per fiber scope
    let out = vm.panic_outcome(Box::new("worker boom"), Span::RUNTIME);
    assert!(
        matches!(out, TaskOutcome::Fault { .. }),
        "a panicking task is a Fault"
    );
    assert!(
        cancel.load(Ordering::Relaxed),
        "a panic-fault must trip its scope's cancel — siblings unwind (and run their defers) only if it does"
    );
}

/// B3.4: `defer` runs even when a task is cancelled at the CPU **back-edge** (not only on the
/// recv path). `worker` (inline) registers `defer cleanup(s)`, signals `trigger` it has started,
/// then spins; cancelled mid-loop, the unwind must run the defer (writing 42). Regression guard:
/// a raw `return Err` from the loop top would bypass the defer machinery — this asserts the
/// cancel unwinds through `unwind_deferred`. (Without the fix this prints `0`.)
#[test]
fn parallel_defer_runs_on_back_edge_cancel() {
    let src = "fn cleanup(s: Shared[int]):\n    s.set(42)\n\
                   fn worker(go: Channel[int], s: Shared[int]):\n    defer cleanup(s)\n    go.send(0)\n    i := 0\n    while i < 1000000000:\n        i = i + 1\n\
                   fn trigger(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn worker(go, s)\n            spawn trigger(go)\n        0\n    print(s.get())\nmain()\n";
    let out = run_capture_parallel(src)
        .expect("the trigger fault is recovered, so the program completes");
    assert_eq!(
        out, "42\n",
        "the CPU-cancelled worker's defer ran on the back-edge unwind"
    );
}

/// B3.4: cancellation is NOT catchable by a `recover:` inside a worker — a cancelled task must
/// die, not resume. `victim` (inline) writes `1`, signals it has started, then wraps a long loop
/// in `recover:` and would write `99` after it. If the cancel sentinel were an ordinary catchable
/// fault, the inner `recover:` would swallow it and `victim` would reach `s.set(99)`; the bypass
/// unwinds past the recover instead, so the sentinel stays at the pre-loop `1`. (Buggy: `99`.)
#[test]
fn parallel_recover_inside_worker_does_not_catch_cancel() {
    let src = "fn victim(go: Channel[int], s: Shared[int]):\n    s.set(1)\n    go.send(0)\n    r := recover:\n        i := 0\n        while i < 1000000000:\n            i = i + 1\n        0\n    s.set(99)\n\
                   fn trigger(go: Channel[int]):\n    go.recv()\n    xs := [1]\n    print(xs[9])\n\
                   fn main():\n    go := Channel[int]()\n    s := Shared(0)\n    r := recover:\n        parallel:\n            spawn victim(go, s)\n            spawn trigger(go)\n        0\n    print(s.get())\nmain()\n";
    let out = run_capture_parallel(src)
        .expect("the trigger fault is recovered, so the program completes");
    assert_eq!(
        out, "1\n",
        "victim's inner recover must NOT catch the cancel; it never reaches s.set(99)"
    );
}

/// C2 golden: `Channel[T]` fan-out — workers `send` at the dedent, the parent `recv`s after the
/// join. Byte-identical to the `.expected` file on the cooperative VM (deterministic FIFO in spawn
/// order); on the M:N engine the three workers `send` concurrently, so the queued strings arrive in
/// EITHER order — the line SET is identical, asserted order-insensitively (like `golden_try_recv`).
#[test]
fn golden_channel_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/channel.chz");
    let expected = include_str!("../../examples/channel.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_same_lines(&vm_out, &run_capture_parallel(src).expect("M:N run"));
}

/// `Atomic[int].add` cross-thread atomicity: N real-OS-thread fibers each `add(1)` one shared
/// box; the join sum must be exactly N (no lost read-modify-write — the whole point of `Atomic`).
#[test]
fn parallel_atomic_add_is_exact() {
    let n = 300;
    let src = format!(
        "fn work(a: Atomic[int]):\n    a.add(1)\n\
             fn main():\n    a := Atomic(0)\n    parallel:\n        for _ in 0..{n}:\n            spawn work(a)\n    print(a.load())\nmain()\n"
    );
    assert_eq!(
        run_capture_parallel(&src).expect("parallel"),
        format!("{n}\n")
    );
}

/// `Atomic[int].cas` under contention: N fibers each increment via a load-then-CAS retry loop. A
/// lost CAS (the box changed under us) retries, so the serialised total is exactly N — proving the
/// compare-and-swap is atomic across threads.
#[test]
fn parallel_atomic_cas_increment_is_exact() {
    let n = 200;
    let src = format!(
        "fn bump(a: Atomic[int]):\n    while true:\n        cur := a.load()\n        if a.cas(cur, cur + 1):\n            break\n\
             fn main():\n    a := Atomic(0)\n    parallel:\n        for _ in 0..{n}:\n            spawn bump(a)\n    print(a.load())\nmain()\n"
    );
    assert_eq!(
        run_capture_parallel(&src).expect("parallel"),
        format!("{n}\n")
    );
}

/// `Atomic[float]` add/sub/exchange/cas must behave identically on both engines (covers the
/// numeric-`T` arm for floats, not just ints).
#[test]
fn atomic_float_ops_two_engine_parity() {
    let src = "fn main():\n    a := Atomic(1.5)\n    print(a.add(2.0))\n    print(a.sub(0.5))\n    print(a.exchange(9.0))\n    print(a.cas(9.0, 4.0))\n    print(a.load())\nmain()\n";
    assert_eq!(
        run_capture(src).expect("vm"),
        run_capture_parallel(src).expect("interp")
    );
}

/// `cas` on a non-scalar `T` (a list) exercises the VM's lock-held `from_wire`/`values_equal` path
/// — the most distinctive Atomic code path. Both engines must agree.
#[test]
fn atomic_cas_on_list_two_engine_parity() {
    let src = "fn main():\n    a := Atomic([1, 2])\n    print(a.cas([1, 2], [9]))\n    print(a.load())\n    print(a.cas([1, 2], [0]))\n    print(a.load())\nmain()\n";
    assert_eq!(
        run_capture(src).expect("vm"),
        run_capture_parallel(src).expect("interp")
    );
}

/// `timer(0)` (and any already-elapsed deadline) delivers `true` immediately, on every engine.
#[test]
fn timer_zero_delivers_immediately() {
    let src = "fn main():\n    print(timer(0).recv())\nmain()\n";
    let out = run_capture(src).expect("vm");
    assert_eq!(out, "true\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp"));
    assert_eq!(run_capture_parallel(src).expect("parallel"), "true\n");
}

/// A1 golden: `Channel[T].try_recv()` — a non-blocking poll returning `T?`. Workers `send` at the
/// dedent; the parent drains with `try_recv` (`Some` per value, then `None`). Never blocks/faults,
/// so byte-identical on the VM, the interpreter, and the `.expected` file.
#[test]
fn golden_try_recv_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/try_recv.chz");
    let expected = include_str!("../../examples/try_recv.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    // M:N workers send concurrently, so the parent's try_recv drains them in either order — the set
    // is identical (exact order pinned on the cooperative engine above).
    assert_same_lines(&vm_out, &run_capture_parallel(src).expect("M:N run"));
    assert_eq!(run_capture_stress(src), expected);
}

/// B1+B2 golden (VM-only): blocking `recv`. The consumer is scheduled first, parks on the empty
/// channel, the cooperative scheduler runs the producer, and the consumer resumes to receive. The
/// interpreter still faults `deadlock` on the same program (documented parity gap — see the interp
/// twin `channel_block_chz_faults_deadlock_on_interp`), so this asserts the VM output + `.expected`
/// only, NOT cross-engine parity.
#[test]
fn golden_channel_block_chz_matches_expected() {
    let src = include_str!("../../examples/channel_block.chz");
    let expected = include_str!("../../examples/channel_block.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    // Parked fibers must survive collection: the same program under GC stress is byte-identical.
    assert_eq!(run_capture_stress(src), expected);
}

/// Cross-nursery circular wakeup — the case-A flat-scheduler fix (M:N / `--parallel` ONLY).
/// `inner` spawns a NESTED nursery; the OUTER `parallel:` spawns sibling O and then calls
/// `inner(a, b)`. O recvs a → sends b; inner's spawned fiber sends a → recvs b. Pre-fix this
/// faulted `deadlock` (the inner nursery's inline owner drained only its private queue and could
/// never RUN the outer sibling O). With the M:N flat scheduler (one global `MnSched`, scope-scoped
/// owner stop) the inline owner's OS thread drains the GLOBAL queue, so it runs O, which unblocks I.
/// M:N-only (no coop/interp leg — the cooperative flatten is a separate, later commit; case A still
/// faults deadlock under plain `run`), mirroring `golden_channel_block`. Wrapped in a 30s
/// `recv_timeout` watchdog so a regression fails LOUD instead of hanging CI.
#[test]
fn golden_parallel_cross_nursery_circular_chz_matches_expected() {
    let src = include_str!("../../examples/parallel_cross_nursery_circular.chz");
    let expected = include_str!("../../examples/parallel_cross_nursery_circular.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => panic!(
            "hung — cross-nursery flat scheduler regressed (lost wakeup or owner stopped too early)"
        ),
    }
}

/// Cross-nursery flat scheduler — a MULTI-TASK inner nursery (regression guard for the early-enlist
/// vs deadlock-predicate race): with >1 inner task, helper workers exist, so the outer sibling `O`
/// MUST be enlisted before any worker can run an inner fiber to a park. M:N-only, 30s watchdog.
#[test]
fn golden_parallel_cross_nursery_fanout_chz_matches_expected() {
    let src = include_str!("../../examples/parallel_cross_nursery_fanout.chz");
    let expected = include_str!("../../examples/parallel_cross_nursery_fanout.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => panic!(
            "hung — multi-task inner cross-nursery regressed (early-enlist/deadlock-predicate race)"
        ),
    }
}

/// Cross-nursery flat scheduler — an INLINE-BODY `send` (issued on the builder VM, `self.mn ==
/// None`, sched held only in `mn_enlist_sched`) must wake an enlisted, parked outer sibling. Before
/// the routing fix this false-faulted `deadlock` under `--parallel` (value queued, receiver never
/// made runnable) while coop prints "O got 42". M:N-only, 30s watchdog.
#[test]
fn golden_parallel_cross_nursery_inline_send_chz_matches_expected() {
    let src = include_str!("../../examples/parallel_cross_nursery_inline_send.chz");
    let expected = include_str!("../../examples/parallel_cross_nursery_inline_send.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => {
            panic!("hung — inline-body send to an enlisted sibling regressed (lost wakeup)")
        }
    }
}

/// Cross-nursery flat scheduler — an INLINE-BODY `close` (and the `send` before it) issued on the
/// builder VM must wake an enlisted sibling RANGING over the channel (`for v in t:`): it receives the
/// value, then observes the close and ends. Same routing fix as inline-send, exercising the `close`
/// arm. M:N-only, 30s watchdog.
#[test]
fn golden_parallel_cross_nursery_inline_close_chz_matches_expected() {
    let src = include_str!("../../examples/parallel_cross_nursery_inline_close.chz");
    let expected = include_str!("../../examples/parallel_cross_nursery_inline_close.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => panic!(
            "hung — inline-body close to an enlisted ranging sibling regressed (lost wakeup)"
        ),
    }
}

/// Cross-nursery flat scheduler — independent multi-level nesting (no shared channel) golden: four
/// levels deep (`main`→`top`→`mid`→`inner`), each level a `parallel:` with a sibling `spawn`. The old
/// "2+ enlisting levels" gate wrongly faulted this; it now RUNS. Order is deterministic by
/// data-dependency (each nested call joins — flushing its print — before that level's own sibling
/// runs), so this golden exact-matches AND equals the coop run. Regression guard for the gate removal.
/// M:N-only, 30s watchdog.
#[test]
fn golden_parallel_cross_nursery_multilevel_chz_matches_expected() {
    let src = include_str!("../../examples/parallel_cross_nursery_multilevel.chz");
    let expected = include_str!("../../examples/parallel_cross_nursery_multilevel.expected");
    // Independent nesting is three-engine-stable here, so coop must produce the SAME bytes.
    assert_eq!(
        run_capture(src).expect("coop run"),
        expected,
        "coop diverged from the golden"
    );
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => panic!("hung — independent multi-level nesting golden regressed"),
    }
}

/// Cross-nursery flat scheduler — independent multi-level nesting (no shared channel): three nested
/// `parallel:` blocks, each just printing, with sibling `spawn`s at each level. This is ordinary
/// nesting that does NOT contend a channel, so it must RUN and match the cooperative engine — it
/// must NOT fault (the old "2+ enlisting levels" gate wrongly rejected it). Data-dependent so the
/// inner level prints before the enclosing level finishes; order may still interleave under threads,
/// so we assert the line MULTISET equals coop. M:N-only, 30s watchdog.
#[test]
fn parallel_cross_nursery_independent_3level_runs_all() {
    let src = "fn inner():\n    parallel:\n        spawn:\n            print(\"inner\")\n        spawn:\n            print(\"inner2\")\nfn mid():\n    parallel:\n        spawn:\n            print(\"mid\")\n        inner()\n        spawn:\n            print(\"mid2\")\nfn top():\n    parallel:\n        spawn:\n            print(\"top\")\n        mid()\n        spawn:\n            print(\"top2\")\nfn main():\n    parallel:\n        spawn:\n            print(\"main\")\n        top()\n        spawn:\n            print(\"main2\")\n    print(\"done\")\nmain()\n";
    let coop = run_capture(src).expect("coop run");
    let s = src.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(&s));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let par =
                r.expect("independent multi-level nesting must run under --parallel, not fault");
            let mut a: Vec<&str> = par.lines().collect();
            let mut b: Vec<&str> = coop.lines().collect();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(
                a, b,
                "parallel multiset != coop\nparallel:\n{par}\ncoop:\n{coop}"
            );
        }
        Err(_) => panic!("hung — independent multi-level nesting must complete, not hang"),
    }
}

/// Cross-nursery flat scheduler — a late `spawn:` into a NON-OUTERMOST (middle) nursery must RUN, not
/// be dropped and not panic `index out of bounds` at `join_enlisted_scope` (the old gate faulted it;
/// removing the gate alone clobbered the held sched). The middle nursery is early-enlisted, then
/// refilled by a `spawn:` after `inner()`; that late task runs at the join on the HELD sched as a
/// fresh trailing scope. Must match coop's line multiset (incl. "M2"). M:N-only, 30s watchdog.
#[test]
fn parallel_cross_nursery_late_spawn_into_middle_runs() {
    let src = "fn inner():\n    spawn:\n        print(\"inner\")\nfn mid():\n    parallel:\n        spawn:\n            print(\"M1\")\n        inner()\n        spawn:\n            print(\"M2\")\nfn main():\n    parallel:\n        spawn:\n            print(\"O1\")\n        mid()\n    print(\"done\")\nmain()\n";
    let coop = run_capture(src).expect("coop run");
    let s = src.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(&s));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let par = r.expect("late spawn into middle nursery must run (no panic, no drop)");
            assert!(par.contains("M2"), "late task M2 dropped: {par}");
            let mut a: Vec<&str> = par.lines().collect();
            let mut b: Vec<&str> = coop.lines().collect();
            a.sort_unstable();
            b.sort_unstable();
            assert_eq!(
                a, b,
                "parallel multiset != coop\nparallel:\n{par}\ncoop:\n{coop}"
            );
        }
        Err(_) => panic!("hung — late-spawn-into-middle-nursery must complete, not hang/panic"),
    }
}

/// Cross-nursery flat scheduler — the genuinely-CONTENDED case: 2+ live receivers racing ONE channel
/// across nested nurseries is a racy program. Under `--parallel` it may diverge in delivery order or
/// even deadlock; that is ALLOWED (suspendable concurrency is VM-only / divergent by design — see
/// PROGRESS.md). It must only NEVER PANIC and NEVER HANG: the outcome is EITHER Ok OR a clean
/// `deadlock` fault. M:N-only, 30s watchdog. (Replaces the old 2+-enlisting-levels gate assertion.)
#[test]
fn parallel_cross_nursery_contended_never_panics() {
    let src = "fn inner():\n    spawn:\n        print(\"i\")\nfn mid(cm: Channel[int]):\n    parallel:\n        spawn:\n            x := cm.recv()\n            print(\"M {x}\")\n        inner()\n        cm.send(2)\nfn main():\n    c := Channel[int]()\n    parallel:\n        spawn:\n            y := c.recv()\n            print(\"O {y}\")\n        mid(c)\n        c.send(1)\nmain()\n";
    let s = src.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(&s));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        // Delivery order is concurrent-divergent by design: accept Ok OR a clean deadlock fault.
        Ok(Ok(_)) => {}
        Ok(Err(e)) => assert!(
            e.message.contains("deadlock"),
            "contended case must succeed or deadlock-fault, got: {}",
            e.message
        ),
        Err(_) => panic!(
            "hung — contended cross-nursery must complete or deadlock-fault, never hang/panic"
        ),
    }
}

/// Cross-nursery flat scheduler — late-spawn-into-middle with a PARKED outer receiver. Guards the
/// `register_scope_seeded` atomicity: a non-atomic register→seed of the late trailing scope opened a
/// window where a SENTINEL helper could read (scope incomplete, awaiting_builder=false, runnable==0,
/// outer recv parked) as a false quiesce and spuriously fault the parked receiver. Single sender /
/// single receiver → no contention → deterministic, so VM (`--parallel`) MUST equal the cooperative
/// engine. Run many times to shake the race. (Hardening for the auto-task panel's blocker.)
#[test]
fn parallel_cross_nursery_late_spawn_parked_matches_coop() {
    let src = include_str!("../../examples/parallel_cross_nursery_late_spawn_parked.chz");
    let expected = include_str!("../../examples/parallel_cross_nursery_late_spawn_parked.expected");
    let coop = run(src);
    assert_eq!(coop, expected, "coop must match expected");
    for _ in 0..12 {
        let (tx, rx) = std::sync::mpsc::channel();
        let s = src.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(run_capture_parallel(&s));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(r) => assert_eq!(
                r.expect("parallel run (no spurious deadlock on the parked outer receiver)"),
                expected
            ),
            Err(_) => panic!("hung — late-spawn-into-middle with a parked receiver regressed"),
        }
    }
}

/// Cross-nursery flat scheduler — the `awaiting_builder` deadlock veto must be SURGICAL: a genuine
/// deadlock inside a NESTED nursery (`inner` spawns a no-sender `recv`) must STILL fault even while an
/// outer sibling is early-enlisted (`awaiting_builder`). The nested scope stays incomplete and is NOT
/// `awaiting_builder`, so `all_incomplete_awaiting_builder` is false → the predicate fires. Guards
/// against the veto hanging a real deadlock. M:N-only, 30s watchdog. (charges #1/#2 risk.)
#[test]
fn parallel_cross_nursery_genuine_nested_deadlock_still_faults() {
    let src = "fn inner(dead: Channel[int]):\n    spawn:\n        v := dead.recv()\n        print(\"never {v}\")\nfn main():\n    dead := Channel[int]()\n    parallel:\n        spawn:\n            print(\"O ran\")\n        inner(dead)\n    print(\"done\")\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let err = r.expect_err("a genuine nested no-sender recv must still fault deadlock");
            assert!(
                err.message.contains("deadlock"),
                "expected deadlock, got: {}",
                err.message
            );
        }
        Err(_) => {
            panic!("hung — awaiting_builder veto wrongly suppressed a genuine nested deadlock")
        }
    }
}

/// gaps.md B5 — a `send` from inside a NESTED (child, eager) `parallel:` nursery must wake a receiver
/// parked on that channel in the OUTER nursery. Uncontended 1-send / 1-recv → deterministic, so serial
/// (the oracle) == M:N == the golden. Before the child→parent wake-routing fix (`MnSched::parent_wake`)
/// the DEFAULT M:N engine spuriously faulted `deadlock`: the value landed in the shared `ChannelCore`
/// but the eager child sched's `send_wake` only scanned its OWN park set, so the outer parked receiver
/// was never made runnable and the parent quiesced to a false deadlock — while serial printed "receiver
/// got 1". 10 rounds under a 30s watchdog so a lost-wakeup regression fails loud instead of hanging.
#[test]
fn golden_parallel_cross_nursery_nested_send_to_outer_recv_matches_expected() {
    let src = include_str!("../../examples/parallel_cross_nursery_nested_send_to_outer_recv.chz");
    let expected =
        include_str!("../../examples/parallel_cross_nursery_nested_send_to_outer_recv.expected");
    // Uncontended single-sender/single-receiver → the serial oracle must produce the SAME bytes.
    assert_eq!(
        run_capture(src).expect("coop run"),
        expected,
        "coop diverged from the golden"
    );
    for _ in 0..10 {
        let (tx, rx) = std::sync::mpsc::channel();
        let s = src.to_string();
        std::thread::spawn(move || {
            let _ = tx.send(run_capture_parallel(&s));
        });
        match rx.recv_timeout(std::time::Duration::from_secs(30)) {
            Ok(r) => assert_eq!(
                r.expect("parallel run (no spurious cross-nursery deadlock)"),
                expected
            ),
            Err(_) => panic!("hung — cross-nursery child→parent wake regressed (lost wakeup)"),
        }
    }
}

/// gaps.md B5 detector-accuracy guard — the child→parent wake routing must NOT weaken the deadlock
/// detector. The B5 shape with NO send anywhere (the nested grandchild does other work, never sends)
/// is a GENUINE deadlock: the outer receiver parks forever. It must STILL fault `deadlock` on M:N
/// (`parent_wake` exists but is never triggered — no send). Proves the fix didn't turn the detector
/// off to silence the false positive. 30s watchdog.
#[test]
fn parallel_cross_nursery_nested_no_send_still_deadlocks() {
    let src = "import std.concurrency\nfn main():\n    ready := Channel[int]()\n    parallel:\n        spawn:\n            parallel:\n                spawn:\n                    print(\"grandchild ran\")\n        spawn:\n            v := ready.recv()\n            print(\"receiver got {v}\")\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let err =
                r.expect_err("a genuine no-sender cross-nursery recv must still fault deadlock");
            assert!(
                err.message.contains("deadlock"),
                "expected deadlock, got: {}",
                err.message
            );
        }
        Err(_) => {
            panic!("hung — genuine cross-nursery deadlock no longer fires (detector weakened)")
        }
    }
}

/// gaps.md B5 — a nested (eager) nursery whose grandchild raises a REAL fault (index-out-of-bounds)
/// while an outer sibling parks on the channel must surface the REAL fault, NOT a spurious `deadlock`
/// (`reduce_task_slots` ranks Fault > Deadlocked). Guards that the wake-routing patch keeps the real
/// error winning. Serial oracle reports the same real fault. 30s watchdog.
#[test]
fn parallel_cross_nursery_nested_real_fault_reports_real_error() {
    let src = "import std.concurrency\nfn main():\n    ready := Channel[int]()\n    parallel:\n        spawn:\n            parallel:\n                spawn:\n                    xs := [1]\n                    ready.send(xs[9])\n        spawn:\n            v := ready.recv()\n            print(\"receiver got {v}\")\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let err = r.expect_err("the nested grandchild's real fault must surface");
            assert!(
                err.message.contains("out of bounds"),
                "expected the real fault, got: {}",
                err.message
            );
            assert!(
                !err.message.contains("deadlock"),
                "real fault masked by a spurious deadlock: {}",
                err.message
            );
        }
        Err(_) => panic!("hung — nested real-fault path regressed"),
    }
}

/// gaps.md B5 residual boundary — the fix routes child→parent (a send OUT OF an eager body) ONLY;
/// `parent_wake` points strictly UPWARD. The REVERSE direction (receiver parked INSIDE the eager body,
/// sender in an ANCESTOR nursery) is NOT routed. This program is timing-divergent (if the ancestor send
/// lands first the eager receiver reads the buffered value; if the eager receiver parks first it is not
/// woken → deadlock), so — like the contended case — it must only ever COMPLETE or fault `deadlock`
/// CLEANLY, never panic/hang. Pins that the fix is NOT over-claimed as full "any live sched" coverage.
/// 30s watchdog.
#[test]
fn parallel_cross_nursery_parent_to_child_residual_never_panics() {
    let src = "import std.concurrency\nfn main():\n    ready := Channel[int]()\n    parallel:\n        spawn:\n            parallel:\n                spawn:\n                    v := ready.recv()\n                    print(\"got {v}\")\n        spawn:\n            ready.send(1)\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => assert!(
            e.message.contains("deadlock"),
            "parent→child residual must succeed or deadlock-fault, got: {}",
            e.message
        ),
        Err(_) => {
            panic!("hung — parent→child residual must complete or deadlock-fault, never hang/panic")
        }
    }
}

/// Cross-nursery flat scheduler — a `spawn:` issued AFTER a nursery was early-enlisted (which drained
/// its task vec) must NOT be silently dropped: the late task runs at the join. Before the fix the
/// "already enlisted" join branch discarded the refilled vec. M:N-only, 30s watchdog. (charge #3.)
#[test]
fn golden_parallel_cross_nursery_late_spawn_chz_matches_expected() {
    let src = include_str!("../../examples/parallel_cross_nursery_late_spawn.chz");
    let expected = include_str!("../../examples/parallel_cross_nursery_late_spawn.expected");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => assert_eq!(r.expect("parallel run"), expected),
        Err(_) => panic!("hung — late-spawn-into-enlisted-nursery regressed"),
    }
}

/// Cross-nursery flat scheduler — on an EARLY EXIT (`return`) past a join whose nursery was
/// early-enlisted AND then refilled by a late `spawn:`, the late task must still be accounted: it
/// never started, so it is reported "pending … cancelled" (not silently leaked/dropped). Guards the
/// `drain_escaped_nursery` half of charge #3. M:N-only.
#[test]
fn parallel_cross_nursery_late_spawn_escape_reports_pending() {
    let src = "fn inner():\n    spawn:\n        print(\"inner ran\")\nfn run():\n    parallel:\n        spawn:\n            print(\"O1 ran\")\n        inner()\n        spawn:\n            print(\"O2 ran\")\n        return\nfn main():\n    run()\n    print(\"after\")\nmain()\n";
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let out = r.expect("parallel run");
            assert!(
                out.contains("pending task(s) cancelled"),
                "late escaped task dropped, not reported: {out}"
            );
            assert!(
                out.ends_with("after\n"),
                "post-parallel statement must run: {out}"
            );
        }
        Err(_) => panic!("hung — late-spawn escape path regressed"),
    }
}

/// Cross-nursery flat scheduler regression guard: a GENUINE no-sender deadlock must STILL fault
/// `deadlock` under `--parallel` (not hang, not succeed) — the global predicate must fire even
/// though park/wake/scopes are now VM-global. 30s watchdog so a regression fails loud.
#[test]
fn golden_parallel_deadlock_still_faults() {
    let src = include_str!("../../examples/parallel_deadlock.chz");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    match rx.recv_timeout(std::time::Duration::from_secs(30)) {
        Ok(r) => {
            let err = r.expect_err("a genuine no-sender nursery must still fault deadlock");
            assert!(
                err.message.contains("deadlock"),
                "expected a deadlock fault, got: {}",
                err.message
            );
        }
        Err(_) => panic!(
            "hung — the global deadlock predicate failed to fire (cross-nursery flat scheduler regressed)"
        ),
    }
}

// ----- B1 + B2: cooperative fibers + blocking recv (VM engine) -----

/// Ping-pong across two channels exercises many suspend↔resume cycles: each fiber repeatedly
/// parks on an empty `recv`, the scheduler runs the sibling whose `send` wakes it, and the parked
/// fiber resumes mid-`while`-loop with its locals intact.
#[test]
fn fibers_ping_pong_interleaves() {
    let src = "fn ping(a: Channel[int], b: Channel[int]):\n    i := 0\n    while i < 3:\n        b.send(i)\n        x := a.recv()\n        print(\"ping {x}\")\n        i = i + 1\nfn pong(a: Channel[int], b: Channel[int]):\n    i := 0\n    while i < 3:\n        y := b.recv()\n        print(\"pong {y}\")\n        a.send(y + 100)\n        i = i + 1\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn ping(a, b)\n        spawn pong(a, b)\nmain()\n";
    let expected = "pong 0\nping 100\npong 1\nping 101\npong 2\nping 102\n";
    assert_eq!(run(src), expected);
    // Same result under GC stress — parked fibers' frames/locals are rooted while they wait.
    assert_eq!(run_capture_stress(src), expected);
}

/// All siblings parked on empty channels that no one will fill ⇒ a real deadlock (detected by the
/// scheduler when no fiber is runnable yet not all are done).
#[test]
fn fibers_all_blocked_is_deadlock() {
    let src = "fn waiter(c: Channel[int]):\n    c.recv()\nfn main():\n    a := Channel[int]()\n    b := Channel[int]()\n    parallel:\n        spawn waiter(a)\n        spawn waiter(b)\nmain()\n";
    assert!(run_err(src).contains("deadlock"), "expected deadlock");
}

/// Native-reentry guard: a blocking `recv` reached inside a list-HOF callback cannot park (the
/// HOF's loop state is on the Rust stack), so it faults `deadlock` even though a sibling could
/// otherwise supply the value — the documented B1 v1 limitation, kept memory-safe.
#[test]
fn fibers_recv_inside_map_callback_faults() {
    let src = "fn use_map(ch: Channel[int]):\n    xs := [0]\n    ys := xs.map(fn(x): ch.recv())\n    print(ys)\nfn fill(ch: Channel[int]):\n    ch.send(1)\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn use_map(ch)\n        spawn fill(ch)\nmain()\n";
    assert!(
        run_err(src).contains("deadlock"),
        "recv inside map must fault, not suspend"
    );
}

/// Native-reentry guard: a blocking `recv` inside a struct `index`/`slice`/`set_index` operator
/// overload (run from the native indexing opcodes, host-stack state) cannot park — it faults
/// `deadlock`. Regression for a guard gap that would otherwise corrupt the operand stack.
#[test]
fn fibers_recv_inside_index_overload_faults() {
    let src = "struct Src:\n    ch: Channel[int]\n    fn index(self, k: int) -> int:\n        return self.ch.recv()\nfn use_index(s: Src):\n    print(s[0])\nfn fill(ch: Channel[int]):\n    ch.send(7)\nfn main():\n    ch := Channel[int]()\n    s := Src(ch)\n    parallel:\n        spawn use_index(s)\n        spawn fill(ch)\nmain()\n";
    assert!(
        run_err(src).contains("deadlock"),
        "recv inside index overload must fault, not suspend"
    );
}

/// Native-reentry guard: a blocking `recv` inside a `defer`red call (run during frame teardown,
/// off the suspendable path) faults rather than parking. The `recv` is in the deferred function's
/// body — only the receiver handle is captured at the `defer` statement — so it runs at teardown.
#[test]
fn fibers_recv_inside_defer_faults() {
    let src = "fn consume(ch: Channel[int]):\n    ch.recv()\nfn worker(ch: Channel[int]):\n    defer consume(ch)\n    print(\"body\")\nfn sender(ch: Channel[int]):\n    ch.send(5)\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn worker(ch)\n        spawn sender(ch)\nmain()\n";
    assert!(
        run_err(src).contains("deadlock"),
        "recv inside defer must fault, not suspend"
    );
}

/// A nested `parallel:` inside a child fiber runs its own scheduler level (recursively); the
/// child resumes after its grandchildren join, and the outer sibling runs afterward.
#[test]
fn fibers_nested_parallel() {
    let src = "fn child():\n    parallel:\n        spawn:\n            print(\"grandchild\")\n    print(\"child after nested\")\nfn main():\n    parallel:\n        spawn child()\n        spawn:\n            print(\"sibling\")\nmain()\n";
    assert_eq!(run(src), "grandchild\nchild after nested\nsibling\n");
}

/// D0 — the cooperative scheduler must run a large nursery in ~O(N·logN), not O(N²). 50k trivial
/// fibers each bump one `Shared` counter; the sum proves every fiber was scheduled, and the
/// wall-clock ceiling is the regression guard: the old `pick_runnable` linear-scan-per-turn took
/// ~2.3 s at 50k (RED), the ready-set takes tens of ms (GREEN). The 5 s ceiling is generous for
/// CI noise yet far below the old quadratic wall.
#[test]
fn fibers_scale_ready_queue_not_quadratic() {
    let n = 50_000;
    let src = format!(
        "fn work(s: Shared[int]):\n    s.update(fn(x): x + 1)\n\
             fn main():\n    s := Shared(0)\n    parallel:\n        for _ in 0..{n}:\n            spawn work(s)\n    print(s.get())\nmain()\n"
    );
    let start = std::time::Instant::now();
    let out = run(&src);
    let elapsed = start.elapsed();
    assert_eq!(out, format!("{n}\n"), "every fiber must run exactly once");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "scheduler is quadratic: {n} fibers took {elapsed:?} (ceiling 5s)"
    );
}

/// D0 — the `blocked_on` wake path: one consumer parks on a shared channel and is re-woken by each
/// of many producers' `send`s. Sibling fibers hold DISTINCT `GcRef`s aliasing the same
/// `Arc<ChannelCore>` (cooperative `spawn` deep-clones the channel), so the wake map must key on
/// the core pointer, not the handle — a `GcRef` key would lose every wakeup and fault `deadlock`.
#[test]
fn fibers_many_producers_one_consumer() {
    let n = 200;
    let src = format!(
        "fn produce(ch: Channel[int]):\n    ch.send(1)\n\
             fn consume(ch: Channel[int], k: int, s: Shared[int]):\n    total := 0\n    for _ in 0..k:\n        total += ch.recv()\n    s.set(total)\n\
             fn main():\n    ch := Channel[int]()\n    s := Shared(0)\n    parallel:\n        spawn consume(ch, {n}, s)\n        for _ in 0..{n}:\n            spawn produce(ch)\n    print(s.get())\nmain()\n"
    );
    assert_eq!(
        run(&src),
        format!("{n}\n"),
        "consumer must receive every produced value"
    );
}

/// D0 — cross-level wakeup: a fiber nested in an INNER `parallel:` `send`s to a channel an
/// OUTER-level sibling is parked on. The `send` arm must drain the blocked set of EVERY scheduler
/// level (not just the innermost), or the outer consumer never wakes and the nursery faults
/// `deadlock` after the inner level joins. (The old `pick_runnable` re-scanned all levels each
/// turn, so this worked; the ready-set must preserve it.)
#[test]
fn fibers_cross_level_wakeup() {
    let src = "fn consumer(ch: Channel[int], s: Shared[int]):\n    s.set(ch.recv())\n\
                   fn inner_sender(ch: Channel[int]):\n    ch.send(42)\n\
                   fn middle(ch: Channel[int]):\n    parallel:\n        spawn inner_sender(ch)\n\
                   fn main():\n    ch := Channel[int]()\n    s := Shared(0)\n    parallel:\n        spawn consumer(ch, s)\n        spawn middle(ch)\n    print(s.get())\nmain()\n";
    assert_eq!(
        run(src),
        "42\n",
        "outer consumer must wake from an inner-level send"
    );
}

/// A `recover:` inside a child fiber catches a fault in that fiber's own context (its handlers /
/// frames are per-fiber); the sibling is unaffected and runs normally.
#[test]
fn fibers_recover_inside_child_is_isolated() {
    let src = "fn boom():\n    xs := [1]\n    print(xs[9])\nfn child():\n    r := recover:\n        boom()\n        0\n    print(\"caught\")\nfn main():\n    parallel:\n        spawn child()\n        spawn:\n            print(\"sibling ok\")\nmain()\n";
    assert_eq!(run(src), "caught\nsibling ok\n");
}

/// C3 golden: `Shared[T]` cross-task box — three tasks bump one serialised counter. Byte-identical
/// on the VM, the interpreter, and the `.expected` file.
#[test]
fn golden_shared_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/shared.chz");
    let expected = include_str!("../../examples/shared.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// C5 golden: the `Executor` escape hatch — submit/shutdown (FIFO drain), `defer ex.shutdown()`,
/// shutdown_now (discard). Byte-identical on the VM, the interpreter, and the `.expected` file.
#[test]
fn golden_executor_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/executor.chz");
    let expected = include_str!("../../examples/executor.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

// Micro-tests mirroring the interpreter's C2/C3 unit tests (src/interp/mod.rs), to pin the VM's
// channel/shared/spawn semantics directly (not just via the example goldens).

#[test]
fn channel_send_recv_fifo() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(1)\n    ch.send(2)\n    print(ch.recv())\n    print(ch.recv())\nmain()\n";
    assert_eq!(run(src), "1\n2\n");
}

#[test]
fn channel_send_deep_copies_value() {
    // Mutating the original list after send must NOT change what the channel holds (airlock).
    let src = "fn main():\n    ch := Channel[List[int]]()\n    xs := [1, 2]\n    ch.send(xs)\n    xs.push(3)\n    print(ch.recv())\nmain()\n";
    assert_eq!(run(src), "[1, 2]\n");
}

#[test]
fn channel_recv_on_empty_is_deadlock_error() {
    let err = run_err("fn main():\n    ch := Channel[int]()\n    print(ch.recv())\nmain()\n");
    assert!(err.contains("deadlock"), "got: {err}");
}

/// A1: `try_recv` on an empty channel returns `None` (never the `recv` deadlock fault).
#[test]
fn channel_try_recv_on_empty_returns_none() {
    let src = "fn main():\n    ch := Channel[int]()\n    match ch.try_recv():\n        Some(v): print(\"got {v}\")\n        None: print(\"empty\")\nmain()\n";
    assert_eq!(run(src), "empty\n");
}

/// A1: `try_recv` on a non-empty channel returns `Some(v)` (FIFO).
#[test]
fn channel_try_recv_with_value_returns_some() {
    let src = "fn main():\n    ch := Channel[int]()\n    ch.send(42)\n    match ch.try_recv():\n        Some(v): print(v)\n        None: print(\"empty\")\nmain()\n";
    assert_eq!(run(src), "42\n");
}

/// A1 × B1/B2 (VM-only): `try_recv` must drain the residue left after a *blocking* `recv` resumed.
/// The consumer parks on an empty `recv`; the producer sends two values; the consumer resumes,
/// `recv`s the first, then polls the rest with `try_recv` (the second value, then `None`). Pins
/// that the resume path leaves `suspend`/`ip` clean so the following non-blocking polls behave.
#[test]
fn try_recv_drains_residue_after_blocking_recv_resumes() {
    let src = "fn producer(ch: Channel[int]):\n    ch.send(1)\n    ch.send(2)\nfn consumer(ch: Channel[int]):\n    a := ch.recv()\n    print(\"recv {a}\")\n    match ch.try_recv():\n        Some(v): print(\"try {v}\")\n        None: print(\"try empty\")\n    match ch.try_recv():\n        Some(v): print(\"try {v}\")\n        None: print(\"try empty\")\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn consumer(ch)\n        spawn producer(ch)\nmain()\n";
    let expected = "recv 1\ntry 2\ntry empty\n";
    assert_eq!(run(src), expected);
    assert_eq!(run_capture_stress(src), expected);
}

/// A1 regression guard: `try_recv` on an empty channel INSIDE an active `parallel:` scheduler must
/// return `None` — it must NOT route through the `recv` park path (which would suspend the lone
/// child and then deadlock, since no sibling can ever send). Pins try_recv as truly non-blocking.
#[test]
fn channel_try_recv_in_parallel_does_not_suspend() {
    let src = "fn probe(ch: Channel[int]):\n    match ch.try_recv():\n        Some(v): print(v)\n        None: print(\"empty\")\nfn main():\n    ch := Channel[int]()\n    parallel:\n        spawn probe(ch)\nmain()\n";
    assert_eq!(run(src), "empty\n");
    assert_eq!(run_capture_stress(src), "empty\n");
}

#[test]
fn shared_get_set_round_trip() {
    let src = "fn main():\n    s := Shared(1)\n    print(s.get())\n    s.set(42)\n    print(s.get())\nmain()\n";
    assert_eq!(run(src), "1\n42\n");
}

#[test]
fn shared_update_read_modify_write() {
    let src = "fn main():\n    s := Shared(10)\n    s.update(fn(x): x * 2)\n    s.update(fn(x): x + 1)\n    print(s.get())\nmain()\n";
    assert_eq!(run(src), "21\n");
}

#[test]
fn shared_get_does_not_alias_box() {
    // `get` copies out: mutating the returned list must not change what the box holds.
    let src = "fn main():\n    s := Shared([1, 2])\n    xs := s.get()\n    xs.push(3)\n    print(s.get())\nmain()\n";
    assert_eq!(run(src), "[1, 2]\n");
}

#[test]
fn vm_rwshared_get_set_read_write_roundtrip() {
    // `get`/`set` (write-lock replace) + `read` (R-poly snapshot) + `write` (write-locked RMW).
    let src = "fn main():\n    r := RwShared(10)\n    print(r.get())\n    r.set(20)\n    print(r.read(fn(x): x + 1))\n    r.write(fn(x): x * 2)\n    print(r.get())\nmain()\n";
    assert_eq!(run(src), "10\n21\n40\n");
    // Byte-identical on the interpreter (the frozen oracle).
    assert_eq!(run(src), run_capture_parallel(src).expect("interp run"));
}

#[test]
fn vm_rwshared_read_returns_closure_result_no_writeback() {
    // `read` returns the closure's value and does NOT write back (the box is unchanged).
    let src = "fn main():\n    r := RwShared(5)\n    print(r.read(fn(x): str(x * 3)))\n    print(r.get())\nmain()\n";
    assert_eq!(run(src), "15\n5\n");
}

#[test]
fn vm_rwshared_get_does_not_alias_box() {
    // `get`/`read` copy out: mutating the returned list must not change what the box holds.
    let src = "fn main():\n    r := RwShared([1, 2])\n    xs := r.get()\n    xs.push(3)\n    print(r.get())\nmain()\n";
    assert_eq!(run(src), "[1, 2]\n");
}

#[test]
fn executor_submit_runs_fifo_at_shutdown() {
    let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\n    ex.shutdown()\nmain()\n";
    assert_eq!(run(src), "0\n1\n2\n");
    // M:N drains the two Executor tasks on the pool, so `1`/`2` can print in either order (`0` is
    // the parent's, always first). Same multiset — cooperative FIFO order pinned above.
    assert_same_lines(&run(src), &run_capture_parallel(src).expect("M:N run"));
}

#[test]
fn executor_submit_after_shutdown_errors() {
    let src = "fn main():\n    ex := Executor()\n    ex.shutdown()\n    ex.submit(fn(): print(1))\nmain()\n";
    let err = run_err(src);
    assert!(err.contains("shut-down Executor"), "got: {err}");
    // Parity: same fault message on the interpreter.
    let interp = run_capture_parallel(src)
        .expect_err("interp should fault")
        .message;
    assert_eq!(err, interp, "VM/interp error divergence");
}

/// `shutdown_now` is "attempts to stop" (decision D4) — COOPERATIVE, not preemptive, exactly like
/// Java's `shutdownNow`. The two engines therefore give it different reach, and that difference is
/// deliberate rather than a parity break:
///
/// * `--serial` still queues at `submit` (decision D3), so the job has provably not started and
///   `shutdown_now` discards it outright. This leg is byte-exact and unchanged from the pre-eager era.
/// * The default M:N engine starts the job at its `submit`, so by the time `shutdown_now` trips the
///   cancel flag the job may already have run to completion — a job with no cancellation point in it
///   (this one is a single `print`) cannot be stopped at all. So the only thing assertable across both
///   engines is that `shutdown_now` returns promptly and the program finishes.
///
/// Asserting the M:N leg as a SET rather than a sequence is what keeps this honest: pinning it to
/// either outcome would encode a race as a guarantee.
#[test]
fn executor_shutdown_now_is_cooperative_not_preemptive() {
    let src = "fn j():\n    print(99)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j())\n    ex.shutdown_now()\n    print(0)\nmain()\n";
    assert_eq!(run(src), "0\n", "serial discards work that never started");
    let mn = run_capture_parallel(src).expect("mn run");
    assert!(
        mn == "0\n" || mn == "99\n0\n",
        "M:N must either stop the job or let it finish, never anything else; got {mn:?}"
    );
}

/// EAGER execution, the headline regression guard. A job that blocks on an empty `recv` must WAIT for
/// a value its submitter sends later — it must not declare a deadlock.
///
/// This is the exact program the first (rejected) attempt at eager execution broke: it faulted
/// `recv on an empty channel: deadlock — no runnable task can send`. That verdict is true only while
/// jobs run at the drain, where the submitter really is stuck inside `shutdown()`; once a job starts
/// at its `submit` the submitter is still running and may send on the very next line, so the fault is
/// a lie.
///
/// The `ready` handshake is what gets the job to the blocking `recv` FIRST — without it the main
/// thread usually wins the race, queues the value, and the empty-`recv` path is never exercised at
/// all (that is why the CLI repro for this looked green until a sleep was added; `std.time` is not
/// resolvable through the single-module test helper, so a handshake stands in for the sleep). If main
/// wins anyway the assertion still holds, so it costs coverage in the rare case, never a flake.
///
/// The handshake leg is M:N-ONLY, and that is the point rather than a gap: on `--serial` the job does
/// not exist until `shutdown()` (decision D3), so `ready.recv()` at top level has genuinely nobody to
/// send to it and deadlocks — correctly. The second program drops the handshake and pins the same
/// end state on BOTH engines.
///
/// Watchdogged because the failure mode of getting this wrong the OTHER way (never waking) is a hang.
#[test]
fn executor_job_blocking_recv_waits_for_a_later_send() {
    // Eager-only: the job reaches its blocking `recv` before the value is sent.
    let handshake = r#"
ch := Channel[int](1)
ready := Atomic(0)
fn worker():
    ready.add(1)
    print("job got {ch.recv()}")
ex := Executor()
ex.submit(worker)
spins := 0
while ready.load() == 0:
    spins = spins + 1
ch.send(7)
ex.shutdown()
print("done")
"#;
    // Engine-agnostic: whenever the job reaches the `recv`, the value is there or it waits for it.
    let plain = r#"
ch := Channel[int](1)
ex := Executor()
ex.submit(fn(): print("job got {ch.recv()}"))
ch.send(7)
ex.shutdown()
print("done")
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send((
            run_capture_parallel(handshake),
            run_capture(plain),
            run_capture_parallel(plain),
        ));
    });
    let (mn_handshake, serial_plain, mn_plain) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a blocking job must wake on the send, not hang");
    let want = "job got 7\ndone\n";
    assert_eq!(mn_handshake.expect("mn handshake run"), want);
    assert_eq!(serial_plain.expect("serial run"), want);
    assert_eq!(mn_plain.expect("mn run"), want);
}

/// W7-12 — the MIRROR of the test above: a job blocked on a channel that only its own joiner could
/// have filled must FAULT, not hang. `main` sends only after `shutdown()`, so it is stuck waiting for
/// the job while the job waits for it — a real deadlock, and the caller's mistake.
///
/// Eager execution (§2c) regressed this from "faults in 0s on both engines" to "faults on `--serial`,
/// hangs forever on M:N": the `recv` arm decides by "am I inside a scheduler?", which cannot tell this
/// program apart from the one above. The fix asks the one question that can — is this executor already
/// being JOINED, with every job it still owes parked — so the two programs keep their opposite verdicts.
///
/// Asserted on BOTH engines with the SAME text: the M:N fault now travels out of a worker `Vm` through
/// `reduce_task_slots` rather than an inline drain, so byte-identity with `--serial` (and so with the
/// pre-eager behaviour) is a real claim, not a formality.
///
/// Watchdogged: getting this wrong is a hang, which would otherwise stall the suite rather than fail it.
#[test]
fn executor_job_blocked_during_shutdown_faults_both_engines() {
    let src = r#"
ch := Channel[int](1)
ex := Executor()
ex.submit(fn(): print("job got {ch.recv()}"))
ex.shutdown()
ch.send(42)
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send((run_capture(src), run_capture_parallel(src)));
    });
    let (serial, mn) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a job blocked on its own joiner must fault, not hang");
    let serial = serial
        .expect_err("serial: this program is a deadlock")
        .message;
    let mn = mn.expect_err("M:N: this program is a deadlock").message;
    assert!(
        serial.contains("recv on an empty channel: deadlock"),
        "serial fault was: {serial}"
    );
    assert_eq!(serial, mn, "the deadlock verdict must be engine-identical");
}

/// `W7-12r` residual (a), CLOSED by the process-wide verdict (`future.md` §2d step 0): TWO jobs of one
/// executor both blocked on an empty channel, with `main` inside `shutdown()`, must fault.
///
/// W7-12's per-executor predicate could only judge `outstanding == 1` — anything with a sibling job
/// was declined, because no counter can tell a parked cap-1 handshake from an unfeedable one — so this
/// program, the commonest accidental executor deadlock there is, hung forever on M:N. Go reports it
/// (`fatal error: all goroutines are asleep - deadlock!`, measured, rc=2, two goroutines on one empty
/// channel behind a `WaitGroup`); CPython hangs. We now match Go.
///
/// M:N-only: `--serial` queues at `submit` (decision D3), so it faults here for an unrelated reason and
/// is not evidence about anything this change touches (`--serial` is scheduled for removal, §2b).
///
/// **Needs ≥2 free pool threads.** With one, the second job is reserved but never dispatched, so it
/// counts toward `live` while never registering as blocked and the verdict correctly declines — the
/// bounded-pool starvation hazard `pool.rs` documents (risk G3), unchanged by this detector and
/// verified at `--threads=1` on the CLI. Same constraint as
/// `eager_send_blocked_on_a_full_channel_faults_when_the_channel_is_closed`.
///
/// Watchdogged — the failure mode is a hang, which would stall the suite rather than fail it.
#[test]
fn two_blocked_jobs_in_one_executor_fault_instead_of_hanging() {
    let src = r#"
ch := Channel[int](1)
ex := Executor()
ex.submit(fn(): print("j1 {ch.recv()}"))
ex.submit(fn(): print("j2 {ch.recv()}"))
ex.shutdown()
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    let mn = rx.recv_timeout(std::time::Duration::from_secs(60)).expect(
        "two jobs deadlocked in one executor must fault, not hang — or this host has <2 free \
             pool threads, in which case the second job is queued-but-undispatched and the verdict \
             correctly declines (pool.rs risk G3)",
    );
    let msg = mn
        .expect_err("both jobs wait on a channel nobody can fill — a deadlock")
        .message;
    assert!(
        msg.contains("recv on an empty channel: deadlock"),
        "fault was: {msg}"
    );
}

/// `W7-12r` residual (b), CLOSED: two executors deadlocking EACH OTHER must fault.
///
/// W7-12's predicate swept the executor registry and went silent while any other executor still owed
/// work — necessary then, because "y still owes a job" was the only available proxy for "y might yet
/// send", and over-ruling it faulted a program both ancestors complete (kept green by
/// `executor_job_keeps_waiting_while_another_executor_still_owes_work`). The process-wide verdict does
/// not need the proxy: it asks whether y's job is itself BLOCKED with nothing satisfiable, so a live
/// producer in y still vetoes while a deadlocked one does not. Go reports this program (measured,
/// rc=2); CPython hangs.
///
/// M:N-only, for the same D3 reason as the test above, and it needs ≥2 free pool threads for the same
/// reason too. Watchdogged.
#[test]
fn two_executors_deadlocking_each_other_fault() {
    let src = r#"
c1 := Channel[int](1)
c2 := Channel[int](1)
x := Executor()
y := Executor()
x.submit(fn(): c1.send(c2.recv()))
y.submit(fn(): c2.send(c1.recv()))
x.shutdown()
y.shutdown()
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    let mn = rx.recv_timeout(std::time::Duration::from_secs(60)).expect(
        "two mutually-deadlocked executors must fault, not hang — or this host has <2 free pool \
             threads (see the sibling test above)",
    );
    let msg = mn
        .expect_err("each executor's job waits for the other's — a deadlock")
        .message;
    assert!(
        msg.contains("recv on an empty channel: deadlock"),
        "fault was: {msg}"
    );
}

/// `W7-12r` residual (c), CLOSED: a blocked job with NO explicit `shutdown()` must fault at the
/// program-exit drain rather than hang there.
///
/// W7-12's `JoinGuard` was armed at the `shutdown()` call site and deliberately NOT at the exit drain,
/// because the drain joins every live executor one at a time and a per-executor verdict would have let
/// REGISTRY ORDER decide whose job faulted. A process-wide verdict has no such ordering problem, so
/// `join_eager_jobs` registers its party for every join, this one included.
///
/// Neither ancestor faults here, and that is stated rather than glossed: Go returns from `main` and
/// ABANDONS the goroutine (measured, rc=0), CPython's `ThreadPoolExecutor` joins its non-daemon
/// threads at exit and hangs (measured, rc=124). Chezzi joins at exit (decision D1 — CPython's model),
/// so with CPython's join and Go's verdict rule the answer here is stricter than both, deliberately.
///
/// M:N-only (D3). Watchdogged.
#[test]
fn blocked_job_with_no_shutdown_faults_at_the_exit_drain() {
    let src = r#"
ch := Channel[int](1)
ex := Executor()
ex.submit(fn(): print("job got {ch.recv()}"))
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    let mn = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the exit drain must fault on a deadlocked job, not hang");
    let msg = mn
        .expect_err("the job waits on a channel nobody can fill — a deadlock")
        .message;
    assert!(
        msg.contains("recv on an empty channel: deadlock"),
        "fault was: {msg}"
    );
}

/// The WRONG ANSWER the process-wide verdict also fixes, found while measuring `W7-12r`: `main`
/// blocking on a channel an eager `Executor` job is about to fill used to FAULT.
///
/// ```text
/// ex.submit(fn(): ch.send(42))
/// print("main got {ch.recv()}")     # ← used to be: recv on an empty channel: deadlock
/// ```
///
/// Both ancestors print `main got 42` (measured: CPython `ThreadPoolExecutor` + `queue.Queue(1)`, and
/// Go with a goroutine over a buffered channel). Chezzi faulted, on BOTH engines. The cause is the
/// stale premise `future.md` §2d names: `chan_recv_step`'s "I have no scheduler ⇒ nobody can ever
/// send" `else` arm, which stopped being true the moment eager execution put running jobs outside
/// every scheduler. `main` now blocks there like any other counted party, and the verdict declines
/// while the job is live.
///
/// M:N-only, and the divergence is recorded rather than defended: `--serial` queues at `submit` (D3),
/// so the job cannot run before `main`'s `recv` and it still faults there. That engine is scheduled
/// for removal (§2b) and is not a standard of correctness.
#[test]
fn main_recv_completes_when_an_eager_job_sends() {
    let src = r#"
ch := Channel[int](1)
ex := Executor()
ex.submit(fn(): ch.send(42))
print("main got {ch.recv()}")
ex.shutdown()
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_capture_parallel(src));
    });
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("main must receive the job's value, not hang")
        .expect("this program has no deadlock — both ancestors run it");
    assert_eq!(out, "main got 42\n");
}

/// The process-wide verdict's HANG regression, caught by adversarial review of the change that
/// introduced it: a `wait:` with a CLOSED arm must still reach its deadlock fault.
///
/// `closed` means opposite things at the two blocking sites. A single `recv` on a closed channel makes
/// progress — it returns `ClosedEmpty`, so a `for v in ch:` ends and a bare `recv` faults. But the
/// `wait:` poll SKIPS a closed+empty recv arm (W7-13r(a)), so that arm is not progress at all. Folding
/// both into one `PartyWait` made the closed arm answer "satisfiable" forever, which vetoed the
/// verdict permanently: this program faults in 0 ms before the detector and HUNG with the variants
/// merged (measured on built binaries, both engines, rc 1 → rc 124).
///
/// Both engines, because neither has an executor in play and the fault text is the same.
#[test]
fn a_wait_with_a_closed_arm_still_reports_the_deadlock() {
    let src = r#"
c1 := Channel[int](1)
c2 := Channel[int](1)
c1.close()
wait:
    v := c1.recv():
        print("c1 {v}")
    w := c2.recv():
        print("c2 {w}")
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send((run_capture(src), run_capture_parallel(src)));
    });
    let (serial, mn) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a closed arm must not veto the deadlock verdict forever");
    for (engine, r) in [("serial", serial), ("M:N", mn)] {
        let msg = r
            .expect_err("nothing can feed c2 — this is a deadlock")
            .message;
        assert!(
            msg.contains("wait on channels that are all empty: deadlock"),
            "{engine} fault was: {msg}"
        );
    }
}

/// The process-wide verdict's FALSE-FAULT regression, also caught by adversarial review: an
/// already-drained `shutdown()` must not be read as a blocked joiner.
///
/// `join_eager_jobs` registers its `PartyWait::Join` before it can take the executor lock, so a join
/// with nothing outstanding — `Executor(); e.shutdown()`, and the whole window while the last job's
/// `finish` wakes a real joiner — briefly puts a party in the registry for a thread that is about to
/// return and keep running. While `Join` answered a flat "never satisfiable", a sibling sampling in
/// that window faulted a LIVE program: measured 2/20 runs on this shape, where Go and CPython both
/// print `got 1`. A join's wait condition is exactly `outstanding() == 0`, so that is what it answers.
///
/// The loop is the amplifier — one drained shutdown is a nanosecond-wide window; 20 000 of them beside
/// a genuinely blocked consumer hit it reliably enough to have shown 2/20, and the whole program still
/// runs in ~16 ms. Repeated here for the same reason.
#[test]
fn a_drained_shutdown_is_not_mistaken_for_a_blocked_joiner() {
    let src = r#"
c := Channel[int](1)
exA := Executor()
exA.submit(fn(): print("got {c.recv()}"))
i := 0
while i < 20000:
    e := Executor()
    e.shutdown()
    i = i + 1
c.send(1)
exA.shutdown()
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bad = Vec::new();
        for _ in 0..15 {
            match run_capture_parallel(src) {
                Ok(o) if o == "got 1\n" => {}
                other => bad.push(format!("{other:?}")),
            }
        }
        let _ = tx.send(bad);
    });
    let bad = rx
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("a drained shutdown must not hang either");
    assert!(
        bad.is_empty(),
        "a live program was reported deadlocked in {}/15 runs: {bad:#?}",
        bad.len()
    );
}

/// W7-12's BOUNDARY, and the reason its join predicate asks who is joining rather than just whether
/// anyone is: a `shutdown()` running inside a nursery task says nothing about whether a value can
/// still arrive, because a SIBLING task can be the producer. Here `spawn: ex.shutdown()` runs while
/// `spawn: … ch.send(42)` is one handshake away from sending, so the job must still WAIT.
///
/// Caught by review, not by the suite: the first cut armed the predicate at every explicit
/// `shutdown()`, which made this exact program fault on M:N while `--serial` printed `job got 42` —
/// re-opening, in a new place, precisely the engine divergence W7-12 exists to close. Asserted on both
/// engines for that reason.
///
/// Run through the FILE helpers, not `run_capture`, for one reason: the producer has to be slower
/// than the verdict's own debounce (2 × `DEMOTE_POLL_BACKOFF`) or the send lands first and the program
/// would pass even with the bug present — verified by mutation. That needs a real `timer`, and
/// `std.time` only resolves through a module graph.
#[test]
fn executor_job_keeps_waiting_when_shutdown_runs_beside_a_live_producer() {
    let src = "
import std.concurrency
import std.time
ch: Channel[int] = Channel[int](1)
ex := Executor()
ex.submit(fn(): print(\"job got {ch.recv()}\"))
parallel:
    spawn:
        timer(200).recv()
        ch.send(42)
    spawn:
        ex.shutdown()
print(\"end\")
";
    let entry = write_temp_chz("w712_sibling_producer", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let cfg = crate::native::HostConfig::default;
        let (so, _se, sr, _sc) = run_file_with(&e, cfg());
        let (mo, _me, mr, _mc) = run_file_p(&e);
        let _ = tx.send((so, sr, mo, mr));
    });
    let (so, sr, mo, mr) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a live sibling producer must keep the job waiting, not hang it");
    let _ = std::fs::remove_file(&entry);
    assert!(sr.is_ok(), "serial run faulted: {sr:?}");
    assert!(mr.is_ok(), "M:N run faulted: {mr:?}");
    assert_eq!(so, "job got 42\nend\n", "serial output");
    assert_eq!(mo, so, "the two engines must agree");
}

/// W7-12's other boundary, and the one that matters most: `x.shutdown()` says nothing about whether a
/// job of a DIFFERENT executor `y` is about to send. Executors are independent.
///
/// The measure of correct here is the ancestor, not the parity oracle. Python's `ThreadPoolExecutor`
/// runs this program to completion — `x.submit(consumer)`, `y.submit(producer)`, `x.shutdown()` prints
/// `got 1` — so reporting a deadlock is a WRONG ANSWER about a live program, not a tolerable engine
/// difference. The first cut of this fix did exactly that, and was defended with "`--serial` faults
/// there too", which is an argument about agreement and not about correctness.
///
/// W7-12's predicate bought that by sweeping the executor registry and going silent while any OTHER
/// executor still owed work — at the cost of the opposite error, two mutually-deadlocked executors
/// HANGING. The process-wide verdict keeps this program correct WITHOUT the proxy: it asks whether
/// y's job is itself blocked with nothing satisfiable, so a live producer vetoes and a deadlocked one
/// does not, and the cost is gone (`two_executors_deadlocking_each_other_fault`).
///
/// M:N-only for the same reason as its sibling above: `--serial` queues at `submit` (decision D3), so
/// x's consumer does not exist until `x.shutdown()` drains it, and this shape faults there regardless
/// of anything W7-12 touches.
#[test]
fn executor_job_keeps_waiting_while_another_executor_still_owes_work() {
    let src = "
import std.concurrency
import std.time
ch: Channel[int] = Channel[int](1)
fn consumer():
    print(\"got {ch.recv()}\")
fn producer():
    timer(200).recv()
    ch.send(1)
x := Executor()
y := Executor()
x.submit(consumer)
y.submit(producer)
x.shutdown()
y.shutdown()
print(\"end\")
";
    let entry = write_temp_chz("w712_cross_executor", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_p(&e));
    });
    let (mo, _me, mr, _mc) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a live producer in another executor must keep the job waiting, not hang it");
    let _ = std::fs::remove_file(&entry);
    assert!(
        mr.is_ok(),
        "M:N run faulted where CPython completes: {mr:?}"
    );
    assert_eq!(mo, "got 1\nend\n");
}

/// W7-12's sharpest boundary: **parked is not unfeedable.** A bounded producer/consumer pipeline
/// inside ONE executor spends its whole life with one job parked on a full `send` and the other on a
/// momentarily-empty `recv` — that is the healthy steady state of a cap-1 handshake, and the two jobs
/// feed EACH OTHER.
///
/// The predicate's first form (`blocked >= outstanding`, i.e. "every job this executor owes is
/// parked") read that as a deadlock and faulted `send on a full channel` in 2–7 of 30 runs of this
/// exact program — a NONDETERMINISTIC wrong answer on code Go and CPython run to completion, and the
/// worst outcome available. The two-observation debounce does not help: consecutive 5 ms samples both
/// land in successive parked windows.
///
/// So the verdict is now restricted to `outstanding == 1`, where there is no sibling to hand off with
/// and the counters cannot be misread. This test is the fence, and it LOOPS because one pass proves
/// nothing about a race — the buggy form passed most runs.
///
/// M:N-ONLY, and not for lack of trying: on `--serial` this program faults `send on a full channel`
/// regardless of anything W7-12 touches, because that engine queues at `submit` (decision D3) and runs
/// the producer to completion before the consumer exists. Adding a serial arm would go red for an
/// unrelated reason. A progress-counter variant of the predicate that would have allowed multi-job
/// verdicts was tried and measured to fail this very test (6/40) — see `docs/gaps.md` W7-12.
#[test]
fn executor_bounded_pipeline_is_not_mistaken_for_a_deadlock() {
    let src = "
import std.concurrency
a: Channel[int] = Channel[int](1)
fn prod():
    for i in range(0, 50):
        a.send(i)
    a.close()
fn cons():
    s := 0
    for v in a:
        s = s + v
    print(\"sum {s}\")
ex := Executor()
ex.submit(prod)
ex.submit(cons)
ex.shutdown()
";
    let entry = write_temp_chz("w712_bounded_pipeline", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let mut bad = Vec::new();
        for _ in 0..12 {
            let (o, _e2, r, _c) = run_file_p(&e);
            if r.is_err() || o != "sum 1225\n" {
                bad.push(format!("{r:?} / out={o:?}"));
            }
        }
        let _ = tx.send(bad);
    });
    let bad = rx
        .recv_timeout(std::time::Duration::from_secs(120))
        .expect("a bounded pipeline must not hang");
    let _ = std::fs::remove_file(&entry);
    assert!(
        bad.is_empty(),
        "a healthy cap-1 handshake was reported as a deadlock in {}/12 runs: {bad:#?}",
        bad.len()
    );
}

/// W7-56 (defect 1, the false verdict) — an eager `Executor` job feeding a channel a `parallel:` task
/// is parked on must WORK, not be pre-emptively declared a deadlock.
///
/// Measured pre-fix on the release binary: `child waiting`, then the nursery deadlock fault, rc=1, at
/// **~7 ms — before `feeder`'s 50 ms sleep had even elapsed**. `MnSched::is_deadlocked`'s five gates
/// model this sched's own fibers only, and an eager job has none: it runs on the shared pool, bumping
/// neither `running`/`runnable` nor `inflight`. Go's equivalent (a goroutine feeding a channel a
/// WaitGroup'd goroutine reads) prints `job sending` / `child got 7` and exits 0.
///
/// **M:N-ONLY, and no predicate change can or should alter that.** `--serial` never dispatches
/// eagerly (`netio.rs`'s `if self.parallel` gate; decision D3 = queue-at-submit, drain-at-`shutdown`),
/// so there `feeder` genuinely cannot run before the join and the program IS a real deadlock —
/// verified post-fix: `--serial` still faults, unchanged. Not in `parity_tests.rs`/`tests/chz/` for
/// exactly that reason (both are gated serial == M:N). Same precedent as
/// `executor_bounded_pipeline_is_not_mistaken_for_a_deadlock`.
///
/// Watchdogged: an over-corrected veto turns this into a hang, which stalls the suite instead of
/// failing it.
#[test]
fn executor_job_feeds_a_parked_nursery_task_instead_of_a_false_deadlock() {
    let src = "
import std.concurrency
import std.time
ch := Channel[int]()
fn feeder():
    time.sleep_ms(50)
    print(\"job sending\")
    ch.send(7)
fn waiter():
    print(\"child waiting\")
    v := ch.recv()
    print(\"child got {v}\")
ex := Executor()
ex.submit(feeder)
parallel:
    spawn waiter()
print(\"after nursery\")
";
    let entry = write_temp_chz("w756_job_feeds_nursery", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("an executor job feeding a parked nursery task must not hang");
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "a live program was faulted: {res:?}");
    assert!(
        out.contains("child got 7") && out.contains("after nursery"),
        "the job's value must reach the parked task: {out:?}"
    );
}

/// W7-56 (defect 2, the LOST WAKEUP) — the same program with an `inflight` sibling holding the
/// predicate vetoed for 2 s, which isolates the wake bug from the verdict bug.
///
/// This is the one that pins R2 independently of the veto. Pre-fix output, measured: `child waiting`
/// → `job sending` **at 50 ms — the send really happened** → `sleeper done` at 2 s → nursery deadlock
/// at 2008 ms, **with the value sitting in the channel queue**. `child got 7` never printed. An eager
/// job's `Vm` gets neither `mn` nor `mn_enlist_sched` (`spawn_worker` sets neither; the only `mn`
/// assignment in the tree is `spawn_shell`'s), so `channel_send_wire` took the no-sched branch and
/// `Vm::wake_on_send` returned immediately on its empty `scheduler_stack` — while the waiter sat in
/// `SchedCore::parked`, drainable only by that sched's `wake_bucket`, whose idle workers `cv.wait`
/// with no timeout. Fixed by the `Vm::sched_registry` walk in `wake_on_send`.
///
/// So: had R1 (the veto) landed alone, this program would have hung silently forever instead of
/// faulting. M:N-only + watchdogged for the reasons on the test above.
#[test]
fn an_executor_jobs_send_wakes_a_task_parked_on_another_scheds_channel() {
    let src = "
import std.concurrency
import std.time
ch := Channel[int]()
fn feeder():
    time.sleep_ms(50)
    ch.send(7)
fn waiter():
    v := ch.recv()
    print(\"child got {v}\")
fn sleeper():
    time.sleep_ms(2000)
ex := Executor()
ex.submit(feeder)
parallel:
    spawn waiter()
    spawn sleeper()
";
    let entry = write_temp_chz("w756_job_send_wakes_park", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the job's send must wake the parked task, not strand it");
    let _ = std::fs::remove_file(&entry);
    // Delivery IS the pin, and it is a clean binary one: with only R1 (the veto) this program behaves
    // exactly as it did pre-fix — `sleeper` holds the predicate vetoed for 2 s, the value lands in the
    // queue at 50 ms and nothing ever wakes the parked waiter, so the run ends in the deadlock fault
    // with `child got 7` unprinted. Only the `wake_on_send` registry walk delivers it.
    //
    // No timing assertion, deliberately: this returns at the JOIN, which waits out `sleeper`'s 2 s in
    // both the fixed and broken builds, so wall time here says nothing about when the waiter woke.
    // (On the CLI it is visible directly — post-fix `child got 7` prints at 50 ms, before
    // `sleeper done`; M:N capture buffers per task and flushes in slot order, so the captured string
    // cannot show it.)
    assert!(res.is_ok(), "a live program was faulted: {res:?}");
    assert!(out.contains("child got 7"), "output was: {out:?}");
}

/// W7-56 (the R3 fence — the veto must EXPIRE): a job that finishes WITHOUT sending must leave the
/// nursery free to report its genuine deadlock, not asleep forever.
///
/// This is the direction that makes W7-56 dangerous. Idle M:N workers `cv.wait` with no timeout, so a
/// veto nobody revokes is a permanent SILENT HANG — strictly worse than the false fault it replaces,
/// and it would have converted this exact program (which correctly faults at ~8 ms today) into a
/// hang. `dispatch_eager_job`'s completion closure therefore pokes every live sched after `finish()`,
/// taking each core lock briefly rather than a bare `notify_all` so a worker that already read
/// `outstanding == 1` cannot miss it. Post-fix this faults at ~59 ms (once the job ends) instead of
/// ~8 ms — later, but still a fault.
///
/// M:N-only + watchdogged for the reasons on the two tests above; here the watchdog IS the assertion.
#[test]
fn a_finished_executor_job_lets_the_genuine_nursery_deadlock_fire() {
    let src = "
import std.concurrency
import std.time
ch := Channel[int]()
fn bail():
    time.sleep_ms(50)
fn waiter():
    v := ch.recv()
    print(\"child got {v}\")
ex := Executor()
ex.submit(bail)
parallel:
    spawn waiter()
";
    let entry = write_temp_chz("w756_finished_job_lifts_veto", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (_out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a job that ends without sending must let the deadlock fire — never hang");
    let _ = std::fs::remove_file(&entry);
    let msg = res
        .expect_err("nobody can ever feed `ch` once the job is done — a real deadlock")
        .message;
    assert!(msg.contains("deadlock"), "fault was: {msg}");
}

// ----- gaps.md W7-58 — a stuck job PLUS a stuck nursery owner is a deadlock, not a hang -----

/// W7-58, THE REPRO. A `parallel:` nursery whose task can never be fed, beside an eager `Executor`
/// job that can never be fed either, HUNG FOREVER. Measured pre-fix on the release binary: rc=124 at
/// a 20 s timeout, with `job blocking` and `child waiting` printed and nothing after. Post-fix it
/// faults in ~7 ms. Go reports `all goroutines are asleep - deadlock!` for the same shape.
///
/// The two vetoes were pointing at each other. `MnSched::is_deadlocked` declined because a job was
/// outstanding (W7-56 — an outstanding job is an uncounted sender, which is RIGHT when the job is
/// running), and the process-wide verdict declined because `live = 1 + outstanding = 2` while only
/// the job had registered: `main`, the `1 +`, was sitting in `mn_worker_loop`, which registered
/// nothing. So `parties.len() == 1 < 2` — "somebody is still running" — and that somebody was `main`,
/// which was exactly the thread that was stuck. Fixed by registering the nursery OWNER
/// (`PartyWait::Nursery`, satisfiable iff its nursery can still move) and by giving an idle worker of
/// that nursery the job of ASKING the verdict (an owner never reaches `block_halt_check`).
///
/// **W7-56's veto is untouched** — this does not weaken it, it teaches the OTHER verdict to see the
/// owner. `executor_job_feeds_a_parked_nursery_task_instead_of_a_false_deadlock` is the fence that it
/// stayed untouched, and it is also this change's audit (a): it runs the healthy shape (a job that
/// sleeps 50 ms and then feeds the parked task) with the owner now registered, and must still print
/// `child got 7`, rc=0.
///
/// **M:N-ONLY.** `--serial` queues eager jobs at `submit` and drains at `shutdown` (decision D3), so
/// `other.recv()` never runs before the join and the program is a DIFFERENT (also real) deadlock,
/// reported at a different site. Verified: `--serial` output is byte-identical pre- and post-fix.
/// Not in `parity_tests.rs`/`tests/chz/` for that reason — both are gated serial == M:N. Same
/// precedent as the W7-56 tests above.
///
/// Watchdogged: pre-fix this HANGS, and a regression must fail the suite rather than stall it.
#[test]
fn a_stuck_executor_job_beside_a_stuck_nursery_faults_instead_of_hanging() {
    let src = "
import std.concurrency
ch := Channel[int]()
other := Channel[int]()
fn blocked():
    print(\"job blocking\")
    print(other.recv())
fn waiter():
    print(\"child waiting\")
    print(ch.recv())
ex := Executor()
ex.submit(blocked)
parallel:
    spawn waiter()
print(\"after nursery\")
";
    let entry = write_temp_chz("w758_stuck_job_and_stuck_nursery", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a stuck job beside a stuck nursery must FAULT — pre-fix this hung forever");
    let _ = std::fs::remove_file(&entry);
    let msg = res
        .expect_err("nothing in this run can ever move again")
        .message;
    assert!(
        msg.contains("deadlock"),
        "the nursery must report its deadlock; fault was: {msg}"
    );
    assert!(
        out.contains("child waiting"),
        "the task ran before it parked: {out:?}"
    );
    assert!(
        !out.contains("after nursery"),
        "the nursery join must NOT complete: {out:?}"
    );
}

/// W7-58, THE ANTI-FALSE-FAULT FENCE, and the highest-value test of this change. A healthy **cap-1**
/// producer/consumer pipeline running across an eager `Executor` job and a `parallel:` nursery task
/// must complete, every time.
///
/// This is the shape the `parked-is-not-stuck` family keeps killing: at steady state the party list
/// is `[Send(job), Nursery(owner)]` with `parties.len() == live`, so the COUNT does not save it and
/// satisfiability has to. It holds because the window in which neither party's wait looks satisfiable
/// (a value pushed, the parked consumer not yet requeued) is OCCUPIED by the sending thread, which is
/// either a fiber (so `running >= 1` and the nursery arm answers satisfiable) or a counted party
/// mid-`send` (so it is UNREGISTERED, `parties.len() < live`, and the count vetoes).
///
/// Both directions are exercised — the job producing into the nursery and the nursery producing into
/// the job — because a cap-1 channel is only ever half of a handshake. 300 handoffs each, which is
/// the density that made W7-12's three false-fault predicates show up as 2–7 failures in 30 rather
/// than never. A single green run here proves nothing; the suite gate is that it is green EVERY run.
/// (Measured on the release binary at the CLI: 30/30 at the default width and 30/30 at
/// `CHEZZI_THREADS=2`.)
///
/// M:N-only + watchdogged for the reasons on the tests above (`--serial` never dispatches eagerly).
#[test]
fn a_cap1_pipeline_across_a_job_and_a_nursery_is_not_mistaken_for_a_deadlock() {
    let cases = [
        // The JOB produces, the nursery task consumes.
        "
import std.concurrency
data := Channel[int](1)
done := Channel[int](1)
fn producer():
    for i in range(300):
        data.send(i)
    data.close()
fn consumer():
    total := 0
    for v in data:
        total = total + v
    done.send(total)
ex := Executor()
ex.submit(producer)
parallel:
    spawn consumer()
print(\"sum {done.recv()}\")
ex.shutdown()
",
        // …and the reverse: the nursery task produces, the job consumes and answers, so BOTH
        // directions of the cap-1 handshake cross the job/nursery boundary.
        "
import std.concurrency
req := Channel[int](1)
resp := Channel[int](1)
fn server():
    for v in req:
        resp.send(v * 2)
    resp.close()
fn client():
    total := 0
    for i in range(300):
        req.send(i)
        total = total + resp.recv()
    req.close()
    print(\"sum {total}\")
ex := Executor()
ex.submit(server)
parallel:
    spawn client()
ex.shutdown()
",
    ];
    let expect = ["sum 44850", "sum 89700"];
    for (i, src) in cases.iter().enumerate() {
        let entry = write_temp_chz(&format!("w758_cap1_pipeline_{i}"), src);
        let run_entry = entry.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_file_parallel(
                &run_entry,
                crate::native::HostConfig::default(),
            ));
        });
        let (out, _err, res, _code) = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("a healthy cap-1 pipeline must not hang");
        let _ = std::fs::remove_file(&entry);
        assert!(res.is_ok(), "a live cap-1 pipeline was faulted: {res:?}");
        assert!(
            out.contains(expect[i]),
            "case {i} lost handoffs: {out:?} (wanted {})",
            expect[i]
        );
    }
}

/// W7-58, THE JUDGE FENCE (R3). Two stuck nurseries and NOT ONE polling party: the top-level
/// `parallel:` owner and the eager job that opened its own `parallel:` are both sitting in
/// `mn_worker_loop`, which never calls `block_halt_check`. So with the party registration alone this
/// program still hangs — nobody ever ASKS the verdict. An idle worker of a quiesced sched therefore
/// escalates to the process-wide question on the owner's behalf.
///
/// Measured pre-fix: rc=124 at 20 s. Post-fix: ~7 ms.
///
/// **Asserts the fault, not the exact stdout.** Which of the two nurseries reports first is a
/// legitimate race between two idle workers, and pinning it would be pinning a scheduling accident.
///
/// M:N-only + watchdogged for the reasons on the tests above.
#[test]
fn two_stuck_nurseries_with_no_polling_party_still_fault() {
    let src = "
import std.concurrency
ch := Channel[int]()
ch2 := Channel[int]()
fn innerjob():
    print(ch2.recv())
fn job():
    parallel:
        spawn innerjob()
fn waiter():
    print(ch.recv())
ex := Executor()
ex.submit(job)
parallel:
    spawn waiter()
print(\"after nursery\")
";
    let entry = write_temp_chz("w758_two_stuck_nurseries", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("two stuck nurseries with no polling party must fault — pre-fix this hung");
    let _ = std::fs::remove_file(&entry);
    let msg = res.expect_err("neither nursery can ever move").message;
    assert!(msg.contains("deadlock"), "fault was: {msg}");
    assert!(
        !out.contains("after nursery"),
        "the outer join must not complete: {out:?}"
    );
}

/// W7-58 — the judge must be LEVEL-triggered, not EDGE-triggered. A stuck sched that asked the
/// process-wide verdict and was told "not yet" must ask again when the answer changes — and the
/// answer changes with the PARTY SET, which lives outside this sched and notifies nothing here.
///
/// Measured with the judge's idle wait left untimed: **5/5 permanent hangs, rc=124 at 20 s**. The
/// job's nursery quiesces immediately, judges (`parties.len() == 1 < live == 2` — `main` is awake and
/// unregistered), and sleeps forever; `main`'s `Join` registration 300 ms later reaches nobody, and
/// the run has no judge at all. The tell that isolated it: adding a third job that finishes later
/// made the same program FAULT, because `finish` pokes every sched and the judge re-ran. Post-fix it
/// faults at ~310 ms — the first moment the verdict is actually true.
///
/// The `sleep_ms` is load-bearing: without it `main` reaches `shutdown()` before the nursery
/// quiesces, so the registration lands before the judge's first look and the bug is invisible.
///
/// M:N-only + watchdogged for the reasons on the tests above.
#[test]
fn a_nursery_judge_re_asks_the_verdict_when_a_party_registers_later() {
    let src = "
import std.concurrency
import std.time
ch := Channel[int]()
fn innerjob():
    print(ch.recv())
fn job():
    parallel:
        spawn innerjob()
ex := Executor()
ex.submit(job)
time.sleep_ms(300)
ex.shutdown()
print(\"done\")
";
    let entry = write_temp_chz("w758_judge_reasks", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a party registering AFTER the judge's last look must still be judged — this hung");
    let _ = std::fs::remove_file(&entry);
    let msg = res.expect_err("the job's nursery can never move").message;
    assert!(msg.contains("deadlock"), "fault was: {msg}");
    assert!(!out.contains("done"), "the join must not return: {out:?}");
}

/// W7-58 residual — a cycle of `Executor` joins with NO other party in it hung forever, because
/// `join_eager_jobs` REGISTERED a party and then waited on a plain untimed condvar: it never asked
/// the verdict it had just made answerable.
///
/// Measured pre-fix: three executors whose jobs each `shutdown()` the next, plus `main` joining the
/// first — 4 parties, `live == 4`, every `Join` unsatisfiable, rc=124 at 15 s. The sibling shapes
/// (`two_executors_deadlocking_each_other_fault`) fault in milliseconds only because SOME party in
/// them happens to be channel-blocked and therefore polls; the fault was an accident of the shape.
/// The join now polls at the same `DEMOTE_POLL_BACKOFF` cadence every other blocking-in-place site
/// pays, and asks — but ONLY when every registered party is a `Join`, since any other kind of party
/// has a judge of its own whose fault names the real blocking SITE (which is why
/// `two_executors_deadlocking_each_other_fault` still reports `recv on an empty channel`, unchanged).
///
/// **The ring is STRUCTURAL and the OUTCOME is deterministic, and those are two different problems.**
/// Two earlier cuts of this test were races and both are recorded, because the second failure is the
/// non-obvious one:
///
/// 1. Three executors whose jobs each `shutdown()` the next, gated only on their `recv`s. The gate
///    did not cover the `shutdown` that closes the ring, so the program was often not a cycle at all
///    — measured on the release binary, it completed normally **7 of 40 runs**.
/// 2. Two executors with a proper start BARRIER (each job announces on an unbuffered channel, `main`
///    collects both, then releases both). That ring really is structural — instrumented with
///    `recover:`, **60 of 60 runs detected the deadlock**. It was still **39/40** as a test, because
///    WHICH party reports first is a legitimate race, and when `jobB` (the job of `ex2`) reports it,
///    its fault lands in `ex2`'s slots — an executor nobody in that program ever reduces — so `jobA`
///    completes cleanly, `main`'s `ex1.shutdown()` returns Ok, and the process exits 0.
///
/// A **self-join ring** removes the second problem without weakening the first: `main` and the job
/// both wait on the SAME executor, whose only outstanding work is that job. The ring is exact (`main`
/// waits for the job; the job waits for the executor to owe nothing, which needs the job to finish),
/// and there is only one slot vec, the one `main` reduces — so whichever party detects the deadlock,
/// the fault reaches `main`. 40/40 on the release binary, and 3/3 hangs at HEAD.
///
/// The barrier is still load-bearing: without it `ex.submit` races `ex.shutdown()` and the job can be
/// dispatched after `main` has already drained.
///
/// M:N-only + watchdogged for the reasons on the tests above.
#[test]
fn a_cycle_of_executor_joins_faults_instead_of_hanging() {
    let src = "
import std.concurrency
ex := Executor()
started := Channel[int]()
go := Channel[int]()
fn job():
    started.send(1)
    go.recv()
    ex.shutdown()
ex.submit(job)
started.recv()
go.send(1)
ex.shutdown()
print(\"after\")
";
    let entry = write_temp_chz("w758_join_cycle", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(run_file_parallel(
            &run_entry,
            crate::native::HostConfig::default(),
        ));
    });
    let (out, _err, res, _code) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("a cycle of executor joins must fault — pre-fix this hung forever");
    let _ = std::fs::remove_file(&entry);
    let msg = res
        .expect_err("every party is joining an executor that owes work")
        .message;
    assert!(msg.contains("deadlock"), "fault was: {msg}");
    assert!(!out.contains("after"), "the join must not return: {out:?}");
}

/// W7-13 — a healthy cap-1 handshake must be driven by WAKEUPS, not by the poll timeout.
///
/// `block_wait_tick` used to hand a freshly-taken `core.q` guard straight to `cv.wait_timeout` with no
/// predicate. Its caller had already dropped that lock to attempt its `send`/`recv`, and then ran
/// `block_halt_check` (which takes `exec_registry` + per-core `eager`) before re-taking it — so a
/// consumer's `notify_all` landing in that window hit a condvar nobody was on yet and was lost, and
/// the sender slept a whole `DEMOTE_POLL_BACKOFF` on a channel that already had room.
///
/// **This asserts on the DEFECT ITSELF, not on how long the pipeline took, and that is what makes it
/// load-independent.** [`crate::vm::netio::BLOCK_WAITS_SLEPT_WHILE_READY`] counts waits that burned a
/// whole `DEMOTE_POLL_BACKOFF` tick and then woke to find the channel READY — the lost wakeup, in one
/// number. `wait_timeout_while` re-evaluates the predicate under the guard after each inner wait, so
/// `timed_out()` implies "still not ready" and that counter is structurally ZERO in a fixed build, on
/// an idle machine and on a hammered one alike. Mutation-verified by reverting the call to the old
/// bare `wait_timeout` (which has no such guarantee): **0 of 323 waits fixed, 309 of 1014 broken**,
/// over these same 6 runs. The bug is that dense, so 6 runs replace the 30 the old timing bound
/// needed, and the test costs 0.05 s instead of 0.21 s idle / 5.07 s loaded.
///
/// **Two earlier designs failed, and both are recorded here because neither failure is obvious.**
///
/// 1. **A wall-clock bound on 30 pipelines.** Mutation separation was real (0.14 s fixed vs 2.19 s
///    broken on the release binary) but useless in the suite: the same 30 runs took **5.07 s** under
///    an ordinary full `cargo test` at `RUST_TEST_THREADS=4` — CPU contention costs MORE than the bug
///    does, so every bound loose enough not to fail on a busy machine also passed while the bug was
///    live. It failed on every full-suite run, permanently.
/// 2. **A process-global count of EXPIRED waits.** libtest runs the whole file in ONE process, and
///    the eager tests a dozen slots away in name order each park a job on a `timer(200)` — ~40
///    expired ticks apiece, onto the same global. That version passed alone, passed when the suite
///    scheduled kindly, and FAILED at 24 under `--test-threads=8`; it had already reported one false
///    green.
///
/// The counter used here is process-global too, and is immune to (2) for a reason worth stating: it
/// counts only an event a healthy build CANNOT produce. A neighbour's honest 5 ms timer park is a
/// wait that expired while genuinely not ready and never touches it; a neighbour could pollute this
/// only by hitting the same defect, and then failing is the correct outcome. `BLOCK_WAITS` — total
/// waits — is polluted by neighbours and so is used only as a `>=` COVERAGE floor: it asserts this
/// program still reaches `block_wait_tick` at all, so a future refactor that stopped blocking there
/// cannot leave the test passing vacuously.
#[test]
fn eager_handshake_is_driven_by_wakeups_not_by_the_poll_timeout() {
    let src = "
import std.concurrency
a: Channel[int] = Channel[int](1)
fn prod():
    for i in range(0, 200):
        a.send(i)
    a.close()
fn cons():
    s := 0
    for v in a:
        s = s + v
    print(\"sum {s}\")
ex := Executor()
ex.submit(prod)
ex.submit(cons)
ex.shutdown()
";
    let entry = write_temp_chz("w713_handshake_wakeups", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    use std::sync::atomic::Ordering::Relaxed;
    let waits0 = crate::vm::netio::BLOCK_WAITS.load(Relaxed);
    let stalls0 = crate::vm::netio::BLOCK_WAITS_SLEPT_WHILE_READY.load(Relaxed);
    std::thread::spawn(move || {
        let mut bad = Vec::new();
        for _ in 0..6 {
            let (o, _e2, r, _c) = run_file_p(&e);
            if r.is_err() || o != "sum 19900\n" {
                bad.push(format!("{r:?} / out={o:?}"));
            }
        }
        let _ = tx.send(bad);
    });
    // A worker panic drops `tx` and returns `Disconnected` immediately, which is NOT a hang — say so,
    // rather than reporting every failure as one.
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(120));
    let _ = std::fs::remove_file(&entry);
    let bad = match outcome {
        Ok(v) => v,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("a bounded cap-1 pipeline hung for 120 s")
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the pipeline worker panicked — see the panic above this one")
        }
    };
    let waits = crate::vm::netio::BLOCK_WAITS.load(Relaxed) - waits0;
    let stalls = crate::vm::netio::BLOCK_WAITS_SLEPT_WHILE_READY.load(Relaxed) - stalls0;
    assert!(
        bad.is_empty(),
        "the pipeline must still be correct: {bad:#?}"
    );
    assert_eq!(
        stalls, 0,
        "{stalls} of {waits} blocking waits slept a full 5 ms poll tick and then woke to a channel \
         that was ALREADY ready — a healthy handshake is driven by notifications, and W7-13's lost \
         wakeup is the only thing that produces this (measured: 0 fixed, 309 of 1014 waits with \
         `block_wait_tick` reverted to a bare `wait_timeout`, same 6 runs)"
    );
    // Coverage floor, not a measurement — a concurrent test's own blocking also lands in
    // `BLOCK_WAITS`, so this can only ever be too GENEROUS. 6 runs × 200 handoffs at cap 1 cannot
    // hand off without blocking; if this trips, the program stopped exercising `block_wait_tick` and
    // the assertion above went vacuous.
    assert!(
        waits >= 100,
        "only {waits} blocking waits — this cap-1 pipeline no longer reaches `block_wait_tick`, so \
         the stall assertion above is vacuous"
    );
}

/// W7-13r(c) — an eager job blocked on a FULL channel must fault when that channel is CLOSED, not
/// wait for something else to notice.
///
/// `enqueue_bounded` never consults `closed`, and the eager block loop never returns to the
/// top-of-`send` closed guard, so a blocked sender had no way to observe a close at all. Measured on
/// the release binary, same program:
///
/// | | pre-fix | post-fix | Go |
/// |---|---|---|---|
/// | with `shutdown()` | 112 ms, but reports `send on a full channel: deadlock` about a CLOSED channel | 105 ms, `send on a closed channel` | 104 ms, `panic: send on closed channel` |
/// | no `shutdown()` (this test) | **HANGS** — killed at 12 s | 105 ms | — |
///
/// Without an explicit `shutdown()` the W7-12 verdict cannot fire (`joining == 0`), so nothing else
/// caught it — that is why this test drops the `shutdown()`.
///
/// **The `ready` handshake is what makes the test prove anything**, and it was added after review:
/// with `closer` merely sleeping, an unlucky schedule could run it BEFORE `blocker`, and then
/// `ch.send(1)` faults at the pre-existing top-of-`send` guard — every assertion below passes for a
/// reason that has nothing to do with this fix, on the PRE-fix binary. Waiting for `ready` makes the
/// first `send` provably succeed, so the fault can only come from the blocked second one.
///
/// **Needs ≥2 free pool threads**, like every two-job eager program: a blocked eager job holds its
/// pool thread (no replacement spin), so at `--threads=1` `closer` is never dispatched and this
/// program hangs — before AND after this fix, and equally on `main`. See `pool.rs`'s "Known v1
/// hazard". The 30 s guard below turns that into a clear failure rather than a hung suite.
///
/// **M:N-only.** On `--serial` both jobs are queued at `submit` and the drain runs them one at a
/// time, so `blocker` faults `FULL_SEND_DEADLOCK` before `closer` gets to run its `close()` — the
/// engine cannot interleave them, so it cannot express this program at all (it is NOT that the closer
/// is absent: it does run, and does close the channel, after the fault). `docs/gaps.md` W7-13r
/// records that divergence deliberately, under the standing rule that correctness outranks engine
/// agreement and `--serial` is scheduled for removal.
#[test]
fn eager_send_blocked_on_a_full_channel_faults_when_the_channel_is_closed() {
    let src = "
import std.concurrency
import std.time
ch: Channel[int] = Channel[int](1)
ready: Channel[bool] = Channel[bool](1)
fn blocker():
    ch.send(1)
    print(\"filled\")
    ready.send(true)
    ch.send(2)
    print(\"blocker finished\")
fn closer():
    _ := ready.recv()
    _2 := time.timer(50).recv()
    ch.close()
ex := Executor()
ex.submit(blocker)
ex.submit(closer)
";
    let entry = write_temp_chz("w713c_send_sees_close", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let (o, _e2, r, _c) = run_file_p(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed) = match outcome {
        Ok(v) => v,
        // Two very different causes, so do not accuse the bug by default.
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => panic!(
            "no result in 30 s: either the W7-13r(c) hang is back (a blocked eager `send` not \
             observing `close()`), or this run had <2 free pool threads and `closer` was never \
             dispatched"
        ),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            panic!("the VM thread panicked — see the panic above this one")
        }
    };
    // Proves the blocked path was actually exercised: without this, a `closer`-first schedule faults
    // at the top-of-`send` guard instead and the test passes on the unfixed code.
    assert!(
        out.contains("filled"),
        "the first send must have succeeded, or this test is not exercising the blocked path: \
         out={out:?} r={rdbg}"
    );
    assert!(
        failed,
        "the send must fault, not succeed: out={out:?} r={rdbg}"
    );
    assert!(
        rdbg.contains("send on a closed channel"),
        "the fault must name the CLOSE, not a full-channel deadlock (that was the wrong answer this \
         fixed): {rdbg}"
    );
    assert!(
        !out.contains("blocker finished"),
        "the send must not appear to succeed: {out:?}"
    );
}

/// W7-13r(a) — an eager `wait:` block must be woken by its first arm, not by the poll timeout.
///
/// `op_wait_poll`'s eager branch was a bare `thread::sleep(DEMOTE_POLL_BACKOFF)`, so EVERY wake cost a
/// full 5 ms tick however fast the value arrived. It now waits on ARM 0's condvar with the tick as the
/// timeout — the trick `demote_wait_block` already used for the same N-arms-no-single-condvar problem,
/// which is why the earlier "this needs a new multi-channel wait primitive" note was wrong.
///
/// 300 blocking `wait:` wakeups, release binary, same answer (`44850`) both ways:
///
/// | | before | after |
/// |---|---|---|
/// | wall clock | 1020 / 733 / 1102 ms | **5 / 5 / 5 ms** |
///
/// ~200×, because the old path paid a tick per wakeup and the new one pays a tick only when arm 0 is
/// not the one that fires. The bound below is deliberately far above the fixed timing and far below
/// the broken one, so it discriminates without flaking under a loaded suite. Mutation-verified
/// in-process (stub the wait back to a blind poll): **0.01 s green vs 1.55 s red**.
#[test]
fn an_eager_wait_block_is_woken_by_its_arm_not_by_the_poll_timeout() {
    // The `gate` handshake is what makes this test exercise the path at all: `cons` announces that it
    // is about to block, and only then does `prod` send. Without it `prod` races ahead, every `wait:`
    // finds its value already queued, and the block branch is never reached — measured: the vacuous
    // version passed even with the wait stubbed back to a blind sleep.
    let src = "
import std.concurrency
data: Channel[int] = Channel[int](1)
other: Channel[int] = Channel[int](1)
gate: Channel[bool] = Channel[bool](1)
done: Channel[int] = Channel[int](1)
fn prod():
    for i in range(0, 300):
        _ := gate.recv()
        data.send(i)
fn cons():
    s := 0
    for i in range(0, 300):
        gate.send(true)
        wait:
            v := data.recv():
                s = s + v
            w := other.recv():
                s = s + w
    done.send(s)
ex := Executor()
ex.submit(prod)
ex.submit(cons)
ex.shutdown()
print(done.recv())
";
    let entry = write_temp_chz("w713a_wait_wakeups", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let (o, _e2, r, _c) = run_file_p(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err(), t0.elapsed()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(60));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed, elapsed) = outcome.expect(
        "300 `wait:` wakeups did not finish in 60 s — either a hang, or <2 free pool threads (see \
         the note on `eager_send_blocked_on_a_full_channel_faults_when_the_channel_is_closed`)",
    );
    assert!(!failed, "the program must succeed: out={out:?} r={rdbg}");
    assert_eq!(
        out, "44850\n",
        "every arm value must still be received exactly once"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(400),
        "300 eager `wait:` wakeups took {elapsed:?} (fixed: ~5 ms, blind-poll: ~700-1100 ms) — the \
         eager `wait:` block is sleeping through its wakeups again"
    );
}

/// W7-13r(a)'s LIVE-LOCK fence — a CLOSED arm 0 beside a live arm must not spin the eager `wait:`.
///
/// Go's most ordinary `select` is `case <-done:` / `case v := <-work:` with `done` closed as a
/// broadcast cancel. `op_wait_poll` SKIPS a closed+empty recv arm, so "closed" is NOT a ready
/// condition — but the first version of the arm-0 predicate included `|| g.closed`, so the wait
/// returned instantly, `ip -= 1` re-polled, the arm was skipped, and round it went at full speed:
///
/// | | user CPU |
/// |---|---|
/// | before (blind sleep) | 0.01 s, **0%** |
/// | the broken predicate | 3.00 s, **99%** |
///
/// `MnSched::park_wait` already documents the rule ("the reverted parity-perf-0 live-lock"), and it
/// was broken anyway — so this test exists to make the rule executable rather than advisory.
///
/// **What this test can and cannot catch.** It pins the SEMANTICS: with arm 0 closed, the live arm's
/// value must still be delivered, and the run must not hang. It does NOT measure CPU — `cargo test`
/// asserts verdicts, and every variant of this bug printed the right answer. A future predicate that
/// spins would still pass here. The honest fence for that is the rule in the comment at the predicate.
#[test]
fn an_eager_wait_with_a_closed_arm_still_takes_the_live_arm() {
    let src = "
import std.concurrency
import std.time
done: Channel[bool] = Channel[bool](1)
work: Channel[int] = Channel[int](1)
out: Channel[int] = Channel[int](1)
fn cons():
    wait:
        d := done.recv():
            out.send(-1)
        v := work.recv():
            out.send(v)
fn prod():
    _ := time.timer(60).recv()
    work.send(7)
done.close()
ex := Executor()
ex.submit(cons)
ex.submit(prod)
ex.shutdown()
print(out.recv())
";
    let entry = write_temp_chz("w713a_closed_arm0", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let (o, _e2, r, _c) = run_file_p(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed) = outcome.expect(
        "a `wait:` with one closed arm hung — either the dead arm is being waited on as if live, or \
         <2 free pool threads",
    );
    assert!(!failed, "the live arm must be taken: out={out:?} r={rdbg}");
    assert_eq!(
        out, "7\n",
        "the closed arm must be SKIPPED (not taken, not blocking) and the live arm's value delivered"
    );
}

/// W7-13r(c)'s ORDERING fence — the closed-check must come AFTER the enqueue retry, never before.
///
/// This is the regression the first draft of that fix shipped, caught by adversarial review and not
/// by any existing test. The shape is the most ordinary one there is: a consumer takes the last item
/// and then closes.
///
/// ```text
/// consumer:  a := ch.recv()   # frees the slot FOR the blocked sender
///            ch.close()       # …then wins the race back to `core.q`
/// ```
///
/// Go completes this program — its receive hands the value to a waiting sender ATOMICALLY inside the
/// recv, so by the time `close` runs the send has already happened. Chezzi's eager sender is
/// retry-based: it is only woken and must re-take the slot, so a closed-check placed BEFORE the retry
/// let the close deterministically beat it. Measured 5/5 each way: Go and pre-fix Chezzi print
/// `sent both`; closed-check-first faulted `send on a closed channel` on every run.
///
/// Checking `closed` only after the retry has failed is also exactly the drain-before-close rule the
/// top-of-`send` guard already documents at the head of `channel_method`.
#[test]
fn a_blocked_eager_send_still_completes_when_a_recv_frees_its_slot_before_the_close() {
    let src = "
import std.concurrency
import std.time
ch: Channel[int] = Channel[int](1)
fn blocker():
    ch.send(1)
    ch.send(2)
    print(\"sent both\")
fn consumer():
    _ := time.timer(100).recv()
    a := ch.recv()
    ch.close()
    print(\"got {a}\")
ex := Executor()
ex.submit(blocker)
ex.submit(consumer)
ex.shutdown()
";
    let entry = write_temp_chz("w713c_recv_then_close", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let (o, _e2, r, _c) = run_file_p(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed) = outcome.expect(
        "no result in 30 s — either a hang, or this run had <2 free pool threads (see the sibling \
         test's note)",
    );
    assert!(
        !failed,
        "a send whose slot was freed by a `recv` must COMPLETE, as Go does — faulting here is the \
         closed-check-before-retry regression: out={out:?} r={rdbg}"
    );
    assert!(
        out.contains("sent both"),
        "the freed slot must be taken by the blocked sender: out={out:?}"
    );
}

/// W7-14 — a `wait:` timer arm must LOSE to a sibling value that arrives first for every waiter that
/// owns its OS thread, exactly as `WAIT-1` already made it lose inside a `parallel:` nursery.
///
/// The timeout arm was beating the thing it is a timeout *for*: `op_wait_poll`'s cooperative
/// inline-sleep slept to the timer deadline and took the timer without re-reading the siblings, and
/// WAIT-1's timed-park fix (`0b72ad60`) never reached these paths because it is gated on
/// `self.mn.is_some()` — none of them has an `MnSched`. So a `wait:` with any timer arm degenerated
/// into a plain sleep. Release binary, the same program per waiter (with `timer(300)`, a value at
/// 50 ms):
///
/// | waiter | before | after |
/// |---|---|---|
/// | `parallel:` / `spawn:` fiber (WAIT-1's path) | `value 9` @ 54 ms | unchanged |
/// | eager `Executor` job | **`timer` @ 306 ms** | **`value 9` @ 56 ms** |
/// | top-level `main` | **`timer` @ 306 ms** | **`value 9` @ 56 ms** |
/// | top-level `main` INSIDE a native callback | **`timer` @ 308 ms** | **`value 9` @ 57 ms** |
///
/// Go's `select` takes the value. Not an eager-execution regression — pre-eager `main` (`b6cb9201`)
/// measures the same 306 ms. The fix is NOT a ported `send_wake`: WAIT-1 injects a background
/// deadline send because a parked fiber has no thread of its own, while these parties own their OS
/// thread and can simply clamp the in-place wait to the deadline. Mutation-verified: with the gate
/// reverted, each of these tests reads `timer`.
///
/// **Why `timer(3000)` here when the bug reproduces at 300 ms.** The discriminator is the OUTPUT, not
/// the clock — `value 9` vs `timer` — and a wall-clock bound tight enough to separate 56 ms from
/// 306 ms flakes under a loaded suite (measured: these tests failed a full concurrent `--lib` run at
/// ~2.2 s elapsed while asserting < 250 ms, and passed in isolation). Pushing the deadline to 3 s
/// makes the *answer* itself robust to a 3 s stall, and leaves a loose 1.5 s bound that still
/// separates fixed (~56 ms) from broken (~3007 ms) with 20× headroom either side.
///
/// The `gate` handshake orders the two jobs so the producer's 50 ms starts only after the consumer
/// has entered its `wait:` — without it a producer that races ahead leaves the value already queued
/// and the POLL takes it, which passes even with the bug. (It is the 50 ms sleep that makes the block
/// branch reachable; the gate is what stops the sleep from starting too early.)
#[test]
fn an_eager_wait_timer_arm_loses_to_a_sibling_value() {
    let src = "
import std.concurrency
import std.time
work: Channel[int] = Channel[int](1)
gate: Channel[bool] = Channel[bool](1)
fn prod():
    _ := gate.recv()
    time.sleep_ms(50)
    work.send(9)
fn cons():
    t := time.timer(3000)
    gate.send(true)
    wait:
        _ := t.recv():
            print(\"timer\")
        v := work.recv():
            print(\"value {v}\")
ex := Executor()
ex.submit(prod)
ex.submit(cons)
ex.shutdown()
";
    let entry = write_temp_chz("w714_eager_timer_arm", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let (o, _e2, r, _c) = run_file_p(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err(), t0.elapsed()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(60));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed, elapsed) = outcome.expect(
        "the eager `wait:` hung — or there were <2 free pool threads (see the note on \
         `eager_send_blocked_on_a_full_channel_faults_when_the_channel_is_closed`)",
    );
    assert!(!failed, "the program must succeed: out={out:?} r={rdbg}");
    assert_eq!(
        out, "value 9\n",
        "the 50 ms value must beat the 3 s timer arm (W7-14: the inline-sleep took the timer)"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "the wait took {elapsed:?} (fixed: ~56 ms, inline-sleep: ~3007 ms) — the timer arm is being \
         slept to again even though the value arrived first"
    );
}

/// W7-14 on the **top-level `main`** thread — see the sibling above for the full account.
///
/// `main` is a counted party with no scheduler under it (`mn == None`), so it took the identical
/// inline-sleep and gave the identical wrong answer while an eager job was about to send. Fixing only
/// `eager_core` parties would have left this live.
///
/// Also fences the verdict: `main` registers as a blocked party for this wait, so a timer arm MUST
/// veto the process-wide deadlock verdict (`quiesce::PartyWait::Wait` answers satisfiable for any
/// `core.timer.is_some()` arm). If that ever stops holding, this test faults instead of printing.
#[test]
fn a_top_level_wait_timer_arm_loses_to_an_eager_job() {
    let src = "
import std.concurrency
import std.time
work: Channel[int] = Channel[int](1)
gate: Channel[bool] = Channel[bool](1)
fn prod():
    _ := gate.recv()
    time.sleep_ms(50)
    work.send(9)
ex := Executor()
ex.submit(prod)
t := time.timer(3000)
gate.send(true)
wait:
    _ := t.recv():
        print(\"timer\")
    v := work.recv():
        print(\"value {v}\")
ex.shutdown()
";
    let entry = write_temp_chz("w714_main_timer_arm", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let (o, _e2, r, _c) = run_file_p(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err(), t0.elapsed()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(60));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed, elapsed) = outcome.expect("the top-level `wait:` hung");
    assert!(
        !failed,
        "the program must succeed — a timer arm must veto the deadlock verdict: out={out:?} r={rdbg}"
    );
    assert_eq!(
        out, "value 9\n",
        "the 50 ms value must beat the 3 s timer arm on the `main` thread too (W7-14)"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "the wait took {elapsed:?} (fixed: ~56 ms, inline-sleep: ~3007 ms)"
    );
}

/// W7-14's THIRD waiter — the top-level `main` thread **inside a native callback** (here a list HOF;
/// `Shared.update` and an FFI callback are the same shape). Found by adversarial review of the first
/// two, which shipped green while this path still answered `timer` @ 308 ms.
///
/// It is the reason the gate is not simply [`Vm::can_block_in_place`]: that folds in
/// `is_counted_party`, which requires `native_reentry == 0`, so a `main` thread with a host frame
/// under it fell through to the inline-sleep. The exclusion is about the deadlock verdict being
/// unable to JUDGE such a party — not about whether it may block — and a LIVE TIMER ARM removes the
/// risk it guards, because the wait then provably ends at the deadline no matter what anyone else
/// does. Hence `timed_block = soonest.is_some() && owns_os_thread()`, deliberately narrower than
/// "always block here": with no timer arm an unjudgeable party that blocked forever would hang where
/// the `wait on channels that are all empty: deadlock` fault is the honest answer
/// (`vm_wait_in_native_callback_no_sender_deadlocks` fences that half).
#[test]
fn a_wait_timer_arm_in_a_native_callback_loses_to_a_sibling_value() {
    let src = "
import std.concurrency
import std.time
work: Channel[int] = Channel[int](1)
gate: Channel[bool] = Channel[bool](1)
t: Channel[bool] = time.timer(3000)
fn prod():
    _ := gate.recv()
    time.sleep_ms(50)
    work.send(9)
fn pick(x: int) -> int:
    gate.send(true)
    wait:
        _ := t.recv():
            print(\"timer\")
        v := work.recv():
            print(\"value {v}\")
    return x
ex := Executor()
ex.submit(prod)
_ := [1].map(pick)
ex.shutdown()
";
    let entry = write_temp_chz("w714_callback_timer_arm", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let (o, _e2, r, _c) = run_file_p(&e);
        let _ = tx.send((o, format!("{r:?}"), r.is_err(), t0.elapsed()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(60));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, failed, elapsed) = outcome.expect("the in-callback `wait:` hung");
    assert!(!failed, "the program must succeed: out={out:?} r={rdbg}");
    assert_eq!(
        out, "value 9\n",
        "a `main` thread inside a native callback owns its OS thread too — the 50 ms value must beat \
         the 3 s timer arm there as well (W7-14, adversarial-review round)"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "the wait took {elapsed:?} (fixed: ~57 ms, inline-sleep: ~3008 ms)"
    );
}

/// W7-14's second half, found while measuring the fix — **a timer arm made an eager `wait:`
/// UNCANCELLABLE**, and the job then ran the timer arm's body after the cancel was requested.
///
/// `std::thread::sleep(deadline - now)` observes nothing. `op_wait_poll`'s cancellation checkpoint is
/// at the top of the op, so it can only fire if something returns to the dispatch loop — and the
/// inline-sleep never did until the deadline had passed, at which point it TOOK the timer arm. So
/// `shutdown_now()` (D4's cooperative stop) could not interrupt `wait:` over `timer(3000)`:
///
/// | | before | after |
/// |---|---|---|
/// | `shutdown_now()` at 50 ms, job waiting on `timer(3000)` | `timer` printed, exit @ **3007 ms** | nothing printed, exit @ **57 ms** |
///
/// Measured on release binaries built in separate target dirs. The block-in-place path re-checks
/// `block_halt_check` (cancel included) once per tick, so the cancel now lands within one
/// `DEMOTE_POLL_BACKOFF` instead of one timer deadline — and the arm body never runs.
#[test]
fn a_timer_armed_eager_wait_is_cancellable_by_shutdown_now() {
    let src = "
import std.concurrency
import std.time
work: Channel[int] = Channel[int](1)
fn cons():
    t := time.timer(3000)
    wait:
        _ := t.recv():
            print(\"timer\")
        v := work.recv():
            print(\"value {v}\")
ex := Executor()
ex.submit(cons)
time.sleep_ms(50)
ex.shutdown_now()
print(\"main done\")
";
    let entry = write_temp_chz("w714_cancel_timer_arm", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let (o, _e2, r, _c) = run_file_p(&e);
        let _ = tx.send((o, format!("{r:?}"), t0.elapsed()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, elapsed) = outcome.expect("the cancelled `wait:` hung");
    assert!(
        !out.contains("timer"),
        "a cancelled job must not run its timer arm's body: out={out:?} r={rdbg}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "`shutdown_now()` took {elapsed:?} to interrupt a `wait:` on `timer(3000)` (fixed: ~57 ms, \
         inline-sleep: ~3007 ms) — the timer arm is un-cancellable again"
    );
}

/// W7-16 — a nursery task **already inside** a `sleep_ms` must die when a sibling faults, not at its
/// own deadline. This is the case the existing parity fence never covered: with the fault ORDERED
/// AFTER the sleep begins, `parity_blocking_native_is_a_cancellation_checkpoint_on_both_engines`'s
/// contract is delivered by the *entry* checkpoint at `invoke_native` — reorder the fault by 50 ms and
/// pre-fix M:N ran the full 3005 ms and printed `napper woke`.
///
/// | | before | after |
/// |---|---|---|
/// | sibling faults at ~100 ms, task sleeping 3 s | `napper woke` printed, exit @ **3005 ms** | nothing printed, exit @ **~105 ms** |
///
/// **M:N-only, deliberately.** Serial cannot preempt: nothing else runs while the fiber sleeps, so the
/// fault can only precede or follow the sleep and serial keeps taking the entry checkpoint. Asserting
/// this shape on both engines would be asserting a physics claim, not a contract — `CLAUDE.md`'s
/// "correctness outranks engine agreement". The `--timeout` half of the same contract IS engine-common
/// and is fenced in `test_runner` (`timeout_aborts_a_sleeping_test_everywhere`).
///
/// The `gate` handshake is what makes the ordering deterministic rather than raced: `boom` cannot
/// fault until `napper` has announced it is about to sleep, and then waits another 100 ms. (Losing
/// that race would only fall back to the entry checkpoint — the test cannot flake red.)
#[test]
fn a_sleeping_nursery_task_is_cancelled_mid_flight_by_a_sibling_fault() {
    let src = "
import std.time
gate: Channel[int] = Channel[int](1)
fn napper():
    defer print(\"cleanup ran\")
    print(\"napper start\")
    gate.send(0)
    time.sleep_ms(3000)
    print(\"napper woke\")
fn boom() -> int:
    _ := gate.recv()
    time.sleep_ms(100)
    return 1 / 0
fn main():
    r := recover:
        parallel:
            spawn napper()
            spawn boom()
        0
    print(\"end\")
main()
";
    let entry = write_temp_chz("w716_nursery_mid_flight", src);
    let (tx, rx) = std::sync::mpsc::channel();
    let e = entry.clone();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let (o, _e2, r, _c) = run_file_p(&e);
        let _ = tx.send((o, format!("{r:?}"), t0.elapsed()));
    });
    let outcome = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let (out, rdbg, elapsed) = outcome.expect("the cancelled sleeper hung");
    assert!(
        !out.contains("napper woke"),
        "a task cancelled DURING its sleep must not run past it: out={out:?} r={rdbg}"
    );
    // …and its `defer` still runs. The cancel is delivered by the timer job as a resumed `Err`, an
    // arm that returns WITHOUT re-entering `run_until` — so it must unwind explicitly or every
    // registered cleanup is silently skipped, while the same cancel arriving 50 ms earlier (the entry
    // checkpoint, which faults inside the VM) runs them. Caught by adversarial review, not by the
    // "did it stop promptly" assertions above: a task that skips its cleanup stops just as promptly.
    assert!(
        out.contains("cleanup ran"),
        "a task cancelled DURING its sleep must still unwind through its `defer`s \
         (docs/concurrency.md: cleanup does not depend on scheduler timing): out={out:?} r={rdbg}"
    );
    assert!(
        elapsed < std::time::Duration::from_millis(1500),
        "the cancel took {elapsed:?} to reach a sleeping sibling (fixed: ~105 ms, \
         un-rearmed timer offload: ~3005 ms)"
    );
}

/// W7-5b — an `Executor` constructed INSIDE a task, never explicitly shut down, must still have its
/// work run and waited for at program exit.
///
/// It did not before: `Vm.executors` is a `Vec<GcRef>`, heap-keyed and swapped per fiber, so a nested
/// executor landed in the task's throwaway worker list and was dropped when the task finished — its
/// jobs never ran, were never reaped, and raised no fault. Verified on the pre-change binary: M:N
/// printed `main done` alone while `--serial` (one shared heap, so its list survived) ran both jobs.
/// The fix is the heap-independent `ExecRegistry`, which every worker shares.
///
/// The jobs print their own evidence rather than a counter being read at the end, because the exit
/// join happens AFTER the last top-level statement — a `print` of an accumulator would race the jobs
/// it is trying to observe. Compared as a line SET: the jobs are concurrent, so their order is free.
#[test]
fn executor_created_inside_a_task_is_joined_at_exit_both_engines() {
    let src = r#"
parallel:
    spawn:
        inner := Executor()
        inner.submit(fn(): print("inner job A"))
        inner.submit(fn(): print("inner job B"))
print("main done")
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send((run_capture(src), run_capture_parallel(src)));
    });
    let (serial, mn) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the exit join must not hang");
    let mut want = ["inner job A", "inner job B", "main done"];
    want.sort_unstable();
    for (engine, got) in [("serial", serial), ("mn", mn)] {
        let mut lines: Vec<&str> = got
            .as_deref()
            .expect("run")
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        lines.sort_unstable();
        assert_eq!(lines, want, "{engine}: a task-created executor lost work");
    }
}

/// An `Executor` created BY a job that the program-exit join is itself running must also be joined.
///
/// Pre-existing on both engines and silent: the exit reap iterated a SNAPSHOT of the executor list
/// taken before it started, so an executor born during the reap was never in it and its work simply
/// vanished (verified on the pre-change binary — neither engine printed the inner line). Both sides
/// now re-scan until no un-shut executor is left, which terminates because `shutdown` marks a core
/// `shut` before running anything.
///
/// Worth a test of its own rather than folding into the W7-5b case: that one is about an executor
/// created inside a TASK (a heap-visibility problem), this one is about an executor created inside the
/// JOIN (an iteration-order problem). Fixing either does not fix the other.
#[test]
fn executor_created_by_a_joined_job_is_also_joined_both_engines() {
    let src = r#"
fn makes_another():
    inner := Executor()
    inner.submit(fn(): print("job created BY a job"))
ex := Executor()
ex.submit(makes_another)
print("main done")
"#;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send((run_capture(src), run_capture_parallel(src)));
    });
    let (serial, mn) = rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the exit join must not hang");
    let mut want = ["job created BY a job", "main done"];
    want.sort_unstable();
    for (engine, got) in [("serial", serial), ("mn", mn)] {
        let mut lines: Vec<&str> = got
            .as_deref()
            .expect("run")
            .lines()
            .filter(|l| !l.is_empty())
            .collect();
        lines.sort_unstable();
        assert_eq!(
            lines, want,
            "{engine}: an executor born during the join was lost"
        );
    }
}

/// `shutdown_now()` must reach a job running inside a NESTED executor, not just the job it submitted
/// directly.
///
/// `prepare_eager_job` was the one seam in the tree that installed a cancel token WITHOUT the
/// `scope_ancestors()` half every nursery seam pairs it with (`spawn_shell`, `run_one_fiber`), so an
/// inner executor's job had the flag chain `[inner.cancel]` and never observed the outer trip. The
/// program then paid the full inner sleep at the exit drain.
///
/// The inconsistency this pins is Chezzi-vs-Chezzi, not a borrowed ancestor rule: `outer`'s own
/// `sleep_ms(8000)` is already cancelled at 50 ms (its chain holds the outer flag), while the
/// IDENTICAL sleep one executor deeper ran to completion. Measured ancestors are split — CPython
/// `ThreadPoolExecutor` does not propagate (nested pool: 8.04 s wall, `nap finished` printed, for both
/// `shutdown(wait=False)` and `wait=True`), Go's derived `context.WithCancel(parent)` does (child
/// cancelled at 50 ms) — so the deciding argument is W7-16's, applied one level down: an executor that
/// disagrees with the nursery beside it is the defect.
///
/// M:N only — eager `submit` requires `self.parallel`; `--serial` queues jobs and runs them at the
/// drain, where there is no running inner job to cancel. Wall-clock asserted under a `recv_timeout`
/// watchdog so a regression FAILS loud instead of hanging the suite for 8 s a run.
#[test]
fn nested_executor_job_is_cancelled_by_an_outer_shutdown_now_mn() {
    let src = r#"
import std.time
fn nap():
    time.sleep_ms(8000)
    print("nap finished")
fn outer():
    inner := Executor()
    inner.submit(nap)
    time.sleep_ms(8000)
    print("outer finished")
ex := Executor()
ex.submit(outer)
time.sleep_ms(50)
ex.shutdown_now()
print("done")
"#;
    let entry = write_temp_chz("nested_shutdown_now", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let out = run_file_p(&run_entry);
        let _ = tx.send((out, t0.elapsed()));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let ((out, _err, res, _code), elapsed) =
        result.expect("hung: an outer shutdown_now never reached the nested executor's job");
    assert!(res.is_ok(), "the program faulted: {res:?}");
    assert!(
        !out.contains("nap finished"),
        "the nested job outlived the outer shutdown_now: {out:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "the outer shutdown_now did not cancel the nested job (took {elapsed:?})"
    );
    assert!(out.contains("done"), "main must still finish: {out:?}");
}

/// The NEGATIVE twin of the test above: a `shutdown_now()` must reach only the executors its own job
/// CREATED, never one that merely had work submitted to it from inside that job.
///
/// An `Executor` crosses the airlock by `Arc`, so any job can `submit` to any executor it can name. A
/// first cut keyed cancel inheritance on the SUBMITTER (`if self.eager_core.is_some() {
/// rw.worker.cancel_outer = self.scope_ancestors() }`), which handed `other`'s cancel chain to a job
/// belonging to `main`'s executor: `other.shutdown_now()` then killed an already-started job that was
/// none of its business, AND `shared.shutdown()` — a GRACEFUL join that promises to wait for its work
/// — returned silently with that work dropped ("job started" printed, "shared job ran" never; before
/// the nested-cancel change both printed). Keying it on the executor's CREATOR
/// (`ExecutorCore::creator_cancel`, captured at `Op::NewExecutor`) is what makes both this and the
/// nested case right at once: `shared` was created by `main`, so it inherits nothing from anyone.
///
/// M:N only, for the same reason as its twin — `--serial` queues jobs and runs them at the drain.
#[test]
fn a_job_submitted_to_mains_executor_survives_another_executors_shutdown_now_mn() {
    let src = r#"
import std.time
fn work():
    time.sleep_ms(300)
    print("shared job ran")
fn jobber():
    print("job started")
    shared.submit(work)
    time.sleep_ms(8000)
shared := Executor()
other := Executor()
other.submit(jobber)
time.sleep_ms(50)
other.shutdown_now()
shared.shutdown()
print("done")
"#;
    let entry = write_temp_chz("cross_executor_shutdown_now", src);
    let run_entry = entry.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let out = run_file_p(&run_entry);
        let _ = tx.send((out, t0.elapsed()));
    });
    let result = rx.recv_timeout(std::time::Duration::from_secs(30));
    let _ = std::fs::remove_file(&entry);
    let ((out, _err, res, _code), elapsed) = result.expect("hung");
    assert!(res.is_ok(), "the program faulted: {res:?}");
    assert!(
        out.contains("job started"),
        "the outer job must run: {out:?}"
    );
    assert!(
        out.contains("shared job ran"),
        "an unrelated executor's shutdown_now killed main's executor's job: {out:?}"
    );
    assert!(out.contains("done"), "main must still finish: {out:?}");
    // The outer job's own 8 s sleep IS cancelled (it belongs to `other`), so the whole program is
    // bounded by the 300 ms job — a regression that waits it out would blow this.
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "took {elapsed:?}"
    );
}

/// The THIRD case in the W7-39 family, and the one the two tests above left silent: an executor
/// CREATED inside an eager job, whose handle outlives the job's cancellation.
///
/// `creator_cancel` is captured once at `Op::NewExecutor` and never reset — sticky by design, the way
/// Go's derived `context.WithCancel(parent)` stays cancelled once the parent is. So after the creating
/// job's executor is `shutdown_now`-ed, EVERY later job this core dispatches starts already-cancelled.
/// That is correct; what was indefensible is that it was SILENT: `main` holds the only handle here,
/// its `submit` vanished, and its own GRACEFUL `shutdown()` — which promises to wait for its work —
/// returned having run nothing (measured pre-fix: `main done`, rc=0, `inner job ran` never printed).
/// A `submit` the executor cannot honour now faults, exactly like a `submit` after `shutdown()`.
///
/// **The checkpoint trap**: `work` MUST contain a cancellation point (the `sleep_ms`). With a bare
/// `print` the job finishes before reaching one and runs to completion (D4 is cooperative, not
/// preemptive) — a test written without it is a false green that proves nothing.
///
/// M:N only: `--serial` never sets `eager_core`, so `creator_cancel` is always empty there.
#[test]
fn submit_to_an_executor_whose_creating_job_was_cancelled_faults_mn() {
    let src = r#"
import std.concurrency
import std.time
fn work():
    time.sleep_ms(10)
    print("inner job ran")
fn jobber(ch: Channel[Executor]):
    inner := Executor()
    ch.send(inner)
    time.sleep_ms(2000)
ch := Channel[Executor](1)
outer := Executor()
outer.submit(fn(): jobber(ch))
inner := ch.recv()
time.sleep_ms(50)
outer.shutdown_now()
inner.submit(work)
inner.shutdown()
print("main done")
"#;
    let entry = write_temp_chz("submit_after_creator_cancel", src);
    let (out, _err, res, _code) = run_file_p(&entry);
    let _ = std::fs::remove_file(&entry);
    let e = res.expect_err(&format!(
        "the poisoned submit was accepted silently: {out:?}"
    ));
    assert!(
        e.message
            .contains("submit on an Executor whose creating job was cancelled"),
        "wrong fault: {e:?}"
    );
    assert!(
        !out.contains("main done"),
        "the fault must propagate out of main: {out:?}"
    );
}

/// Negative control 1 — the SAME program with a graceful `outer.shutdown()`. Nothing is cancelled, so
/// the inherited chain stays untripped and the submitted job runs normally.
#[test]
fn submit_to_a_nested_executor_after_a_graceful_outer_shutdown_runs_mn() {
    let src = r#"
import std.concurrency
import std.time
fn work():
    time.sleep_ms(10)
    print("inner job ran")
fn jobber(ch: Channel[Executor]):
    inner := Executor()
    ch.send(inner)
    time.sleep_ms(200)
ch := Channel[Executor](1)
outer := Executor()
outer.submit(fn(): jobber(ch))
inner := ch.recv()
time.sleep_ms(50)
outer.shutdown()
inner.submit(work)
inner.shutdown()
print("main done")
"#;
    let entry = write_temp_chz("submit_after_creator_graceful", src);
    let (out, _err, res, _code) = run_file_p(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "the program faulted: {res:?}");
    assert!(
        out.contains("inner job ran"),
        "the job was dropped: {out:?}"
    );
    assert!(out.contains("main done"), "main must finish: {out:?}");
}

/// Negative control 2 — an executor created by `main` has NO `creator_cancel` at all, so an unrelated
/// executor's `shutdown_now()` cannot reach it. This is the case the new guard must leave untouched.
#[test]
fn submit_to_mains_own_executor_after_an_unrelated_shutdown_now_runs_mn() {
    let src = r#"
import std.time
fn work():
    time.sleep_ms(10)
    print("mine ran")
fn nap():
    time.sleep_ms(8000)
mine := Executor()
other := Executor()
other.submit(nap)
time.sleep_ms(50)
other.shutdown_now()
mine.submit(work)
mine.shutdown()
print("done")
"#;
    let entry = write_temp_chz("submit_mains_own_after_unrelated_now", src);
    let (out, _err, res, _code) = run_file_p(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "the program faulted: {res:?}");
    assert!(
        out.contains("mine ran"),
        "main's own job was dropped: {out:?}"
    );
    assert!(out.contains("done"), "main must finish: {out:?}");
}

// ----- C5 (A2): program-exit auto-drain (VM parity) -----

#[test]
fn golden_executor_autodrain_matches_expected_and_interp() {
    let src = include_str!("../../examples/executor_autodrain.chz");
    let expected = include_str!("../../examples/executor_autodrain.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    // M:N auto-drains the two jobs on the pool — same lines, order may differ.
    assert_same_lines(&vm_out, &run_capture_parallel(src).expect("M:N run"));
}

#[test]
fn executor_autodrain_runs_unshut_at_exit() {
    let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\nmain()\n";
    assert_eq!(run(src), "0\n1\n2\n");
    // M:N drains the two jobs on the pool, racing `1`/`2`; same multiset (`0` is the parent's).
    assert_same_lines(&run(src), &run_capture_parallel(src).expect("M:N run"));
}

#[test]
fn executor_autodrain_not_redrained_after_explicit_shutdown() {
    let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.shutdown()\n    print(0)\nmain()\n";
    assert_eq!(run(src), "1\n0\n");
    assert_eq!(run(src), run_capture_parallel(src).expect("interp run"));
}

#[test]
fn executor_autodrain_survives_gc_stress() {
    // The VM executor registry roots un-shut work so it isn't collected before the exit drain.
    // Under collect-before-every-instruction, the drained closures must still be reachable.
    let src = "fn j(n: int):\n    print(n)\nfn main():\n    ex := Executor()\n    ex.submit(fn(): j(1))\n    ex.submit(fn(): j(2))\n    print(0)\nmain()\n";
    let normal = run(src);
    assert_eq!(
        run_capture_stress(src),
        normal,
        "VM gc_stress diverged (executor auto-drain rooting bug?)"
    );
}

#[test]
fn executor_fault_during_drain_still_runs_every_sibling() {
    // W7-5 run-all review Fix 3: this used to pin the OLD abort contract (a mid-drain fault leaves
    // not-yet-run siblings queued; a `defer ex.shutdown()` then reaps them on the fault exit path,
    // which produces the SAME "A\nC\ndone\n" string as the new contract — that version of this test
    // could not discriminate between the two contracts). With the `defer` removed, `ex.shutdown()`'s
    // own explicit call is the only drain: under run-all it runs `A`, then `boom` (fault noted, not
    // yet raised), then `C`, and only then raises the lowest-submission-index fault (`boom`'s) out of
    // `shutdown()` — genuinely pinning "every submitted job runs, in one drain, even when an earlier
    // one faults". Pre-fix (abort-on-first-fault) this would print only `A` (the drain stops at
    // `boom`, `C` never runs, and there is no `defer` left to reap it on the fault exit path).
    let src = "fn boom():\n    x := [1]\n    print(x[9])\nfn run():\n    ex := Executor()\n    ex.submit(fn(): print(\"A\"))\n    ex.submit(fn(): boom())\n    ex.submit(fn(): print(\"C\"))\n    ex.shutdown()\nfn main():\n    r := recover:\n        run()\n        0\n    print(\"done\")\nmain()\n";
    let vm = run_capture(src).expect("vm run");
    assert_eq!(vm, "A\nC\ndone\n");
    // Cooperative-engine invariant: the M:N engine runs the Executor on a real thread pool, so its
    // drain ordering differs — not a serial-vs-M:N parity comparison.
}

/// W7-5c — `reduce_task_slots` used to flush a faulting task's buffered output only for the FIRST
/// fault (`if first_fault.is_none()`), so with run-all drains (W7-5) a second faulting job's stdout
/// vanished on M:N while serial printed it live. Both faulters' lines must survive; only the
/// lowest-index error propagates.
#[test]
fn executor_second_faulting_job_keeps_its_output_both_engines() {
    let src = r#"
import std.concurrency

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
    r := recover: ex.shutdown()
    match r:
        Ok(_): print("no fault")
        Err(e): print("fault: {e.message()}")

main()
"#;
    let serial = run_capture(src).expect("serial run");
    let mn = run_capture_parallel(src).expect("M:N run");
    for out in [&serial, &mn] {
        assert!(out.contains("a-before-fault"), "lost job 0's output: {out}");
        assert!(out.contains("b-before-fault"), "lost job 1's output: {out}");
        assert!(
            out.contains("fault: boom a"),
            "lowest index must propagate: {out}"
        );
    }
    assert_same_lines(&serial, &mn);
}

#[test]
fn executor_reentrant_shutdown_now_during_drain() {
    // A task that calls `shutdown_now()` mid-drain discards the remaining siblings on BOTH
    // engines (the drain pops from the live queue, so the clear takes effect) — pins the C1 fix.
    let src = "fn stop(e: Executor):\n    e.shutdown_now()\nfn main():\n    ex := Executor()\n    ex.submit(fn(): print(\"A\"))\n    ex.submit(fn(): stop(ex))\n    ex.submit(fn(): print(\"C\"))\n    ex.shutdown()\n    print(\"end\")\nmain()\n";
    let vm = run_capture(src).expect("vm run");
    assert_eq!(vm, "A\nend\n");
    // Cooperative-engine invariant: the M:N engine's pooled Executor drains differently — not a
    // serial-vs-M:N parity comparison.
}

#[test]
fn spawn_first_error_aborts_siblings() {
    // The first task to fault aborts the remaining siblings and propagates out of `parallel:`.
    let src = "fn boom():\n    x := [1]\n    print(x[5])\nfn quiet():\n    print(\"ran\")\nfn main():\n    parallel:\n        spawn boom()\n        spawn quiet()\nmain()\n";
    let vm = run_err(src);
    let interp = match run_capture_parallel(src) {
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
    assert_eq!(vm, run_capture_parallel(src).expect("interp run"));
}

/// Block-scoped defer: assert VM == interp == `expected` for a snippet.
fn assert_defer_scope(src: &str, expected: &str) {
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected, "vm output");
    assert_eq!(
        run_capture_parallel(src).expect("interp run"),
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
/// `Deferred::Block` on the interp. Asserted byte-identical on both engines.
#[test]
fn defer_block_form_runs_in_order_lifo_as_unit() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    defer log(\"outer\")\n    defer:\n        log(\"b1\")\n        log(\"b2\")\n    log(\"body\")\nmain()\n",
        "body\nb1\nb2\nouter\n",
    );
}

/// Uniform by-reference capture (E1): a `defer:` block shares the enclosing binding's cell (it runs
/// in the same task), so it sees the LATEST value of a captured local when it runs at frame exit —
/// `x` is `2` by then, not the `1` it held at the defer point. Parity-checked across both engines.
#[test]
fn defer_block_form_shares_latest_value() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    x := 1\n    defer:\n        log(\"x={x}\")\n    x = 2\n    log(\"now={x}\")\nmain()\n",
        "now=2\nx=2\n",
    );
}

/// A `?` short-circuit INSIDE a `defer:` block: the block has no error-return contract, so the
/// propagated `Err` is discarded (statements after the `?` don't run) — byte-identical on both
/// engines. The VM runs the block as a closure and discards its return at the defer boundary; the
/// interp absorbs the propagation in `run_block_task` to match (regression for a found divergence
/// where the interp leaked a "? propagation" runtime error).
#[test]
fn defer_block_form_discards_question_propagation() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn risky(ok: bool) -> int!:\n    if ok:\n        return Ok(1)\n    return Err(\"boom\")\nfn main() -> int!:\n    defer:\n        log(\"clean start\")\n        n := risky(false)?\n        log(\"clean end\")\n    log(\"body\")\n    return Ok(0)\nmain()\n",
        "body\nclean start\n",
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
        "fn log(s: str):\n    print(s)\nfn risky(ok: bool) -> int!:\n    if ok:\n        return Ok(1)\n    return Err(\"boom\")\nfn main():\n    r := recover:\n        defer log(\"release\")\n        x := risky(true)?\n        x\n    log(\"got\")\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
        "release\ngot\nok 1\n",
    );
}

/// A `recover:`-block defer runs on the **`?` short-circuit** path, before the propagated
/// `Err`/`None` is bound as the recover's result.
#[test]
fn defer_recover_runs_on_try_path() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn risky(ok: bool) -> int!:\n    if ok:\n        return Ok(1)\n    return Err(\"boom\")\nfn main():\n    r := recover:\n        defer log(\"release\")\n        x := risky(false)?\n        x\n    log(\"got\")\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
        "release\ngot\nerr boom\n",
    );
}

/// A `recover:`-block defer runs on the **genuine-fault** path, as the panic unwinds to the
/// boundary, before the `Err(message)` is bound.
#[test]
fn defer_recover_runs_on_fault_path() {
    assert_defer_scope(
        "fn log(s: str):\n    print(s)\nfn main():\n    r := recover:\n        defer log(\"release\")\n        xs := [1]\n        y := xs[5]\n        y\n    log(\"got\")\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
        "release\ngot\nerr index 5 out of bounds (len 1)\n",
    );
}

/// A defer that itself faults during a `recover:` unwind supersedes the in-flight result — its
/// fault becomes the recover's `Err`.
#[test]
fn defer_recover_fault_supersedes() {
    assert_defer_scope(
        "fn boom():\n    xs := [1]\n    x := xs[9]\nfn main():\n    r := recover:\n        defer boom()\n        42\n    match r:\n        Ok(v): print(\"ok {v}\")\n        Err(e): print(\"err {e.message()}\")\nmain()\n",
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
        "fn log(s: str):\n    print(s)\nfn boom() -> int!:\n    return Err(\"x\")\nfn main():\n    for i in 0..2:\n        defer log(\"loop{i}\")\n        r := recover:\n            if true:\n                defer log(\"inner{i}\")\n                n := boom()?\n                n\n            0\n        log(\"end{i}\")\nmain()\n",
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

// ----- golden coverage for the formerly-orphaned examples + the comprehensive torture
// programs (edge_cases / evaluator / ledger). Each pins exact output AND cross-engine parity.

/// `examples/hof.chz` — a function-typed parameter applied to a closure.
#[test]
fn golden_hof_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/hof.chz");
    let expected = include_str!("../../examples/hof.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `examples/list_hof.chz` — `map`/`filter`/`fold`, incl. an element-type-changing map.
#[test]
fn golden_list_hof_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/list_hof.chz");
    let expected = include_str!("../../examples/list_hof.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `examples/list_hof_shrink.chz` — map/filter/fold iterate a snapshot, so a callback that
/// shrinks (or grows) the receiver does not perturb iteration (and never OOB-panics). Locks
/// VM==interp byte-identical: before the fix the VM panicked here while the interp passed.
#[test]
fn golden_list_hof_shrink_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/list_hof_shrink.chz");
    let expected = include_str!("../../examples/list_hof_shrink.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `examples/list_methods.chz` — pop/reverse/contains/index_of/sum + value/iter ergonomics
/// (min/max/first/last/reversed/insert/remove_at, unique/dedup/chunk/windows/take_while/drop_while/count/position).
#[test]
fn golden_list_methods_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/list_methods.chz");
    let expected = include_str!("../../examples/list_methods.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `examples/loops.chz` — break/continue across for-range, for-list, and while loops.
#[test]
fn golden_loops_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/loops.chz");
    let expected = include_str!("../../examples/loops.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `examples/match_value.chz` — `match` on int/str literals with `_`, stmt + expr forms.
#[test]
fn golden_match_value_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/match_value.chz");
    let expected = include_str!("../../examples/match_value.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `examples/pair.chz` — tuples, multi-return, destructuring let, `.0`/`.1` access.
#[test]
fn golden_pair_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/pair.chz");
    let expected = include_str!("../../examples/pair.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `examples/method_default_args.chz` — default + named args on methods (was parity-only).
#[test]
fn golden_method_default_args_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/method_default_args.chz");
    let expected = include_str!("../../examples/method_default_args.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `examples/method_type_params.chz` — a method's own `[U]` inferred per call (was parity-only).
#[test]
fn golden_method_type_params_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/method_type_params.chz");
    let expected = include_str!("../../examples/method_type_params.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `examples/param_protocol.chz` — a user-defined parameterized protocol bound (was parity-only).
#[test]
fn golden_param_protocol_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/param_protocol.chz");
    let expected = include_str!("../../examples/param_protocol.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `examples/edge_cases.chz` — torture test: arithmetic faults under `recover:`, int/float
/// boundaries, empty/nested collection printing, slice clamping, index faults, truthiness,
/// block-scoped shadowing, closure capture-by-value, defer LIFO, and comprehensions.
#[test]
fn golden_edge_cases_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/edge_cases.chz");
    let expected = include_str!("../../examples/edge_cases.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Left-shift overflow is a recoverable fault (`integer overflow in Shl`), matching the
/// `+ - * / %` checked-arith policy — not a silent wrap to `i64::MIN`. Right shift and every
/// non-overflowing left shift (incl. negative operands that round-trip) stay byte-identical,
/// and the VM stays in lock-step with the interpreter.
#[test]
fn shift_left_overflow_is_recoverable_fault() {
    // overflow → recoverable fault with the shared arith-overflow message, on both engines
    for src in ["print(1 << 63)", "print(3 << 62)", "print(2 << 62)"] {
        let vm = run_capture(src).expect_err("vm: shift overflow should fault");
        assert_eq!(vm.message, "integer overflow in Shl");
        let it = run_capture_parallel(src).expect_err("interp: shift overflow");
        assert_eq!(it.message, "integer overflow in Shl");
    }

    // non-overflowing shifts (incl. `-1 << 63 == i64::MIN` which round-trips, `>>` which
    // never overflows) must NOT fault and must agree across engines
    for (src, want) in [
        ("print((0 - 1) << 63)", "-9223372036854775808\n"),
        ("print(1 << 62)", "4611686018427387904\n"),
        ("print(0 << 63)", "0\n"),
        ("print(1024 >> 2)", "256\n"),
        ("print((0 - 8) >> 1)", "-4\n"),
    ] {
        let vm = run_capture(src).expect("vm: non-overflowing shift");
        assert_eq!(vm, want, "vm mismatch for `{src}`");
        assert_eq!(run_capture_parallel(src).expect("interp"), want);
    }
}

/// `pad_left` with an EMPTY `fill` used to LIVELOCK (the pad loop never grew the string), producing
/// zero output and no diagnostic. It must now raise a RECOVERABLE fault EAGERLY — before the
/// `width <= len` early-out, so the fault is not input-dependent — on both engines, for both the
/// native method and the `std.string` free fn.
#[test]
fn pad_left_empty_fill_is_recoverable_fault() {
    const MSG: &str = "pad_left: fill must not be empty";
    // Native method. Both a growing width and a width the receiver already exceeds (the eager rule:
    // `width <= len` does NOT excuse an empty fill), plus the degenerate `"".pad_left(0, "")`.
    for src in [
        r#"print("a".pad_left(5, ""))"#,
        r#"print("abc".pad_left(1, ""))"#,
        r#"print("".pad_left(0, ""))"#,
    ] {
        assert_eq!(
            run_capture(src)
                .expect_err("serial: empty fill should fault")
                .message,
            MSG,
            "serial mismatch for `{src}`"
        );
        assert_eq!(
            run_capture_parallel(src)
                .expect_err("M:N: empty fill should fault")
                .message,
            MSG,
            "M:N mismatch for `{src}`"
        );
    }

    // (The `std.string` free fn's identical fault needs the module-graph runner — it is asserted in
    // `parity_tests::parity_std_str_pad_left_empty_fill_faults`.)

    // The fault is CATCHABLE by `recover:` (a recoverable fault, not a host panic).
    let rec = "fn main():\n\
               \x20   r := recover:\n\
               \x20       \"a\".pad_left(5, \"\")\n\
               \x20   match r:\n\
               \x20       Ok(v): print(\"ok {v}\")\n\
               \x20       Err(e): print(\"caught {e.message()}\")\n\
               main()\n";
    let want = format!("caught {MSG}\n");
    assert_eq!(run_capture(rec).expect("serial: recover"), want);
    assert_eq!(run_capture_parallel(rec).expect("M:N: recover"), want);
}

/// A multi-character `fill` is a repeating cycle TRUNCATED to fit: the result is EXACTLY `width`
/// codepoints (it used to overshoot — `"a".pad_left(4, "xy")` produced the 5-char `"xyxya"`).
#[test]
fn pad_left_multi_char_fill_is_exactly_width() {
    for (src, want) in [
        (r#"print("a".pad_left(4, "xy"))"#, "xyxa\n"),
        (r#"print("ab".pad_left(7, "xyz"))"#, "xyzxyab\n"),
        (r#"print("a".pad_left(2, "xy"))"#, "xa\n"),
        // Single-char fill (the normal path) and the never-shrinks rule are unchanged.
        (r#"print("7".pad_left(3, "0"))"#, "007\n"),
        (r#"print("12345".pad_left(3, "0"))"#, "12345\n"),
        (r#"print("a".pad_left(-5, "0"))"#, "a\n"),
        // `width = i64::MIN` (reachable from safe source) must ALSO just return `s` — the `need`
        // subtraction must not overflow (debug: host panic; release: wraps to a huge positive
        // `need` and bogusly faults `string pad capacity overflow`).
        (r#"print("ab".pad_left(-9223372036854775808, "x"))"#, "ab\n"),
        (r#"print("".pad_left(-9223372036854775808, "x"))"#, "\n"),
    ] {
        assert_eq!(
            run_capture(src).expect("serial"),
            want,
            "serial mismatch for `{src}`"
        );
        assert_eq!(
            run_capture_parallel(src).expect("M:N"),
            want,
            "M:N mismatch for `{src}`"
        );
    }

    // (The `std.string` free fn is a byte-identical alias — asserted in
    // `parity_tests::parity_std_str_pad_left_matches_native_method`.)
}

/// Padding counts CODEPOINTS, not bytes — a non-ASCII fill char counts as 1 and no mid-char slice
/// can panic.
#[test]
fn pad_left_codepoints_not_bytes() {
    for (call, want) in [
        (r#""é".pad_left(3, "ü")"#, "üüé\n"),
        (r#""a".pad_left(4, "日本")"#, "日本日a\n"),
        (r#""héllo".pad_left(3, "0")"#, "héllo\n"),
    ] {
        let src = format!("print({call})");
        assert_eq!(
            run_capture(&src).expect("serial"),
            want,
            "serial mismatch for `{call}`"
        );
        assert_eq!(
            run_capture_parallel(&src).expect("M:N"),
            want,
            "M:N mismatch for `{call}`"
        );
    }
}

/// A huge `width` routes through the same capacity guard as `repeat`: a recoverable fault, not an
/// OOM/abort. (Native only — the `std.string` free fn cannot probe the allocator; that is a documented
/// divergence, docs/stdlib.md.)
#[test]
fn pad_left_huge_width_is_recoverable_fault() {
    for src in [
        r#"print("a".pad_left(9223372036854775807, "x"))"#,
        r#"print("a".pad_left(100000000000000000, "x"))"#,
    ] {
        assert_eq!(
            run_capture(src)
                .expect_err("serial: huge pad should fault")
                .message,
            "string pad capacity overflow",
            "serial mismatch for `{src}`"
        );
        assert_eq!(
            run_capture_parallel(src)
                .expect_err("M:N: huge pad should fault")
                .message,
            "string pad capacity overflow",
            "M:N mismatch for `{src}`"
        );
    }
}

/// `str.repeat(n)` with a huge `n` must raise a RECOVERABLE fault (not hard-panic the process via
/// Rust's `str::repeat` capacity-overflow abort), on both engines, matching the repo's
/// checked-overflow policy. Normal repeats still work and stay parity-equal.
#[test]
fn str_repeat_capacity_overflow_is_recoverable_fault() {
    // `expect_err` (not a process abort) proves the panic was converted to a fault.
    let src = r#"print("ab".repeat(9223372036854775807))"#;
    let vm = run_capture(src).expect_err("vm: repeat overflow should fault");
    assert_eq!(vm.message, "string repeat capacity overflow");
    let it = run_capture_parallel(src).expect_err("interp: repeat overflow should fault");
    assert_eq!(it.message, "string repeat capacity overflow");

    // Huge-but-representable count: passes the `isize::MAX` byte guard but the allocation is
    // infeasible. Before the `try_reserve_exact` guard this ABORTED the process (SIGABRT).
    let huge = r#"print("ab".repeat(100000000000000000))"#;
    let vm = run_capture(huge).expect_err("vm: huge repeat should fault");
    assert_eq!(vm.message, "string repeat capacity overflow");
    let it = run_capture_parallel(huge).expect_err("interp: huge repeat should fault");
    assert_eq!(it.message, "string repeat capacity overflow");

    // Empty receiver with a huge count: `total == 0` passes BOTH the byte guard and
    // `try_reserve_exact`, so the result must be "" produced INSTANTLY. A naive
    // `for _ in 0..n { out.push_str("") }` fill would loop ~1e17 times (an uncatchable hang);
    // `str::repeat` short-circuits the empty receiver. The `expect`/value assertion only
    // returns if no hang occurred.
    let empty_huge = r#"print("".repeat(100000000000000000))"#;
    assert_eq!(run_capture(empty_huge).expect("vm"), "\n");
    assert_eq!(run_capture_parallel(empty_huge).expect("interp"), "\n");

    // Sane repeats are unaffected and agree across engines.
    let ok = r#"print("ab".repeat(3))"#;
    assert_eq!(run_capture(ok).expect("vm"), "ababab\n");
    assert_eq!(run_capture_parallel(ok).expect("interp"), "ababab\n");
}

/// Collection operators (gap #3): list `+` (concat) / `*` (repeat) and set `| & - ^`
/// (union/intersection/difference/symmetric-difference). Value-correctness on the VM; the
/// golden parity test below proves VM==interp. Set print preserves insertion order (mine-then-
/// other for union; mine-filtered for intersection/difference; mine-not-in-other then
/// other-not-in-mine for symmetric-difference).
#[test]
fn collection_operators_eval_correct() {
    // list concat
    assert_eq!(
        run_capture("print([1, 2] + [3, 4])").expect("vm"),
        "[1, 2, 3, 4]\n"
    );
    // empty-side concat keeps element type / values
    assert_eq!(run_capture("print([] + [1, 2])").expect("vm"), "[1, 2]\n");
    assert_eq!(run_capture("print([1, 2] + [])").expect("vm"), "[1, 2]\n");
    // list repeat (both orders)
    assert_eq!(run_capture("print([0] * 3)").expect("vm"), "[0, 0, 0]\n");
    assert_eq!(run_capture("print(2 * [7])").expect("vm"), "[7, 7]\n");
    // zero / negative repeat → empty
    assert_eq!(run_capture("print([1] * 0)").expect("vm"), "[]\n");
    assert_eq!(run_capture("print([1] * -2)").expect("vm"), "[]\n");
    // set algebra (insertion-order preserved)
    let setops = "a: Set[int] = {1, 2, 3}\nb: Set[int] = {2, 3, 4}\nprint(\"{a | b}\")\nprint(\"{a & b}\")\nprint(\"{a - b}\")\nprint(\"{a ^ b}\")\n";
    assert_eq!(
        run_capture(setops).expect("vm"),
        "{1, 2, 3, 4}\n{2, 3}\n{1}\n{1, 4}\n"
    );
}

/// Compound-assign forms (`+= *= |= &= ^= -=`) of the collection operators lower through the
/// same opcodes as the binary forms, so they must produce identical values — and stay in
/// lock-step across both engines.
#[test]
fn collection_compound_assign_eval_correct() {
    let cases = [
        ("xs := [1, 2]\nxs += [3, 4]\nprint(xs)", "[1, 2, 3, 4]\n"),
        ("xs := [7]\nxs *= 3\nprint(xs)", "[7, 7, 7]\n"),
        (
            "a: Set[int] = {1, 2, 3}\nb: Set[int] = {2, 3, 4}\na |= b\nprint(\"{a}\")",
            "{1, 2, 3, 4}\n",
        ),
        (
            "a: Set[int] = {1, 2, 3}\nb: Set[int] = {2, 3, 4}\na &= b\nprint(\"{a}\")",
            "{2, 3}\n",
        ),
        (
            "a: Set[int] = {1, 2, 3}\nb: Set[int] = {2, 3, 4}\na -= b\nprint(\"{a}\")",
            "{1}\n",
        ),
        (
            "a: Set[int] = {1, 2, 3}\nb: Set[int] = {2, 3, 4}\na ^= b\nprint(\"{a}\")",
            "{1, 4}\n",
        ),
    ];
    for (src, want) in cases {
        assert_eq!(
            run_capture(src).expect("vm"),
            want,
            "vm mismatch for `{src}`"
        );
        assert_eq!(
            run_capture_parallel(src).expect("interp"),
            want,
            "interp mismatch for `{src}`"
        );
    }
}

/// `[0] * n` with a huge `n` must raise a RECOVERABLE fault, not abort the process via a Vec
/// capacity-overflow panic — same checked-overflow policy as `str.repeat`, on both engines.
#[test]
fn list_repeat_capacity_overflow_is_recoverable_fault() {
    let src = "print([0] * 9223372036854775807)";
    let vm = run_capture(src).expect_err("vm: list repeat overflow should fault");
    assert_eq!(vm.message, "list repeat capacity overflow");
    let it = run_capture_parallel(src).expect_err("interp: list repeat overflow");
    assert_eq!(it.message, "list repeat capacity overflow");

    // Huge-but-representable count: passes the `isize::MAX` byte guard (1e17 * 16B = 1.6e18 <
    // isize::MAX ≈ 9.2e18) but the allocation itself is infeasible. Before the
    // `try_reserve_exact` guard this ABORTED the process (SIGABRT) instead of faulting.
    let huge = "print([0] * 100000000000000000)";
    let vm = run_capture(huge).expect_err("vm: huge list repeat should fault");
    assert_eq!(vm.message, "list repeat capacity overflow");
    let it = run_capture_parallel(huge).expect_err("interp: huge list repeat");
    assert_eq!(it.message, "list repeat capacity overflow");

    // Boundary: small repeats still work on both engines.
    assert_eq!(run_capture("print([0] * 3)").expect("vm"), "[0, 0, 0]\n");
    assert_eq!(
        run_capture_parallel("print([0] * 3)").expect("interp"),
        "[0, 0, 0]\n"
    );
}

/// `examples/evaluator.chz` — a full tokenizer + recursive-descent parser + AST evaluator with
/// `Result`/`?` error paths (bad char, unbalanced parens, trailing input, divide-by-zero).
#[test]
fn golden_evaluator_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/evaluator.chz");
    let expected = include_str!("../../examples/evaluator.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// `examples/ledger.chz` — account ledger: a map of mutable structs, overdraft `Result`s, a
/// `defer` closing line, `sort_by` ranking, and guarded comprehensions.
#[test]
fn golden_ledger_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/ledger.chz");
    let expected = include_str!("../../examples/ledger.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// M1 (tier-1) golden: `examples/string_iter.chz` (chars + iterable strings) byte-identical
/// on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_string_iter_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/string_iter.chz");
    let expected = include_str!("../../examples/string_iter.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Default + named arguments on free functions: `examples/default_args.chz` byte-identical on
/// the VM, the interpreter, and its `.expected`.
#[test]
fn golden_default_args_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/default_args.chz");
    let expected = include_str!("../../examples/default_args.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Default + named arguments on struct constructors: `examples/named_struct.chz` byte-identical
/// on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_named_struct_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/named_struct.chz");
    let expected = include_str!("../../examples/named_struct.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Gap #5 golden: `examples/map.chz` is byte-identical to its `.expected` on the VM,
/// and to the interpreter (the cross-engine acceptance bar for maps).
#[test]
fn golden_map_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/map.chz");
    let expected = include_str!("../../examples/map.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// M10-G1 golden: `examples/stringable.chz` (the `Stringable` protocol — `str(self)` dispatch
/// from print/str()/interpolation, nested too) byte-identical on the VM, interp, and `.expected`.
#[test]
fn golden_stringable_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/stringable.chz");
    let expected = include_str!("../../examples/stringable.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// M10-G3 golden: `examples/operators.chz` (operator overloading via `Add`/`Sub`/`Mul` + the
/// multi-bound `T: Add + Mul`) byte-identical on the VM, interp, and `.expected`.
#[test]
fn golden_operators_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/operators.chz");
    let expected = include_str!("../../examples/operators.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// M10-G3 golden: `examples/type_alias.chz` (transparent type aliases) byte-identical on the
/// VM, interp, and `.expected`.
#[test]
fn golden_type_alias_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/type_alias.chz");
    let expected = include_str!("../../examples/type_alias.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// G1 golden: `examples/generics.chz` (generics + structural `Comparable`) is byte-identical
/// on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_generics_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/generics.chz");
    let expected = include_str!("../../examples/generics.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// G2 golden: generic structs are byte-identical on the VM, interpreter, and `.expected`.
#[test]
fn golden_generic_structs_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/generic_structs.chz");
    let expected = include_str!("../../examples/generic_structs.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Tier-2 golden: generic enums (Tree[T] / Either[A, B]) — byte-identical VM, interp, expected.
#[test]
fn golden_generic_enum_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/generic_enum.chz");
    let expected = include_str!("../../examples/generic_enum.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Golden: real hash-table map/set with Hashable struct keys — byte-identical VM, interp, expected.
#[test]
fn golden_hashmap_keys_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/hashmap_keys.chz");
    let expected = include_str!("../../examples/hashmap_keys.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Tech-debt golden: `examples/explicit_type_args.chz` (explicit call-site type arguments on a
/// generic fn / struct / enum-variant constructor) byte-identical VM, interp, and `.expected`.
#[test]
fn golden_explicit_type_args_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/explicit_type_args.chz");
    let expected = include_str!("../../examples/explicit_type_args.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Type-side declaration-site turbofish (PART 1): `examples/turbofish_type_args.chz` exercises
/// `Box[int].Has(5)`, nullary `Box[int].Empty`, the 2-param `Pair[int, str].Both(…)` multi-arg
/// carrier, and a generic static `Box[int].empty()`. The change is purely in the checker's
/// resolution + the value's inferred type args (runtime is type-erased), so VM, interp, and the
/// `--parallel` engine must be byte-identical to `.expected`. Also asserts it type-checks clean.
#[test]
fn golden_turbofish_type_args_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/turbofish_type_args.chz");
    let expected = include_str!("../../examples/turbofish_type_args.expected");
    // The full type-checker accepts the program (the golden run path skips checking).
    let module = parser::parse(lexer::tokenize(src).expect("lex")).expect("parse");
    assert!(
        crate::checker::check(&module).is_ok(),
        "turbofish_type_args.chz must type-check clean"
    );
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected, "vm output drifted from .expected");
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("interp run"),
        "interp drifted from vm"
    );
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("parallel run"),
        "parallel drifted from vm"
    );
}

/// Member-side declaration-site turbofish (PART 2): `examples/turbofish_member_args.chz`
/// exercises a generic static method's OWN `[U]` inferred (`Box[int].make(5)`) AND via the
/// combined turbofish (`Box[int].make[str]("hi")` + the bare carrier `Box.make[str]`), an
/// instance method with multi type-arg + multi value-arg turbofish (`p.first[int, str](…)`),
/// AND the regression guard `arr[i].handlers[k](x)` / `arr[0].handlers[0](y)` (a fn-valued field
/// on an INDEXED receiver must stay ordinary index-then-call, not a member-turbofish). The change
/// is checker resolution + type-erased dispatch, so VM, interp, and `--parallel` must be
/// byte-identical to `.expected`. Also asserts it type-checks clean.
#[test]
fn golden_turbofish_member_args_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/turbofish_member_args.chz");
    let expected = include_str!("../../examples/turbofish_member_args.expected");
    let module = parser::parse(lexer::tokenize(src).expect("lex")).expect("parse");
    assert!(
        crate::checker::check(&module).is_ok(),
        "turbofish_member_args.chz must type-check clean"
    );
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected, "vm output drifted from .expected");
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("interp run"),
        "interp drifted from vm"
    );
    assert_eq!(
        vm_out,
        run_capture_parallel(src).expect("parallel run"),
        "parallel drifted from vm"
    );
}

/// Tech-debt golden: `examples/set_eq.chz` (order-independent set equality incl. nested in a
/// struct/list) byte-identical on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_set_eq_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/set_eq.chz");
    let expected = include_str!("../../examples/set_eq.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Golden: `examples/map_eq.chz` — map equality is order-independent (same key→value pairs
/// regardless of insertion order), incl. nested in a struct/list, byte-identical on the VM, the
/// interpreter, and its `.expected`. Pins the fix that made map `==` consistent with set `==`.
#[test]
fn golden_map_eq_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/map_eq.chz");
    let expected = include_str!("../../examples/map_eq.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Golden: `examples/cycle_guard.chz` — a cyclic data structure makes `print`/`==` a recoverable
/// `RuntimeError` (depth-guarded) instead of an uncatchable host stack overflow, and a
/// deep-but-acyclic structure still renders fine. Byte-identical on the VM, the interpreter, and
/// its `.expected`.
#[test]
fn golden_cycle_guard_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/cycle_guard.chz");
    let expected = include_str!("../../examples/cycle_guard.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// FLIPPED (item A, was `airlock_cyclic_struct_recoverable_both_engines`) — a SELF-REFERENTIAL value
/// crossing the concurrency airlock (`spawn` arg) now ROUND-TRIPS via identity-preserving container
/// serialization (`WireValue::Backref` on every container arm), instead of tripping the depth cap. The
/// spawned task reads `a.val == 1` (prints `got 1`) and the recover returns `done`; byte-identical on
/// the serial and M:N engines (both copy the local cyclic value via `deep_clone`→`to_wire`).
#[test]
fn airlock_cyclic_struct_crosses_both_engines() {
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
    r := recover:
        parallel:
            spawn use_it(a)
        \"done\"
    match r:
        Ok(v):  print(\"ok: {v}\")
        Err(e): print(\"caught: {e.message()}\")
main()
";
    assert_mc_parity(src, "got 1\nok: done\n");
}

/// FLIPPED (item A, was `airlock_cyclic_via_channel_send_and_shared_recoverable`) — the same self-
/// referential value crossing via `Channel.send` and `Shared(...)` (both route through `to_wire_at`)
/// also now ROUND-TRIPS. Regression lock on the other airlock entry points beyond a bare `spawn` arg.
#[test]
fn airlock_cyclic_via_channel_send_and_shared_crosses() {
    let chan_src = "\
import std.concurrency
struct Node:
    val: int
    next: List[Node]
fn main():
    a := Node(1, [])
    b := Node(2, [])
    a.next.push(b)
    b.next.push(a)
    r := recover:
        ch := Channel[Node]()
        ch.send(a)
        \"done\"
    match r:
        Ok(v):  print(\"ok: {v}\")
        Err(e): print(\"caught: {e.message()}\")
main()
";
    assert_mc_parity(chan_src, "ok: done\n");
    let shared_src = "\
import std.concurrency
struct Node:
    val: int
    next: List[Node]
fn main():
    a := Node(1, [])
    b := Node(2, [])
    a.next.push(b)
    b.next.push(a)
    r := recover:
        s := Shared(a)
        \"done\"
    match r:
        Ok(v):  print(\"ok: {v}\")
        Err(e): print(\"caught: {e.message()}\")
main()
";
    assert_mc_parity(shared_src, "ok: done\n");
}

/// A nested (local) recursive `fn` crossing the airlock has a self-referential capture graph (the
/// letrec self-cell: `Closure h -> captured[Cell] -> Cell.inner = h`). Identity-preserving airlock
/// serialization (`WireValue::Backref`, scoped to the Cell/Closure arms) ties that knot back together
/// on the far heap, so the recursive fn is now SENDABLE and computes correctly — `fact(5) == 120`,
/// byte-identical on both engines (serial `deep_clone`→`to_wire`, M:N spawn-block `to_wire`/`to_snap`).
#[test]
fn airlock_recursive_local_fn_round_trips_both_engines() {
    let src = "\
fn main():
    fn fact(n: int) -> int:
        if n <= 1: return 1
        return n * fact(n - 1)
    ch := Channel[int]()
    parallel:
        spawn: ch.send(fact(5))
    print(\"ok: {ch.recv()}\")
main()
";
    assert_mc_parity(src, "ok: 120\n");
}

/// Memory-safety lock for the tie-the-knot reconstruction: the same recursive-fn airlock crossing under
/// GC STRESS. The reconstructed closure/cell cycle (placeholder-alloc → patch) must be fully GC-traced —
/// if the placeholder patch left a dangling `GcRef`, a collection at the next safepoint would panic
/// ("dangling GcRef"). A GC pass runs before/after the spawn, then the result is used (`fact(6) == 720`).
#[test]
fn airlock_recursive_local_fn_round_trips_under_gc_stress() {
    let src = "\
fn main():
    fn fact(n: int) -> int:
        if n <= 1: return 1
        return n * fact(n - 1)
    junk := [1, 2, 3, 4, 5]
    ch := Channel[int]()
    parallel:
        spawn: ch.send(fact(6))
    got := ch.recv()
    more := [got, got]
    print(\"ok: {got}\")
main()
";
    assert_eq!(run_capture_stress(src), "ok: 720\n");
    assert_eq!(run_capture_stress(src), run(src));
}

/// Memory-safety lock for the CONTAINER tie-the-knot reconstruction (item A): a self-referential
/// `struct` crossing the airlock under GC STRESS. The reconstructed struct/list cycle (placeholder-
/// alloc → patch) must be fully GC-traced — if the placeholder patch left a dangling `GcRef`, a
/// collection at the next safepoint would panic ("dangling GcRef"). A GC pass runs before/after the
/// spawn, then the crossed cyclic node is read (`a.val == 1`, and the cycle is intact: `a.next[0]` is
/// `b` and `b.next[0]` is `a`). Mirrors `airlock_recursive_local_fn_round_trips_under_gc_stress`.
#[test]
fn airlock_self_ref_struct_round_trips_under_gc_stress() {
    let src = "\
struct Node:
    val: int
    next: List[Node]
fn use_it(n: Node):
    print(\"ok: {n.val} {n.next[0].val} {n.next[0].next[0].val}\")
fn main():
    junk := [1, 2, 3, 4, 5]
    a := Node(1, [])
    b := Node(2, [])
    a.next.push(b)
    b.next.push(a)
    ch := Channel[int]()
    parallel:
        spawn use_it(a)
    more := [a, b]
main()
";
    assert_eq!(run_capture_stress(src), "ok: 1 2 1\n");
    assert_eq!(run_capture_stress(src), run(src));
}

/// W7-11 — an `RwShared` copy-out view of an element whose cycle closes through the ROOT container
/// used to ABORT THE HOST: the piece rebuild hit `from_wire_memo`'s
/// `.expect("a wire Backref always targets an already-reconstructed node id")` on a legal,
/// single-threaded, checker-clean program, while `get()` on the same box worked. `elem_split` cannot
/// cover it — it re-emits CELL definitions per depth-1 subtree, and the missing node is a CONTAINER.
///
/// **This test's failure mode is a dead process, not a red assert** — which is exactly why it is in
/// Rust and not only in `tests/chz/`: a regression takes libtest down with it.
///
/// The expected output is CPython's, measured on the same shape:
/// ```text
/// b = copy.deepcopy(xs[0]); b.val = 42; b.next[0].val            -> 42
/// b.next[0].next[0].next[0].val                                  -> 42
/// ```
#[test]
fn rwshared_view_of_a_container_cycling_element_does_not_abort_the_host() {
    let src = "\
import std.concurrency
struct Node:
    val: int
    back: List[Node]
fn main():
    a := Node(1, [])
    xs := [a, Node(2, [])]
    a.back = xs
    s := RwShared(xs)
    print(\"get {s.get()[0].val}\")
    match s.at(0):
        Some(e):
            print(\"walk {e.back[0].back[0].back[0].val}\")
            e.val = 42
            print(\"identity {e.back[0].val} {e.back[0].back[0].back[0].val}\")
        None: print(\"WRONG: at(0) was None\")
    match s.at(9):
        Some(v): print(\"WRONG: at(9) was Some({v.val})\")
        None: print(\"oob None\")
main()
";
    assert_mc_parity(src, "get 1\nwalk 1\nidentity 42 42\noob None\n");
}

/// W7-11 under GC STRESS — the fallback rebuilds the WHOLE container and hands back one node out of
/// it, so the returned element's own cycle is the only thing keeping the rest reachable. If that
/// rooting were wrong, a collection between the rebuild and the caller's use would surface here
/// (dangling `GcRef` panic or a wrong value), not in the plain run above.
#[test]
fn rwshared_cyclic_view_round_trips_under_gc_stress() {
    let src = "\
import std.concurrency
struct Node:
    val: int
    back: List[Node]
fn main():
    junk := [1, 2, 3, 4, 5]
    a := Node(1, [])
    xs := [a, Node(2, [])]
    a.back = xs
    s := RwShared(xs)
    match s.at(0):
        Some(e): print(\"ok: {e.val} {e.back[0].back[0].val} {e.back.len()}\")
        None: print(\"WRONG\")
    more := [a]
main()
";
    assert_eq!(run_capture_stress(src), "ok: 1 1 2\n");
    assert_eq!(run_capture_stress(src), run(src));
}

/// W7-4 REVIEW (perf cliff, regression lock) — an `RwShared` read VIEW must stay O(element), never
/// O(whole container). The first cut resolved a piece's cross-element `Backref` by rebuilding the
/// ENTIRE stored container once PER ELEMENT, so `for_each`/`fold`/`at` over a container of closures
/// sharing one binding went quadratic (measured on the pre-fix release binary: n=4000 → 3.7s, n=12000
/// → 34s, versus 0.02s before W7-4). Stored wires are now self-contained per element, so no view ever
/// re-materializes the whole. A coarse CLIFF detector, not a benchmark: the budget is ~50× the actual
/// debug-build cost and the pre-fix code blew it (measured: 10.5s debug, versus 0.03s fixed).
#[test]
fn rwshared_view_over_shared_bindings_is_not_quadratic() {
    let src = "\
import std.concurrency
fn main():
    n := 0
    fn inc() -> int:
        n = n + 1
        return n
    fs: List[fn() -> int] = [inc]
    for i in range(0, 3000):
        fs.push(inc)
    s := RwShared(fs)
    c := 0
    fn tick(f: fn() -> int):
        c = c + 1
    s.for_each(tick)
    print(c)
main()
";
    let t = std::time::Instant::now();
    assert_eq!(run_capture(src).unwrap(), "3001\n");
    let el = t.elapsed();
    assert!(
        el < std::time::Duration::from_secs(5),
        "RwShared.for_each over 3001 sibling-binding closures took {el:?} — the view is materializing \
         the whole container per element again"
    );
}

/// W7-4 memory-safety lock for the module-scoped REBUILD MAP: `fault_module` now keeps one wire-`id`
/// → `GcRef` map alive ACROSS the whole `module_define` loop (so two globals over one captured local
/// rebuild ONE cell). A `GcRef` parked in that map between globals must stay rooted — if it did not, a
/// collection would panic ("dangling GcRef") or the second global would tie to a recycled slot. Run it
/// under GC STRESS with junk allocation around the crossing, and assert the shared binding actually
/// held (`2`, not `0`) as well as `stress == non-stress`.
#[test]
fn airlock_module_global_shared_binding_survives_gc_stress() {
    let src = "\
struct Ctr:
    inc: fn() -> nil
    get: fn() -> int
fn make() -> Ctr:
    n := 0
    fn inc():
        n = n + 1
    fn get() -> int:
        return n
    return Ctr(inc, get)
c := make()
gi := c.inc
gg := c.get
pad := [1, 2, 3, 4, 5]
fn main():
    junk := [1, 2, 3]
    r := Channel[int]()
    parallel:
        spawn:
            gi()
            gi()
            r.send(gg())
    more := [junk, junk]
    print(\"ok: {r.recv()}\")
main()
";
    assert_eq!(run_capture_stress(src), "ok: 2\n");
    assert_eq!(run_capture_stress(src), run(src));
}

/// W7-4a — cell identity across MODULES. `k.C` holds two sibling closures over one factory-local
/// `n`; `l.GI` and `main.GG` are globals in two DIFFERENT modules pointing at them. A memo per module
/// minted a fresh id per module, so the task rebuilt two cells and its `l.GI()` writes were invisible
/// to `GG()` — `0`, where CPython (`import pk, pl` + `threading.Thread`) and Go (two packages +
/// a goroutine) both measure `2`. One snapshot-wide `WireMemo` + one `Vm`-lived rebuild map fixes it.
#[test]
fn airlock_cross_module_shared_binding_is_one_cell() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w74a_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("k.chz"),
        "\
struct Ctr:
    inc: fn() -> nil
    get: fn() -> int
fn make() -> Ctr:
    n := 0
    fn inc():
        n = n + 1
    fn get() -> int:
        return n
    return Ctr(inc, get)
C := make()
",
    )
    .unwrap();
    std::fs::write(dir.join("l.chz"), "import k\nGI := k.C.inc\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "\
import k
import l
GG := k.C.get
fn main():
    r := Channel[int]()
    parallel:
        spawn:
            l.GI()
            l.GI()
            r.send(GG())
    print(r.recv())
main()
",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    // Modules fault in LAZILY, so a cell built by `k`'s fault sits in the `Vm`-lived rebuild map
    // across real safepoints before `l`'s and `main`'s faults tie to it. (The map is also rooted by
    // `collect`; that root is belt-and-braces today — this test still passes without it, because
    // every entry is reachable from the global it was `module_define`d into. It is the LAZY-FAULT
    // window this run locks down, not the root line.)
    let (stress_out, _se, stress_res, _) = run_file_stress(&entry, true);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vm_res.is_ok(), "serial faulted: {vm_res:?}");
    assert!(par_res.is_ok(), "M:N faulted: {par_res:?}");
    assert!(stress_res.is_ok(), "gc-stress faulted: {stress_res:?}");
    assert_eq!(
        vm_out, "2\n",
        "cross-module sibling closures split their cell"
    );
    assert_eq!(
        par_out, "2\n",
        "cross-module sibling closures split their cell (M:N)"
    );
    assert_eq!(
        stress_out, "2\n",
        "the snapshot rebuild map is not GC-rooted"
    );
}

/// W7-4b — cell identity on the SNAPSHOT SLOW ARM. `p := [k]` holds a module, so the whole global
/// fails `to_snap`'s `!has_handle()` fast lane (only `Obj::Module` still forces this — `Native`/
/// `Cffi`/`Builtin` all cross by value now) and its cell lands in `SnapValue::Cell`, which carried no
/// id: the two sibling closures rebuilt two cells and `GC()` read `1` where CPython (`p = [bk]` +
/// `threading.Thread`) measures `3`. Fixed by giving `SnapValue` the same id/`Backref` encoding the
/// wire arms have, drained by the same rebuild map.
#[test]
fn airlock_handle_bearing_cell_keeps_one_binding() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w74b_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("k.chz"), "V := 41\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "\
import k
struct Sw:
    add: fn() -> nil
    count: fn() -> int
fn make() -> Sw:
    p := [k]
    fn add():
        p = p + [k]
    fn count() -> int:
        return p.len()
    return Sw(add, count)
C := make()
GA := C.add
GC := C.count
fn main():
    r := Channel[int]()
    parallel:
        spawn:
            GA()
            GA()
            r.send(GC())
    print(r.recv())
main()
",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let (stress_out, _se, stress_res, _) = run_file_stress(&entry, true);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vm_res.is_ok(), "serial faulted: {vm_res:?}");
    assert!(par_res.is_ok(), "M:N faulted: {par_res:?}");
    assert!(stress_res.is_ok(), "gc-stress faulted: {stress_res:?}");
    assert_eq!(
        vm_out, "3\n",
        "a handle-bearing cell split its binding on the snapshot slow arm"
    );
    assert_eq!(par_out, "3\n", "…and on M:N");
    assert_eq!(stress_out, "3\n", "…and under GC stress");
}

/// W7-4b's second, unplanned fix — a RECURSIVE local `fn` whose captures embed a module used to abort
/// the whole spawn with `maximum structural depth (10000) exceeded (cyclic data structure?)`. The
/// self-cell cycle only reaches the `SnapValue` slow arm when the closure ALSO holds a handle, and
/// that arm had no `Backref`, so the walk ran the cycle to the shared depth cap and "rejected
/// cleanly" — a fault on a program CPython runs fine (`m = bk` + the same recursive `down`, printing
/// `41`). Giving `SnapValue` the id/`Backref` encoding terminates the walk the way the wire path
/// always did. Measured on the pre-fix binary (rc=1) and post-fix (`41`, both engines).
#[test]
fn airlock_handle_bearing_recursive_local_fn_round_trips() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w74b_cyc_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("k.chz"), "V := 41\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "\
import k
fn make() -> fn(int) -> int:
    m := k
    fn down(n: int) -> int:
        if n <= 0:
            return m.V
        return down(n - 1)
    return down
G := make()
fn main():
    r := Channel[int]()
    parallel:
        spawn:
            r.send(G(3))
    print(r.recv())
main()
",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(
        vm_res.is_ok(),
        "serial faulted (depth cap is back?): {vm_res:?}"
    );
    assert!(
        par_res.is_ok(),
        "M:N faulted (depth cap is back?): {par_res:?}"
    );
    assert_eq!(vm_out, "41\n");
    assert_eq!(par_out, "41\n");
}

/// W7-4c — the shared rebuild map must not let the two crossings disagree about a binding's VALUE.
/// `from_wire_memo`'s `Cell` arm is FIRST-WINS, and the two writers can hold DIFFERENT values for one
/// binding: a write THROUGH a cell does not drop `snapshot_memo` (only a module-SLOT write does), so
/// the cached snapshot below still carries `0` while the second spawn's clone carries `1`.
///
/// Serial eager-faults every module BEFORE rebuilding the task, M:N rebuilds first and faults lazily
/// — so whoever wrote first won, and that differed by engine: serial printed `0`, M:N `1`, where
/// CPython measures `1` and serial's own pre-W7-4c answer was `1`. Found by adversarial review, with
/// that repro. Fixed by rebuilding the task's crossing FIRST on both engines (the clone is the
/// correct value — a task sees the binding as of its own spawn).
#[test]
fn airlock_shared_cell_takes_the_spawn_time_value_on_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w74c_val_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "\
struct Ctr:
    inc: fn() -> nil
    get: fn() -> int
fn make() -> Ctr:
    n := 0
    fn inc():
        n = n + 1
    fn get() -> int:
        return n
    return Ctr(inc, get)
C := make()
I := C.inc
fn go():
    gg := C.get
    r := Channel[int](1)
    parallel:
        spawn:
            pass
        I()
        spawn:
            r.send(gg())
    print(r.recv())
go()
",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vm_res.is_ok(), "serial faulted: {vm_res:?}");
    assert!(par_res.is_ok(), "M:N faulted: {par_res:?}");
    assert_eq!(
        vm_out, "1\n",
        "serial kept the STALE cached-snapshot cell value"
    );
    assert_eq!(par_out, "1\n");
    assert_eq!(
        vm_out, par_out,
        "serial and M:N disagreed on a shared binding's value"
    );
}

/// W7-4a — a DISCARDED speculative wire attempt must not forge a `Backref`. Found by adversarial
/// review, and it was a live host PANIC, not a theoretical one: `main.chz` below aborted the M:N task
/// with `CellLoad on a non-handle value` while `--serial` printed `7`.
///
/// Mechanism: `H := (k, k.C.get)` embeds a module, so the tuple fails `to_snap`'s `!has_handle()`
/// fast lane — but the attempt ALREADY marked `k.C.get`'s cell as emitted, and that cell's id was
/// minted back when module `k` was walked. `try_wire_speculative`'s rollback pruned `emitted` by
/// `id >= mint_from`, which cannot see an id from an earlier module, so the marking survived a walk
/// whose output was thrown away. `GG := k.C.get` (the next global) then emitted `Backref(id)` into a
/// module that never wrote the definition. Ordering matters: the handle-bearing global must come
/// FIRST — move `H` below `GG` and it prints `7` either way.
#[test]
fn airlock_discarded_wire_attempt_does_not_forge_a_backref() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w74a_spec_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("k.chz"),
        "\
struct Ctr:
    inc: fn() -> nil
    get: fn() -> int
fn make() -> Ctr:
    n := 7
    fn inc():
        n = n + 1
    fn get() -> int:
        return n
    return Ctr(inc, get)
C := make()
",
    )
    .unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "\
import k
H := (k, k.C.get)
GG := k.C.get
fn main():
    r := Channel[int]()
    parallel:
        spawn:
            r.send(GG())
    print(r.recv())
main()
",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vm_res.is_ok(), "serial faulted: {vm_res:?}");
    assert!(
        par_res.is_ok(),
        "M:N faulted — a discarded attempt forged a Backref: {par_res:?}"
    );
    assert_eq!(vm_out, "7\n");
    assert_eq!(par_out, "7\n", "serial and M:N diverged");
}

/// Control (regression lock, rc=0) — the SAME recursive `fn` HOISTED to module scope IS sendable: it
/// crosses as `Obj::Func` (no captures; recursion resolves via its home-global slot), never entering
/// the `Obj::Closure` serialization arm, so the new self-ref diagnostic never fires and the send works.
#[test]
fn airlock_module_global_recursive_fn_control_sends() {
    let src = "\
fn fact(n: int) -> int:
    if n <= 1: return 1
    return n * fact(n - 1)
fn main():
    ch := Channel[int]()
    parallel:
        spawn: ch.send(fact(5))
    print(ch.recv())
main()
";
    assert_mc_parity(src, "120\n");
}

/// FLIPPED (was `airlock_cyclic_module_global_recoverable_mn`): a cyclic MODULE-GLOBAL value
/// snapshotted for an M:N worker (`to_snap`) now CROSSES via the `to_wire && !has_handle` fast path —
/// once containers are identity-preserved, the cyclic `Node` serializes with a `Backref` instead of
/// tripping the depth cap, and `to_snap` rides that fast path. Self-referential module-global data now
/// crosses exactly like local self-referential data (the consistent, no-drift behavior). This path is
/// M:N-ONLY: the serial engine never snapshots module globals, so this is a `run_capture_parallel`-only
/// assertion (NOT `assert_mc_parity`). The `parallel:` nursery forces worker-prep (`snapshot_modules` →
/// `to_snap`) to walk the cyclic global; the worker prints `w`, then the recover returns `done`.
#[test]
fn airlock_cyclic_module_global_crosses_mn() {
    let src = "\
struct Node:
    val: int
    next: List[Node]
a := Node(1, [])
b := Node(2, [])
a.next.push(b)
b.next.push(a)
fn worker():
    print(\"w\")
fn main():
    r := recover:
        parallel:
            spawn worker()
        \"done\"
    match r:
        Ok(v):  print(\"ok: {v}\")
        Err(e): print(\"caught: {e.message()}\")
main()
";
    let out = run_capture_parallel(src).expect("M:N run should not crash");
    assert_eq!(out, "w\nok: done\n");
}

/// Bug A guard-precision — a BREADTH-shaped acyclic value (~100k shallow siblings, structural depth
/// ~2) must STILL cross the airlock fine: the 10_000 depth cap counts nesting LEVELS, not siblings,
/// so a large-but-shallow sendable never false-trips. (A genuinely >10_000-DEEP acyclic nest would
/// error — same false-positive the print/`==` path already has — so this is deliberately breadth-shaped.)
#[test]
fn airlock_wide_acyclic_crosses_fine() {
    let src = "\
fn use_it(xs: List[int]):
    print(\"len {xs.len()} first {xs[0]} last {xs[99999]}\")
fn main():
    xs := [0]
    for i in 1..100000:
        xs.push(i)
    parallel:
        spawn use_it(xs)
main()
";
    assert_mc_parity(src, "len 100000 first 0 last 99999\n");
}

/// Golden: `examples/airlock_cycle.chz` — a SELF-REFERENTIAL sendable now ROUND-TRIPS across the
/// airlock (`spawn` arg / `Channel.send` / `Shared`) via identity-preserving container serialization,
/// and a wide-acyclic sendable still crosses fine (the depth cap stays the acyclic-nesting backstop).
/// All sections use LOCAL self-referential values (the `to_wire` path), so the output is byte-identical
/// on the serial and M:N engines.
#[test]
fn golden_airlock_cycle_chz_matches_expected() {
    let src = include_str!("../../examples/airlock_cycle.chz");
    let expected = include_str!("../../examples/airlock_cycle.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("M:N run"));
}

/// Tech-debt parity: a `set` nested inside a struct / list must compare unordered on BOTH
/// engines (top-level set `==` already did). Previously the interp's derived `SetData::eq` was
/// order-sensitive, so `W(Set([1,2,3])) == W(Set([3,2,1]))` was `true` on the VM but `false` on
/// the interp.
#[test]
fn nested_set_equality_parity() {
    let src = "\
struct W:
    s: Set[int]
a := W(Set([1, 2, 3]))
b := W(Set([3, 2, 1]))
print(a == b)
print([Set([1, 2])] == [Set([2, 1])])
";
    let vm = run_capture(src).expect("vm");
    let interp = run_capture_parallel(src).expect("interp");
    assert_eq!(vm, interp);
    assert_eq!(vm, "true\ntrue\n");
}

#[test]
fn sort_over_comparable_structs_on_vm() {
    let src = "\
struct P:
    n: int
    t: str
    fn compare(self, o: P) -> int:
        return self.n - o.n
    fn eq(self, o: P) -> bool:
        return self.n == o.n
    fn show(self) -> str:
        return self.t + str(self.n)
xs := [P(3, \"c\"), P(1, \"a\"), P(2, \"b\"), P(1, \"z\")]
xs.sort()
for x in xs:
    print(x.show())
";
    assert_eq!(run(src), "a1\nz1\nb2\nc3\n");
}

#[test]
fn struct_ordering_dispatches_to_compare_on_vm() {
    let src = "\
struct P:
    n: int
    fn compare(self, other: P) -> int:
        return self.n - other.n
    fn eq(self, other: P) -> bool:
        return self.n == other.n
print(P(1) < P(2))
print(P(2) < P(1))
print(P(5) >= P(5))
";
    assert_eq!(run(src), "true\nfalse\ntrue\n");
}

/// `Contains` operator protocol (L5): `x in some_struct` dispatches to a user
/// `contains(self, item) -> bool` method on BOTH engines. A checker-only change would `check` OK
/// then trap at runtime in `op_contains`'s reject arm — so this RUN test is the safety net.
#[test]
fn contains_protocol_struct_dispatches() {
    let src = "\
struct Bag:
    items: List[int]
    fn contains(self, x: int) -> bool:
        for it in self.items:
            if it == x:
                return true
        return false
b := Bag([1, 2, 3])
print(2 in b)
print(9 in b)
";
    assert_mc_parity(src, "true\nfalse\n");
}

/// `Contains` on a generic `Box[T]` — the `contains` param type must be the INSTANTIATED `int`,
/// and BOTH engines must lower the generic instantiation to the method call.
#[test]
fn contains_generic_box_runs() {
    let src = "\
struct Box[T]:
    v: T
    fn contains(self, x: T) -> bool:
        return x == self.v
b := Box[int](2)
print(2 in b)
print(3 in b)
";
    assert_mc_parity(src, "true\nfalse\n");
}

/// `Contains` also dispatches on enums (protocol-satisfaction machinery already covers them).
#[test]
fn contains_protocol_enum_dispatches() {
    let src = "\
enum Dir:
    N
    S
    fn contains(self, x: int) -> bool:
        return x == 0
d := Dir.N
print(0 in d)
print(1 in d)
";
    assert_mc_parity(src, "true\nfalse\n");
}

/// `in` resolves through a `Contains[T]` generic bound — the exact analog of `<` through a
/// `Comparable` bound. `contains_item_ty` recovers the item type from the bound; at runtime the
/// value is a concrete monomorphized struct/enum, so BOTH engines dispatch via `op_contains`.
#[test]
fn contains_through_generic_bound_runs() {
    let src = "\
struct Bag:
    xs: List[int]
    fn contains(self, x: int) -> bool:
        for e in self.xs:
            if e == x:
                return true
        return false
fn has[C: Contains[int]](c: C, n: int) -> bool:
    return n in c
print(has(Bag([4, 5, 6]), 5))
print(has(Bag([4, 5, 6]), 9))
";
    assert_mc_parity(src, "true\nfalse\n");
}

#[test]
fn primitive_compare_method_on_vm() {
    let src = "fn c[T: Comparable](a: T, b: T) -> int:\n    return a.compare(b)\nprint(c(2, 5))\nprint(c(5, 2))\n";
    assert_eq!(run(src), "-1\n1\n");
}

/// Scalars intrinsically satisfy `Stringable` (checker arm in `proto.rs`) AND the erased generic
/// body's `v.str()` is dispatched by the scalar `str` branch in `do_method_call` — a checker-only
/// change would type-OK then runtime-trap "type int has no method 'str'". Covers all four scalars
/// (int/float/bool/str; the T=str arm exercises the already-`Obj::Str` receiver re-alloc). Parity:
/// serial-VM == M:N-VM (both share `do_method_call`).
#[test]
fn primitive_str_method_on_vm() {
    let src = "fn show[T: Stringable](v: T) -> str:\n    return v.str()\nprint(show(5))\nprint(show(3.14))\nprint(show(true))\nprint(show(\"hi\"))\n";
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, "5\n3.14\ntrue\nhi\n");
    assert_eq!(out, run_capture_parallel(src).expect("M:N run"));
}

/// Gap #11 golden: `examples/sort_by.chz` (custom comparators, stable order, tuple-field sort)
/// is byte-identical on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_sort_by_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/sort_by.chz");
    let expected = include_str!("../../examples/sort_by.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Call-flattening guarantee: deep *plain-function* recursion no longer consumes host Rust stack
/// (frames live in the heap `frames` `Vec`, not via a per-call `run_until` recursion), so it runs
/// to completion on a stack far below the production 256 MiB `VM_STACK_BYTES`. Before flattening,
/// the VM recursed ~25 KiB of host stack per call and overflowed a 1 MiB stack (an uncatchable
/// abort). Depth stays well under `MAX_CALL_DEPTH` (10_000). Parity: same value on the interpreter.
#[test]
fn deep_plain_recursion_runs_on_small_host_stack() {
    let src = "\
fn sum_to(n: int) -> int:
    if n <= 0:
        return 0
    return n + sum_to(n - 1)

print(sum_to(5000))
";
    let out = super::run_capture_on_stack(src, 1024 * 1024)
        .expect("deep plain recursion should run on a 1 MiB host stack after call-flattening");
    assert_eq!(out, "12502500\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp run"));
}

/// M19 — guards the `run_until` per-entry program borrow (the hoisted `Arc::clone` →
/// raw-pointer) across the native-reentry paths that re-enter `run_until`: HOF callbacks
/// (`map`/`fold` closures), an operator-overload `compare` (`<` on a struct), and a
/// `defer` unwinding through a recursive call. If the raw pointer ever dangled across a
/// re-entry or resume, VM output would diverge from the interpreter.
#[test]
fn native_reentry_hof_compare_defer_parity() {
    let src = "\
struct P:
    v: int
    fn compare(self, other: P) -> int:
        return self.v - other.v
    fn eq(self, other: P) -> bool:
        return self.v == other.v

fn leave(n: int):
    print(\"leave {n}\")

fn rec(n: int) -> int:
    defer leave(n)
    if n <= 0:
        return 0
    doubled := [1, 2, 3].map(fn(x: int) -> int: x * n)
    s := doubled.fold(0, fn(a: int, x: int) -> int: a + x)
    if P(n) < P(n + 1):
        s = s + rec(n - 1)
    return s

print(rec(3))
";
    let out = run_capture(src).expect("vm run");
    assert_eq!(out, "leave 0\nleave 1\nleave 2\nleave 3\n36\n");
    assert_eq!(out, run_capture_parallel(src).expect("interp run"));
}

/// Gap #10 golden: `examples/cipher.chz` (ord/chr — ROT13 + manual digit parsing) is
/// byte-identical on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_cipher_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/cipher.chz");
    let expected = include_str!("../../examples/cipher.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Gap #14 (+ #11) golden: `examples/word_freq.chz` iterates a map with `for w, c in counts`
/// and ranks tuples with `sort_by`. Byte-identical on the VM, the interpreter, and `.expected`.
#[test]
fn golden_word_freq_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/word_freq.chz");
    let expected = include_str!("../../examples/word_freq.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Gap #15 golden: `examples/match_nested.chz` (tuple patterns, nested `Some((a, b))`, nested
/// literals) is byte-identical on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_match_nested_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/match_nested.chz");
    let expected = include_str!("../../examples/match_nested.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// L2 golden: `examples/match_struct.chz` (struct positional destructuring in `match` — single-arm
/// exhaustive, literal fields + a whole-value catch-all binding, a generic `Box[int]` field, and a
/// nested `Line(Point(..), Point(..))`) is byte-identical on both VM engines and its `.expected`.
#[test]
fn golden_match_struct_chz_matches_expected_and_parity() {
    let src = include_str!("../../examples/match_struct.chz");
    let expected = include_str!("../../examples/match_struct.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("M:N run"));
}

/// Match-guard golden: `examples/match_guard.chz` (`pattern if cond:` arms, expr + stmt forms)
/// is byte-identical on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_match_guard_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/match_guard.chz");
    let expected = include_str!("../../examples/match_guard.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Range-pattern golden: `examples/match_range.chz` (half-open `start..end` int patterns) is
/// byte-identical on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_match_range_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/match_range.chz");
    let expected = include_str!("../../examples/match_range.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Gap #13 golden: `examples/bits.chz` (`& | ^ << >>` — XOR-fold + bitmask) is byte-identical
/// on the VM, the interpreter, and its `.expected`.
#[test]
fn golden_bits_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/bits.chz");
    let expected = include_str!("../../examples/bits.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

/// Round-2 probe goldens: recursive data-structure + evaluator programs that surfaced the
/// round-2 gaps. Byte-identical on the VM, the interpreter, and their `.expected`.
#[test]
fn golden_bst_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/bst.chz");
    let expected = include_str!("../../examples/bst.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

#[test]
fn golden_linked_list_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/linked_list.chz");
    let expected = include_str!("../../examples/linked_list.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

#[test]
fn golden_calc_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/calc.chz");
    let expected = include_str!("../../examples/calc.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

// ----- struct iterator protocol (`for x in s` driven by `next(self) -> Option[T]`) -----

#[test]
fn for_over_struct_iterator_counts() {
    let src = "struct Counter:\n    n: int\n    limit: int\n    fn next(self) -> Option[int]:\n        if self.n >= self.limit:\n            return None\n        v := self.n\n        self.n = self.n + 1\n        return Some(v)\nfn main():\n    for x in Counter(0, 5):\n        print(x)\nmain()\n";
    assert_eq!(run(src), "0\n1\n2\n3\n4\n");
    assert_eq!(run(src), run_capture_parallel(src).expect("interp run"));
}

#[test]
fn for_over_struct_iterator_break_lazy() {
    let src = "struct Fib:\n    a: int\n    b: int\n    fn next(self) -> Option[int]:\n        v := self.a\n        nb := self.a + self.b\n        self.a = self.b\n        self.b = nb\n        return Some(v)\nfn main():\n    for x in Fib(0, 1):\n        if x > 10:\n            break\n        print(x)\nmain()\n";
    assert_eq!(run(src), "0\n1\n1\n2\n3\n5\n8\n");
    assert_eq!(run(src), run_capture_parallel(src).expect("interp run"));
}

/// Golden: the iterator example runs on the VM with exactly the expected output, matching interp.
#[test]
fn golden_iterator_chz_matches_expected_and_interp() {
    let src = include_str!("../../examples/iterator.chz");
    let expected = include_str!("../../examples/iterator.expected");
    let vm_out = run_capture(src).expect("vm run");
    assert_eq!(vm_out, expected);
    assert_eq!(vm_out, run_capture_parallel(src).expect("interp run"));
}

// ----- cyclic-data structural-depth guard + order-independent map == -----

#[test]
fn cyclic_print_errors_not_crashes() {
    let src = "\
struct Node:
    next: List[Node]
a := Node([])
b := Node([])
a.next.push(b)
b.next.push(a)
print(a)
";
    assert!(
        run_err(src).contains("maximum structural depth"),
        "expected structural-depth error"
    );
}

#[test]
fn cyclic_equality_errors_not_crashes() {
    let src = "\
struct Node:
    next: List[Node]
a := Node([])
b := Node([])
a.next.push(b)
b.next.push(a)
c := Node([])
d := Node([])
c.next.push(d)
d.next.push(c)
print(a == c)
";
    assert!(
        run_err(src).contains("maximum structural depth"),
        "expected structural-depth error"
    );
}

#[test]
fn cyclic_print_is_recoverable() {
    let src = "\
struct Node:
    next: List[Node]
a := Node([])
b := Node([])
a.next.push(b)
b.next.push(a)
r := recover:
    print(a)
match r:
    Ok(v): print(\"ok\")
    Err(e): print(\"caught: {e.message()}\")
";
    let out = run(src);
    assert!(
        out.contains("caught: maximum structural depth"),
        "expected recovered error, got {out:?}"
    );
}

#[test]
fn deep_acyclic_structure_ok() {
    let src = "\
x := [0]
i := 0
while i < 100:
    x = [x]
    i = i + 1
y := [0]
j := 0
while j < 100:
    y = [y]
    j = j + 1
print(x == y)
";
    assert_eq!(run(src), "true\n");
}

// ---- Bug E: runtime faults inside string interpolation report the real source line ----

/// Assert a runtime fault reports `line == expected_line` on BOTH engines (serial + M:N).
fn assert_fault_line(src: &str, needle: &str, expected_line: u32) {
    let e = run_capture(src).unwrap_err();
    assert!(
        e.message.contains(needle),
        "serial: expected message to contain {needle:?}, got {:?}",
        e.message
    );
    assert_eq!(
        e.span.line, expected_line,
        "serial: expected line {expected_line}, got {} (col {})",
        e.span.line, e.span.col
    );
    let ep = run_capture_parallel(src).unwrap_err();
    assert!(
        ep.message.contains(needle),
        "M:N: expected message to contain {needle:?}, got {:?}",
        ep.message
    );
    assert_eq!(
        ep.span.line, expected_line,
        "M:N: expected line {expected_line}, got {} (col {})",
        ep.span.line, ep.span.col
    );
}

/// Like [`assert_fault_line`] but pins the COLUMN too — the axis M24-6 was about. A binary
/// expression's span is its LEFT OPERAND's first char (`cc := 10 / b` faults at the `1`), so the
/// expected column is hand-counted to there.
fn assert_fault_at(src: &str, needle: &str, expected: (u32, u32)) {
    assert_fault_line(src, needle, expected.0);
    let e = run_capture(src).unwrap_err();
    assert_eq!((e.span.line, e.span.col), expected, "serial");
    let ep = run_capture_parallel(src).unwrap_err();
    assert_eq!((ep.span.line, ep.span.col), expected, "M:N");
}

#[test]
fn interpolation_div_by_zero_reports_real_line() {
    let src =
        "print(\"line 1\")\nprint(\"line 2\")\nb := 0\nmsg := \"result = {10 / b}\"\nprint(msg)\n";
    assert_fault_line(src, "division by zero", 4);
}

#[test]
fn interpolation_index_oob_reports_real_line() {
    let src = "print(\"line 1\")\nprint(\"line 2\")\nxs := [1, 2, 3]\nmsg := \"val = {xs[9]}\"\nprint(msg)\n";
    assert_fault_line(src, "index", 4);
}

#[test]
fn interpolation_overflow_reports_real_line() {
    let src = "print(\"line 1\")\nprint(\"line 2\")\nbig := 9223372036854775807\nmsg := \"val = {big * big}\"\nprint(msg)\n";
    assert_fault_line(src, "overflow", 4);
}

#[test]
fn interpolation_multiline_reports_the_fragments_real_line() {
    // The triple-quoted string OPENS on line 4, but the faulting fragment physically sits on line
    // 6 — and that is what gets reported, column included. This used to report the opening line
    // (4) and a column that kept counting from the literal's start, because `raw` is post-escape
    // and a `\n` escape was indistinguishable from a real newline. The lexer now carries a
    // content-index → source-position map, so the two ARE distinguishable (`docs/gaps.md` M24-6).
    //   line 6:  result = {10 / b}
    //   cols:    1234567890
    // `result = ` is 9 chars, `{` is col 10, so the `10` starts at col 11.
    let src = "print(\"line 1\")\nprint(\"line 2\")\nb := 0\nmsg := \"\"\"\nsome text\nresult = {10 / b}\n\"\"\"\nprint(msg)\n";
    assert_fault_at(src, "division by zero", (6, 11));
}

#[test]
fn interpolation_escape_newline_before_fragment_not_misattributed() {
    // A `\n` ESCAPE is NOT a source newline, so it must not shift the reported line — and now that
    // is proved rather than approximated: the map records where each content char physically is, so
    // the escape costs the fragment two columns (its two source chars) and zero lines.
    //   line 4:  msg := "a\nb {10 / b}"
    //   cols:    12345678901234
    // `"`=8, `a`=9, `\`=10, `n`=11, `b`=12, ` `=13, `{`=14, so the `10` starts at col 15.
    let src =
        "print(\"line 1\")\nprint(\"line 2\")\nb := 0\nmsg := \"a\\nb {10 / b}\"\nprint(msg)\n";
    assert_fault_at(src, "division by zero", (4, 15));
}

#[test]
fn interpolation_backstop_still_faults_for_generic_str() {
    // Static format-spec/value-type checking fires only for CONCRETE scalars; a generic body
    // `"{v:.2f}"` (v: T) passes check, so instantiating it with a str MUST still fault at RUNTIME
    // with the identical message on BOTH engines — proving the runtime backstop is intact.
    let src = "fn show[T](v: T) -> str:\n    return \"{v:.2f}\"\nfn main():\n    print(show(\"hi\"))\nmain()\n";
    let e1 = run_capture(src).unwrap_err().message;
    let e2 = run_capture_parallel(src).unwrap_err().message;
    assert!(
        e1.contains("type 'f' not valid for a string"),
        "serial: {e1}"
    );
    assert!(
        e2.contains("type 'f' not valid for a string"),
        "parallel: {e2}"
    );
    // A valid concrete-float `{x:.2f}` still renders identically on both engines.
    let ok = "x: float = 3.14159\nprint(\"{x:.2f}\")\n";
    assert_eq!(run_capture(ok).unwrap(), "3.14\n");
    assert_eq!(run_capture_parallel(ok).unwrap(), "3.14\n");
}

#[test]
fn non_interpolation_fault_span_unchanged() {
    // A direct top-level fault (no interpolation) still reports its real position — proves
    // `origin: None` leaves normal lexing byte-identical. `c := 10 / b`: `c`=1 … `1`=6.
    let src = "print(\"line 1\")\nprint(\"line 2\")\nb := 0\nc := 10 / b\nprint(c)\n";
    assert_fault_at(src, "division by zero", (4, 6));
    // A literal-only string + a valid interpolation still runs with correct output on both engines.
    let ok = "x := 5\nprint(\"x = {x}, done\")\n";
    assert_eq!(run_capture(ok).unwrap(), "x = 5, done\n");
    assert_eq!(run_capture_parallel(ok).unwrap(), "x = 5, done\n");
}

// ---- Bug B: print's `str(self)` display hook is used only when it CONFORMS to Stringable ----
// (returns a `str`) — checked on the RETURNED VALUE, so an annotated / inferred / aliased str all
// work, while a non-`str` return falls back to the default repr instead of recursing forever
// (was an uncatchable stack-overflow SIGABRT on a check-accepted program).

#[test]
fn str_hook_nonstr_return_struct_falls_back_to_default_repr() {
    // The Bug B repro: `str(self) -> S` returns the struct itself. Pre-fix this recursed in the
    // native stringifier → SIGABRT (the test binary would ABORT). Now it falls back to `S(n=5)`.
    let src = "struct S:\n    n: int\n    fn str(self) -> S:\n        return self\nfn main(): print(S(5))\nmain()\n";
    assert_mc_parity(src, "S(n=5)\n");
}

#[test]
fn str_hook_nonstr_return_enum_and_newtype_fall_back() {
    let enum_src = "enum E:\n    A(int)\n    fn str(self) -> E:\n        return self\nfn main(): print(E.A(5))\nmain()\n";
    assert_mc_parity(enum_src, "A(5)\n");
    let nt_src = "newtype N = int:\n    fn str(self) -> N:\n        return self\nfn main(): print(N(5))\nmain()\n";
    assert_mc_parity(nt_src, "N(5)\n");
}

#[test]
fn str_hook_used_when_it_returns_str_annotated_inferred_or_aliased() {
    // Annotated `-> str`.
    let annotated = "struct S:\n    n: int\n    fn str(self) -> str:\n        return \"A{self.n}\"\nfn main(): print(S(5))\nmain()\n";
    assert_mc_parity(annotated, "A5\n");
    // Inferred str (un-annotated) — a syntactic `-> str` gate would wrongly drop this; the
    // returned-value check keeps it working.
    let inferred = "struct S:\n    n: int\n    fn str(self):\n        return \"custom<{self.n}>\"\nfn main(): print(S(5))\nmain()\n";
    assert_mc_parity(inferred, "custom<5>\n");
    // A str type-alias return also conforms.
    let aliased = "type MyStr = str\nstruct S:\n    n: int\n    fn str(self) -> MyStr:\n        return \"hi{self.n}\"\nfn main(): print(S(5))\nmain()\n";
    assert_mc_parity(aliased, "hi5\n");
    // Same gate applies inside string interpolation.
    let interp = "struct S:\n    n: int\n    fn str(self) -> S:\n        return self\nfn main(): print(\"v={S(5)}\")\nmain()\n";
    assert_mc_parity(interp, "v=S(n=5)\n");
}

#[test]
fn str_hook_direct_call_returning_nonstr_still_works() {
    // `str` stays a normal user method: a direct `s.str()` returning the struct is unaffected by the
    // display-hook gate (no checker rejection, no runtime restriction).
    let src = "struct S:\n    n: int\n    fn str(self) -> S:\n        return self\nfn main():\n    x := S(5).str()\n    print(x.n)\nmain()\n";
    assert_mc_parity(src, "5\n");
}

#[test]
fn str_hook_nonstr_fallback_is_gc_safe_after_mutating_hook() {
    // A non-`str`-returning `str(self)` that MUTATES a non-interned heap field (a List) and
    // allocates enough to trigger a mark-sweep before returning `self`: the default-repr fallback
    // must re-read the LIVE rooted struct (not the pre-hook clone, whose old field was swept), or it
    // would dereference a dangling GcRef and panic uncatchably. Expect the CURRENT field state.
    let src = "struct S:\n    n: int\n    tag: List[int]\n    fn str(self) -> S:\n        self.tag = [9, 9, 9]\n        i := 0\n        acc := [0]\n        while i < 100000:\n            acc = [i]\n            i = i + 1\n        return self\nfn main(): print(S(5, [1, 2, 3]))\nmain()\n";
    assert_mc_parity(src, "S(n=5, tag=[9, 9, 9])\n");
}

// A bare same-module generic fn pinned through a builtin closure-result HOF slot (`.map`/`.fold`)
// must RUN correctly on both engines (runtime is generic-erased; the checker fix is what unblocks it).

#[test]
fn generic_fn_value_map_conv_runs_both_engines() {
    // `.map(conv)` — conv's own T pinned = int from the element type; yields the string list.
    let src = "fn conv[T](x: T) -> str:\n    return str(x)\nprint([1, 2, 3].map(conv))\n";
    assert_mc_parity(src, "['1', '2', '3']\n");
}

#[test]
fn generic_fn_value_map_ident_runs_both_engines() {
    // `.map(ident)` — return-only T pinned = int; yields the int list unchanged.
    let src = "fn ident[T](x: T) -> T:\n    return x\nprint([1, 2, 3].map(ident))\n";
    assert_mc_parity(src, "[1, 2, 3]\n");
}

#[test]
fn generic_fn_value_fold_add_runs_both_engines() {
    // `.fold(0, add)` — accumulator U pinned by init=int, then add's T pinned from fn(int,int)->int.
    let src = "fn add[T: Add](a: T, b: T) -> T:\n    return a + b\nprint([1, 2, 3].fold(0, add))\n";
    assert_mc_parity(src, "6\n");
}

#[test]
fn generic_fn_value_map_closure_still_runs_both_engines() {
    // Regression: the unannotated-closure loop-back is untouched — still [2, 4, 6].
    let src = "print([1, 2, 3].map(fn(x): x * 2))\n";
    assert_mc_parity(src, "[2, 4, 6]\n");
}

#[test]
fn generic_fn_value_filter_keep_runs_both_engines() {
    // Regression: a bare generic keep[T] into .filter still runs (concrete fn(int)->bool slot).
    let src = "fn keep[T](x: T) -> bool:\n    return true\nprint([1, 2, 3].filter(keep))\n";
    assert_mc_parity(src, "[1, 2, 3]\n");
}

#[test]
fn generic_fn_value_user_hof_and_turbofish_run_both_engines() {
    // Regression: the pre-existing user-HOF pin and the turbofish workaround still run unchanged.
    let src = "fn conv[T](x: T) -> str:\n    return str(x)\nfn mymap(xs: List[int], f: fn(int) -> str) -> List[str]:\n    return xs.map(f)\nprint(mymap([1, 2, 3], conv))\nprint([1, 2, 3].map(conv[int]))\n";
    assert_mc_parity(src, "['1', '2', '3']\n['1', '2', '3']\n");
}

// ── Parameterized protocols in value position (Q1) — two-engine goldens ──────────────────────

#[test]
fn param_protocol_value_arg_runs_both_engines() {
    // A `Container[int]` parameter accepts a conforming struct; `c.get(0)` dispatches by name and
    // yields the recovered `int` on both engines.
    let src = "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    items: List[int]\n    fn get(self, i: int) -> int:\n        return self.items[i]\nfn f(c: Container[int]) -> int:\n    return c.get(0) + 1\nfn main():\n    print(f(Bag([41])))\nmain()\n";
    assert_mc_parity(src, "42\n");
}

#[test]
fn param_protocol_method_return_recovered_runs_both_engines() {
    // DECISION-2 recovery: `x := c.get(0)` is typed `int` and used in int arithmetic; identical
    // stdout across the serial VM and the M:N engine.
    let src = "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    fn get(self, i: int) -> int:\n        return 7\nfn f(c: Container[int]) -> int:\n    x := c.get(0)\n    return x * 6\nfn main():\n    print(f(Bag()))\nmain()\n";
    assert_mc_parity(src, "42\n");
}

#[test]
fn param_protocol_field_and_nesting_run_both_engines() {
    // A `Container[int]` struct field and a `List[Container[int]]` param both witness + erase; the
    // stored values dispatch by name identically on both engines.
    let src = "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct Bag:\n    n: int\n    fn get(self, i: int) -> int:\n        return self.n\nstruct Holder:\n    c: Container[int]\nfn sum2(xs: List[Container[int]]) -> int:\n    return xs[0].get(0) + xs[1].get(0)\nfn main():\n    h := Holder(Bag(10))\n    print(h.c.get(0))\n    print(sum2([Bag(20), Bag(30)]))\nmain()\n";
    assert_mc_parity(src, "10\n50\n");
}

#[test]
fn param_protocol_reassign_accept_runs_both_engines() {
    // DECISION-3 reassignment write-site (accept path): reassigning a different conforming struct into
    // a `Container[int]` local witnesses at the reassign boundary, then dispatches by name identically.
    let src = "protocol Container[T]:\n    fn get(self, i: int) -> T\nstruct A:\n    fn get(self, i: int) -> int:\n        return 1\nstruct B:\n    fn get(self, i: int) -> int:\n        return 2\nfn f(c: Container[int]) -> int:\n    c = B()\n    return c.get(0)\nfn main():\n    print(f(A()))\nmain()\n";
    assert_mc_parity(src, "2\n");
}

#[test]
fn convert_witness_runs_two_engine() {
    // Convert/From slice 2 — a static-ctor witness of `[T: Convert[int]]` type-checks; the bound-check
    // runs at the call site then erases (the trivial body never calls `T.convert` — that's slice 3), so
    // the program runs byte-identically on the serial VM and the M:N engine.
    let src = "struct Port:\n    n: int\n    fn convert(x: int) -> Port:\n        return Port(n=x)\nfn use2[T: Convert[int]](x: int) -> int:\n    return x\nfn main():\n    print(use2[Port](5))\nmain()\n";
    assert_mc_parity(src, "5\n");
}

#[test]
fn legal_deep_nested_pattern_type_checks_and_runs() {
    // VERIFY (guards against over-rejection): a legally-nested `Some(...)` value and a matching deep
    // pattern must NOT be rejected by the new pattern-depth guard — it still type-checks, and the
    // checker's exhaustiveness + type walk and the VM matcher stay safe recursing to that depth.
    // Depth is capped by the VALUE expression itself: each `Some(` nesting costs ~2 of the shared
    // `parser::MAX_DEPTH` budget in expression position (parse_expr_bp + parse_unary), so a nested
    // ctor value maxes out near MAX_DEPTH/2 — the pattern side (1 depth/level) is the looser
    // constraint. THIS `n` IS CALIBRATED TO `parser::MAX_DEPTH` and must move with it: 30 at the
    // shipped cap of 64. (It was cut to 20 while `Span` was briefly 24 bytes and the cap 48; `Span`
    // is now 12 bytes, the cap is 64 again, so 30 is restored.) 30 is deep enough to prove the
    // pattern walk is safe and shallow enough to stay legal on both axes.
    let n = 30;
    let value = format!("{}0{}", "Some(".repeat(n), ")".repeat(n));
    let pattern = format!("{}x{}", "Some(".repeat(n), ")".repeat(n));
    let src = format!("o := {value}\nr := match o:\n    {pattern}: x\n    _: -1\nprint(r)\n");
    assert_eq!(run_capture(&src).unwrap().trim(), "0");
}

/// Crash-safety over the ITERATIVE-chain / composed-nesting depth axis: the deepest AST the parser
/// ACCEPTS (bounded by `MAX_DEPTH` paren nesting × `MAX_CHAIN_DEPTH` per chain ≈ 15 k here) must run
/// on the VM's dedicated stack without a host stack overflow. This is the regression guard for the
/// deep-`1+1+…` / `a.f.f…` SIGABRT: the parser cap keeps the depth bounded, and the large VM /
/// front-end stacks absorb the walk. Runs on the debug test-harness → the 384 MiB `VM_STACK_BYTES`
/// thread (via `run_capture`), the *smaller* of the two front-end stacks, so debug frames are the
/// worst case. A single flat chain just under the cap must also run and compute correctly.
#[test]
fn deep_accepted_chains_run_without_stack_overflow() {
    // ~12 k-deep AST: 25 nested parens (comfortably under the `parser::MAX_DEPTH` paren ceiling, which
    // each paren costs ~2 of), each adding a near-`MAX_CHAIN_DEPTH` `+0` chain onto the left spine.
    // Value is invariant (all `+0`) so the result is deterministic; the point is that
    // walking/compiling/running this depth does not abort. Assign then print separately so the
    // `print(...)` call wrapper doesn't eat into the paren budget. THE PAREN COUNT IS CALIBRATED TO
    // `parser::MAX_DEPTH` and must move with it: 25 at the shipped cap of 64. (It was cut to 18 while
    // `Span` was briefly 24 bytes and the cap 48; `Span` is now 12 bytes and the cap is 64 again.)
    let mut inner = String::from("1");
    for _ in 0..25 {
        inner = format!("({inner}{})", "+0".repeat(499));
    }
    assert_eq!(
        run_capture(&format!("x := {inner}\nprint(x)\n"))
            .unwrap()
            .trim(),
        "1"
    );

    // A single flat chain just under the cap runs and computes the right sum.
    let flat = format!("print(1{})\n", "+1".repeat(400));
    assert_eq!(run_capture(&flat).unwrap().trim(), "401");
}

// ===== Multi-line pipe chains (leading-`|>` line continuation) + `iter.sum` =====

/// A line starting with `|>` continues the previous logical line — same result as the one-liner,
/// on both engines. Also covers a chain nested inside a fn body.
#[test]
fn pipe_multiline_chain_both_engines() {
    let src = "\
fn dbl(x: int) -> int:
    return x * 2

fn inc(x: int) -> int:
    return x + 1

fn f(n: int) -> int:
    r := n
        |> dbl()
        # keep going
        |> inc()
    return r

r := 5
    |> dbl()
    |> inc()
print(r)
print(f(10))
";
    assert_mc_parity(src, "11\n21\n");
}

/// `iter.sum` delegates to the native List `sum` method — including its empty-list → `0`.
#[test]
fn iter_sum_delegates_and_empty_is_zero() {
    let src = "import std.iter\nprint(iter.sum([1, 2, 3]))\nprint(iter.sum([]))\n";
    let entry = write_temp_chz("iter_sum", src);
    let (out, _e, res, _c) = run_file(&entry);
    assert!(res.is_ok(), "serial iter.sum faulted: {res:?}");
    assert_eq!(out, "6\n0\n", "serial");
    let (pout, _pe, pres, _pc) = run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(pres.is_ok(), "M:N iter.sum faulted: {pres:?}");
    assert_eq!(pout, "6\n0\n", "M:N");
}

/// The verbatim docs/syntax.md §11 pipe example must run and print 60 on both engines.
#[test]
fn docs_syntax_pipe_example_runs_on_both_engines() {
    let src = "\
import std.iter

total := [1, 2, 3, 4]
    |> iter.filter(fn(x: int) -> bool: x % 2 == 0)   # → iter.filter([1,2,3,4], ...)
    |> iter.map(fn(x: int) -> int: x * 10)
    |> iter.sum()
print(total)                                         # 60
";
    let entry = write_temp_chz("docs_s11", src);
    let (out, _e, res, _c) = run_file(&entry);
    assert!(res.is_ok(), "serial docs §11 faulted: {res:?}");
    assert_eq!(out, "60\n", "serial");
    let (pout, _pe, pres, _pc) = run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_file(&entry);
    assert!(pres.is_ok(), "M:N docs §11 faulted: {pres:?}");
    assert_eq!(pout, "60\n", "M:N");
}

/// Regression net for the check-time `return`-in-`defer:`/`spawn:` rejection: every legal nesting
/// of `defer:`/`spawn:`/`parallel:` (plus a nested `fn` that DOES `return`, declared inside each block)
/// must keep running identically on both engines. If the checker guard over-rejects, this goes red.
#[test]
fn defer_spawn_nesting_matrix_parity() {
    let src = "\
fn f() -> int:
    defer:
        fn g() -> int:
            return 40
        print(\"nested-fn-in-defer {g()}\")
        defer:
            print(\"defer-in-defer\")
        parallel:
            spawn print(\"spawn-call-in-defer\")
        parallel:
            spawn:
                print(\"spawn-block-in-defer\")
    parallel:
        spawn:
            fn h() -> int:
                return 41
            print(\"nested-fn-in-spawn {h()}\")
            defer:
                print(\"defer-in-spawn\")
    return 1
print(f())
";
    assert_mc_parity(
        src,
        "nested-fn-in-spawn 41\ndefer-in-spawn\nnested-fn-in-defer 40\nspawn-call-in-defer\nspawn-block-in-defer\ndefer-in-defer\n1\n",
    );
}

// ---------------------------------------------------------------------------------------------
// R1 — the bytes native seam (`NativeRet::Bytes` / `Host::arg_bytes` / `NativeArg::Bytes`).
// ---------------------------------------------------------------------------------------------

/// Run a std-module-importing program on BOTH engines (a `import std.*` program needs the resolver,
/// so it runs from a file) and assert identical stdout.
///
/// TYPE-CHECKS FIRST — `run_file`/`run_file_p` are resolve → compile → run and skip `check_graph`
/// (the known test-helper-vs-CLI divergence), so without this gate a test could assert a program
/// green that `chezzi run` rejects outright. Every R1 program here is what the CLI would run.
fn assert_mc_parity_file(tag: &str, src: &str, expected: &str) {
    let entry = write_temp_chz(tag, src);
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    if let Err(errs) = crate::checker::check_graph(&graph) {
        panic!("program must type-check ({tag}), got: {errs:?}");
    }
    let (out, _e, res, _c) = run_file(&entry);
    assert!(res.is_ok(), "serial VM faulted: {res:?}");
    assert_eq!(out, expected, "serial VM");
    let (out_p, _e, res_p, _c) = run_file_p(&entry);
    assert!(res_p.is_ok(), "M:N engine faulted: {res_p:?}");
    assert_eq!(out_p, expected, "M:N engine");
    let _ = std::fs::remove_file(&entry);
}

/// R1 step 1 — a native fn RETURNING bytes lowers to `Obj::Bytes` (`NativeRet::Bytes`).
#[test]
fn native_ret_bytes_lowers_to_bytes() {
    let src = "\
import std.encoding

fn go() -> int!:
    b := encoding.base64_decode_bytes(\"AAD/\")?
    print(b.len())
    print(b[0])
    print(b[2])
    return Ok(0)

fn main():
    match go():
        Ok(_): print(\"ok\")
        Err(e): print(\"ERR:\" + e.message())
main()
";
    assert_mc_parity_file("r1_ret_bytes", src, "3\n0\n255\nok\n");
}

/// R1 step 2 — `Host::arg_bytes` carries a `bytes` arg across the seam. The seam is `bytes`-ONLY: a
/// `bytearray` buffer is converted with `bytes(ba)` (checker rule 7b29552, pinned by
/// `checker::tests::bytes_native_seam_takes_bytes_only_bytearray_needs_an_explicit_convert`), and that
/// converted buffer hashes to the same digest as the literal.
#[test]
fn host_arg_bytes_accepts_bytes_and_a_converted_bytearray() {
    let src = "\
import std.crypto
import std.encoding

fn main():
    print(crypto.sha256_bytes(b\"abc\"))
    print(crypto.sha256_bytes(bytes(bytearray(b\"abc\"))))
    print(encoding.base64_encode_bytes(b\"\\x00\\xff\"))
main()
";
    let d = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    assert_mc_parity_file("r1_arg_bytes", src, &format!("{d}\n{d}\nAP8=\n"));
}

/// R1 step 2 — a non-bytes arg to a bytes-taking native. In real code this is a CHECK-time error
/// (the param is typed `bytes`; the assignability rule is pinned by
/// `checker::tests::bytes_native_seam_takes_bytes_only_bytearray_needs_an_explicit_convert`), so the
/// seam's runtime arg-type fault is pinned DIRECTLY on `VmHost`
/// rather than through a .chz program that could never compile.
#[test]
fn host_arg_bytes_rejects_non_bytes() {
    use crate::native::Host;
    let entry = write_temp_chz(
        "r1_arg_bytes_bad",
        "import std.crypto\n\nfn main():\n    print(crypto.sha256_bytes(7))\nmain()\n",
    );
    let graph = crate::resolver::build_graph(&entry).expect("resolve");
    let errs =
        crate::checker::check_graph(&graph).expect_err("a non-bytes arg must not type-check");
    let _ = std::fs::remove_file(&entry);
    assert!(
        errs.iter()
            .any(|e| e.message.contains("expected bytes, found int")),
        "{errs:?}"
    );

    let mut vm = Vm::new(Arc::new(empty_program()));
    let mut host = VmHost {
        vm: &mut vm,
        args: vec![Value::int(7)],
    };
    let err = host.arg_bytes(0).expect_err("int is not bytes");
    assert_eq!(err.message, "argument 0 must be bytes, got int");
}

/// R1 step 3 — `Vm::extract_native_args` carries a `bytes` arg across the D5 off-heap
/// handoff (without this NO blocking native can take a bytes arg: extraction returns `None`, the
/// call silently runs INLINE and pins a core worker — invisible black-box, hence this direct test).
#[test]
fn extract_native_args_carries_bytes() {
    use crate::native::NativeArg as A;
    let mut vm = Vm::new(Arc::new(empty_program()));
    let b = Value::obj(vm.heap.alloc(Obj::Bytes(vec![0u8, 255].into_boxed_slice())));
    assert_eq!(
        vm.extract_native_args(&[b, Value::int(3)]),
        Some(vec![A::Bytes(vec![0, 255]), A::Int(3)])
    );
    // A `bytearray` is NOT a seam arg (the checker rejects it at a `bytes` param): extraction bails
    // to `None` → the call would run inline, never off-heap with a stale copy of a mutable buffer.
    let ba = Value::obj(vm.heap.alloc(Obj::ByteArray(vec![1u8, 2])));
    assert_eq!(vm.extract_native_args(&[ba]), None);
}

/// R1 step 3 — the off-heap host SERVES a pre-extracted bytes arg (leaving it on the trait default
/// would make a blocking bytes native fail ONLY under M:N).
#[test]
fn offload_host_serves_bytes_arg() {
    use crate::native::{Host, NativeArg as A};
    let mut h = OffloadHost {
        args: vec![A::Bytes(vec![7, 8]), A::Str("x".into())],
    };
    assert_eq!(h.arg_bytes(0).unwrap(), vec![7u8, 8]);
    assert!(h.arg_bytes(1).is_err());
}

/// R1 step 4 — a real BINARY file (NULs, 0xFF, invalid UTF-8) round-trips byte-exactly through
/// `io.write_bytes` → `io.read_bytes` (both BLOCKING natives — the end-to-end proof of the
/// `NativeArg::Bytes` offload path), and `io.read_file` on it errs with a hint at `read_bytes`.
#[test]
fn io_bytes_round_trips_a_binary_file() {
    let path = std::env::temp_dir().join(format!("chezzi_r1_bin_{}.dat", std::process::id()));
    let p = path.to_str().unwrap().to_string();
    let src = format!(
        "\
import std.io

fn go() -> int!:
    b := bytes([0, 255, 254, 128])
    io.write_bytes(\"{p}\", b)?
    back := io.read_bytes(\"{p}\")?
    print(back.len())
    for x in back:
        print(x)
    print(back == b)
    match io.read_file(\"{p}\"):
        Ok(s): print(\"decoded?! \" + s)
        Err(e): print(e.message().contains(\"read_bytes\"))
    return Ok(0)

fn main():
    match go():
        Ok(_): print(\"ok\")
        Err(e): print(\"ERR:\" + e.message())
main()
"
    );
    assert_mc_parity_file("r1_io_bin", &src, "4\n0\n255\n254\n128\ntrue\ntrue\nok\n");
    let _ = std::fs::remove_file(&path);
}

/// R1 — REGRESSION PIN: a `bytes` / `bytearray` value already crosses the task airlock
/// (`WireValue::Bytes`/`ByteArray`). R1 must not regress it.
#[test]
fn bytes_crosses_the_task_airlock() {
    let src = "\
fn worker(ch: Channel[bytes], b: bytes):
    ch.send(b)

fn main():
    ch := Channel[bytes](1)
    parallel:
        spawn worker(ch, b\"\\x00A\\xff\")
    got := ch.recv()
    print(got.len())
    print(got[0])
    print(got[2])
    ba := bytearray(b\"\\x01\")
    parallel:
        spawn print(ba.len())
main()
";
    assert_mc_parity(src, "3\n0\n255\n1\n");
}

/// **W6-3 structural ratchet — every intrinsic protocol grant must be CALLABLE at runtime.**
///
/// The checker grants built-ins protocol conformance INTRINSICALLY (no user method) at the
/// `grant_intrinsic` early-outs in `checker::proto::satisfies_args_d`, and the granted method must
/// therefore be callable from an erased generic body. That pairing was honored for 2 of ~11 grants and
/// broke the rest (`type int has no method 'add'` after `check: ok`).
///
/// The rows are keyed on `(protocol, method, receiver-KIND)` because the receiver kind is the axis
/// W6-3 actually failed on: `compare`/`str` *were* paired, but their interceptions were type-gated
/// narrower than the checker's grant set. This test generates one probe program PER ROW from a
/// receiver-literal table and a call template, and RUNS it on both engines. Widening a grant to one
/// more type therefore cannot pass: `grant_intrinsic`'s `debug_assert` demands the new row, and the
/// row demands a probe that actually runs.
///
/// The probes are deliberately NOT generic — `run_capture` compiles without the checker and the
/// compiler is type-blind, so `a := <literal>` + `a.<method>(…)` reaches the exact same
/// `Vm::do_method_call` dispatch an erased `[T: P]` body does, with no bound/type-arg noise. The
/// generic-body spelling, operator equivalence, and fault-text equality are covered by
/// `tests/chz/spec/intrinsic_proto_methods_test.chz`.
#[test]
fn intrinsic_grants_all_have_vm_arms() {
    use crate::checker::proto::{INTRINSIC_PROTO_METHODS, INTRINSIC_UNPAIRED};
    /// One receiver per kind `Checker::intrinsic_recv_kind` can classify a grant as — the value to
    /// probe with, plus the literals/types the parameterized bounds need. `"-"` = the kind has no
    /// grant needing that column. `struct` needs ONE struct satisfying all three struct-granted
    /// protocols at once: zero fields and no `hash` (intrinsic `Hashable`) plus `next(self) -> int?`
    /// (`Iterator`, and `Iterable` through it).
    struct Recv {
        kind: &'static str,
        prelude: &'static str,
        lit: &'static str,
        key: &'static str,
        val: &'static str,
        elem_ty: &'static str,
        key_ty: &'static str,
        val_ty: &'static str,
        slice_ty: &'static str,
    }
    let r = |kind, prelude, lit, key, val, elem_ty, key_ty, val_ty, slice_ty| Recv {
        kind,
        prelude,
        lit,
        key,
        val,
        elem_ty,
        key_ty,
        val_ty,
        slice_ty,
    };
    let recvs: &[Recv] = &[
        r("int", "", "7", "-", "-", "-", "-", "-", "-"),
        r("float", "", "1.5", "-", "-", "-", "-", "-", "-"),
        r("bool", "", "true", "-", "-", "-", "-", "-", "-"),
        r("str", "", "\"abc\"", "0", "-", "str", "int", "str", "str"),
        r(
            "bytes", "", "b\"abc\"", "0", "-", "int", "int", "int", "bytes",
        ),
        r(
            "bytearray",
            "",
            "bytearray(b\"abc\")",
            "0",
            "1",
            "int",
            "int",
            "int",
            "bytearray",
        ),
        r(
            "list",
            "",
            "[1, 2, 3]",
            "0",
            "9",
            "int",
            "int",
            "int",
            "List[int]",
        ),
        r("set", "", "Set([1, 2])", "-", "-", "int", "-", "-", "-"),
        r(
            "map",
            "",
            "{\"a\": 1}",
            "\"a\"",
            "9",
            "str",
            "str",
            "int",
            "-",
        ),
        r(
            "struct",
            "struct Cur:\n    fn next(self) -> int?:\n        return None\n",
            "Cur()",
            "-",
            "-",
            "int",
            "-",
            "-",
            "-",
        ),
        r(
            "newtype",
            "newtype NT = int\n",
            "NT(7)",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
        ),
        // D1 — the kinds the widened `Eq` grant added. `enum`/`option`/`result` are ONE runtime
        // shape (`Obj::Enum`), so their probes are what pin the `Obj::Enum` dispatch fallback.
        r("tuple", "", "(1, 2)", "-", "-", "-", "-", "-", "-"),
        r(
            "enum",
            "enum En:\n    A\n    B\n",
            "En.A",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
        ),
        r("option", "", "Some(1)", "-", "-", "-", "-", "-", "-"),
        r("result", "", "Ok(1)", "-", "-", "-", "-", "-", "-"),
        // W7-54 — a function value's `Eq` grant. Only the `eq` template applies; it uses no other
        // column.
        r(
            "func",
            "fn g(x: int) -> int:\n    return x\n",
            "g",
            "-",
            "-",
            "-",
            "-",
            "-",
            "-",
        ),
        // SECOND row for the SAME kind, and it is not redundant: `Ty::BuiltinFn` (`ord`/`chr`/
        // `panic`/`print`) is a DISTINCT `Ty` variant that shares `"func"`'s `intrinsic_recv_kind`
        // (the W7-54 follow-up). The matrix sweep below loops `for rv in recvs`, so this row is what
        // actually feeds a `Ty::BuiltinFn` to every protocol's `bound_probe` — without it the sweep
        // would prove nothing about the variant, and a future protocol arm matching `BuiltinFn` but
        // not `Func` would get no cell at all. `accepted` is deduped, so the shared ("Eq", "func")
        // cell still matches `registered` exactly once; `recv_of` returns the FIRST row, so the
        // runtime probe below stays the `Ty::Func` one.
        r("func", "", "ord", "-", "-", "-", "-", "-", "-"),
    ];
    // (method, call template) — `{r}` is the receiver, `{k}`/`{v}` the index key/value.
    let calls: &[(&str, &str)] = &[
        ("compare", "{r}.compare(b)"),
        ("eq", "{r}.eq(b)"),
        ("str", "{r}.str()"),
        ("hash", "{r}.hash()"),
        ("message", "{r}.message()"),
        ("as_path", "{r}.as_path()"),
        ("iter", "{r}.iter()"),
        ("next", "{r}.next()"),
        ("index", "{r}.index({k})"),
        ("set_index", "{r}.set_index({k}, {v})"),
        ("slice", "{r}.slice(Some(0), Some(1), None)"),
        ("add", "{r}.add(b)"),
        ("sub", "{r}.sub(b)"),
        ("mul", "{r}.mul(b)"),
        ("div", "{r}.div(b)"),
        ("mod", "{r}.mod(b)"),
        ("neg", "{r}.neg()"),
    ];
    // Build the probe for a row, or `None` if the tables can't express it (which the assertions
    // below turn into a failure — a row must never be silently skipped).
    let recv_of = |kind: &str| recvs.iter().find(|r| r.kind == kind);
    let probe = |method: &str, kind: &str| -> Option<String> {
        let rv = recv_of(kind)?;
        let (_, tmpl) = *calls.iter().find(|(m, _)| *m == method)?;
        // An unfilled ("-") column the template needs = a missing table entry, not a skip.
        if (tmpl.contains("{k}") && rv.key == "-") || (tmpl.contains("{v}") && rv.val == "-") {
            return None;
        }
        let call = tmpl
            .replace("{r}", "a")
            .replace("{k}", rv.key)
            .replace("{v}", rv.val);
        // BIND the result: a bare `Option`/`Result`-valued expression statement auto-propagates
        // (`unhandled error: None`), which would mask the dispatch this probe is about.
        Some(format!(
            "{}a := {}\nb := {}\nres := {call}\n",
            rv.prelude, rv.lit, rv.lit
        ))
    };
    // The CHECKER half: bind the receiver to a `[T: Protocol]` param (no method call needed — the
    // bound alone forces the conformance decision). Type-checking this per row (a) proves the grant
    // for that (protocol, receiver-kind) really EXISTS, so a stale row fails, and (b) is what actually
    // executes `Checker::grant_intrinsic`, whose `debug_assert` fires when a grant has no row — i.e.
    // this loop is what makes widening a grant to one more TYPE a test failure.
    let bound_probe = |protocol: &str, kind: &str| -> String {
        let rv = recv_of(kind).expect("every kind has a receiver");
        // A parameterized protocol's bound must state its type args (arity is checked), so spell them
        // from the receiver's own element/key/value/slice types. A `"-"` column means the kind has no
        // grant for that protocol, so the arg is irrelevant to the (rejecting) answer — `int` stands in
        // rather than skipping the cell, because a SKIPPED cell is a hole in the matrix.
        let t = |c: &'static str| if c == "-" { "int" } else { c };
        let bound = match protocol {
            "Iterable" | "Iterator" => format!("{protocol}[{}]", t(rv.elem_ty)),
            "Index" | "IndexSet" => format!("{protocol}[{}, {}]", t(rv.key_ty), t(rv.val_ty)),
            "Slice" => format!("Slice[{}]", t(rv.slice_ty)),
            _ => protocol.to_string(),
        };
        format!(
            "{}fn probe[T: {bound}](a: T):\n    pass\nprobe({})\n",
            rv.prelude, rv.lit
        )
    };
    let type_errors = |src: &str| -> Vec<String> {
        let tokens = crate::lexer::tokenize(src).expect("probe should lex");
        let module = crate::parser::parse(tokens).expect("probe should parse");
        crate::checker::check(&module)
            .err()
            .unwrap_or_default()
            .iter()
            .map(|e| e.message.clone())
            .collect()
    };
    // **The (protocol × receiver-kind) MATRIX — the half that catches a widened grant.** An assert
    // inside `grant_intrinsic` can only fire on inputs a test actually feeds it, so sweep the FULL
    // cross product of every registered protocol against every receiver kind, and require the set of
    // cells the checker ACCEPTS to equal the set of registered rows. Adding `Ty::Bytes` to the
    // `Comparable` grant (or `Ty::Float` to `Hashable`, or `Ty::Set` to `index_kv`) flips one cell to
    // accepted and fails HERE — the per-type hole a protocol-keyed table cannot see.
    let protocols: Vec<&str> = {
        let mut ps: Vec<&str> = INTRINSIC_PROTO_METHODS
            .iter()
            .chain(INTRINSIC_UNPAIRED)
            .map(|(p, _, _)| *p)
            .collect();
        ps.sort_unstable();
        ps.dedup();
        ps
    };
    let mut accepted: Vec<(&str, &str)> = Vec::new();
    for p in &protocols {
        for rv in recvs {
            let src = bound_probe(p, rv.kind);
            let errs = type_errors(&src);
            if errs.is_empty() {
                accepted.push((p, rv.kind));
            } else {
                // A registered row the checker does NOT grant is a stale row (or a broken probe).
                assert!(
                    !INTRINSIC_PROTO_METHODS
                        .iter()
                        .chain(INTRINSIC_UNPAIRED)
                        .any(|(tp, _, tk)| tp == p && *tk == rv.kind),
                    "registered row ({p}, {}) claims an INTRINSIC grant the checker does not give: \
                     {errs:?}\n--- probe ---\n{src}",
                    rv.kind
                );
            }
        }
    }
    let mut registered: Vec<(&str, &str)> = INTRINSIC_PROTO_METHODS
        .iter()
        .chain(INTRINSIC_UNPAIRED)
        .map(|(p, _, k)| (*p, *k))
        .collect();
    registered.sort_unstable();
    registered.dedup();
    accepted.sort_unstable();
    accepted.dedup();
    assert_eq!(
        accepted, registered,
        "the (protocol, receiver-kind) cells the checker grants no longer match \
         INTRINSIC_PROTO_METHODS ∪ INTRINSIC_UNPAIRED — a grant was widened or narrowed. Add/remove \
         the row AND make the method callable at runtime (W6-3)"
    );
    for (p, m, kind) in INTRINSIC_PROTO_METHODS {
        let src = probe(m, kind).unwrap_or_else(|| {
            panic!(
                "no probe for intrinsic grant ({p}, {m}, {kind}) — add its receiver literal / call \
                 template to intrinsic_grants_all_have_vm_arms (W6-3)"
            )
        });
        for (engine, run) in [
            (
                "serial",
                run_capture as fn(&str) -> Result<String, RuntimeError>,
            ),
            ("M:N", run_capture_parallel),
        ] {
            if let Err(e) = run(&src) {
                panic!(
                    "intrinsic grant ({p}, {m}, {kind}) is NOT callable on the {engine} engine: \
                     {e}\n--- probe ---\n{src}"
                );
            }
        }
    }
    // A registered carve-out must STAY a fault (see `INTRINSIC_UNPAIRED`) — if a later change makes it
    // work, the row must be retired with it. `INTRINSIC_UNPAIRED` is EMPTY since W6-3b (the raw-
    // collection `Iterator` grant was narrowed away), so this loop and the disjointness loop below are
    // currently no-ops; they stay so the ratchet re-arms the moment a new unpairable grant is added.
    for (p, m, kind) in INTRINSIC_UNPAIRED {
        let src = probe(m, kind)
            .unwrap_or_else(|| panic!("no probe for unpaired grant ({p}, {m}, {kind})"));
        let Err(err) = run_capture(&src) else {
            panic!("({p}, {m}, {kind}) is the documented carve-out — it must still fault");
        };
        assert!(
            err.to_string().contains(&format!("has no method '{m}'")),
            "unexpected ({p}, {m}, {kind}) fault: {err}"
        );
    }
    // No row may appear in both tables (a paired row that also claims to be a carve-out).
    for row in INTRINSIC_UNPAIRED {
        assert!(
            !INTRINSIC_PROTO_METHODS.contains(row),
            "{row:?} is registered as BOTH paired and unpaired"
        );
    }
}

/// W6-2 — compile + run `src` on the serial engine and report how many module-global SNAPSHOTS it built
/// (`snapshot_builds`, bumped on every `ensure_snapshot` cache MISS). Reaches a private counter, so it
/// lives here rather than in a Chezzi test; a timing bench can only hint at what this counts exactly.
fn snapshot_builds_for(src: &str) -> usize {
    let tokens = crate::lexer::tokenize(src).expect("lex");
    let module = crate::parser::parse(tokens).expect("parse");
    let program = crate::compiler::compile_module_standalone(&module).expect("compile");
    let mut vm = Vm::new(Arc::new(program));
    vm.run().expect("run");
    vm.snapshot_builds
}

/// W6-2 — the snapshot cache must actually SHORT-CIRCUIT, not merely be correct: a fix that re-snapshots
/// per `spawn` passes every correctness test and quietly costs O(all module globals) per spawn (measured
/// 84x on a spawn storm with a big aggregate global). The two invalidation rules give exactly:
///
/// * all-immutable globals (`reusable`) → ONE build for the whole run, however many nurseries;
/// * a mutable aggregate global → one build per NURSERY (rule 2 drops the cache at `EnterNursery`,
///   because `q.push(..)` writes no module slot for rule 1 to see) — and, crucially, still only ONE
///   build for N spawns into the SAME nursery;
/// * a global ASSIGNMENT between spawns (rule 1) → one build per assignment-then-spawn.
#[test]
fn snapshot_cache_short_circuits_per_epoch_not_per_spawn() {
    let three_nurseries = "\nfor i in range(3):\n    parallel:\n        spawn: pass\n";
    assert_eq!(
        snapshot_builds_for(&format!("n: int = 1\ns := \"x\"{three_nurseries}")),
        1,
        "all-immutable globals: 3 nurseries must share ONE build"
    );
    assert_eq!(
        snapshot_builds_for(&format!("n: int = 1\nq: List[int] = [1]{three_nurseries}")),
        3,
        "an aggregate global: one build per nursery (in-place mutation writes no slot)"
    );
    assert_eq!(
        snapshot_builds_for(
            "q: List[int] = [1]\nparallel:\n    for i in range(50):\n        spawn: pass\n"
        ),
        1,
        "50 spawns into ONE nursery must share ONE build, aggregate global or not"
    );
    assert_eq!(
        snapshot_builds_for("g: int = 0\nparallel:\n    spawn: pass\n    g = 1\n    spawn: pass\n"),
        2,
        "a global assignment between two spawns must refresh the view (one build each)"
    );
}

/// W6-2 — a snapshot BUILD failure is CARRIED on the queued task and raised where the task is PREPARED
/// (its nursery's join), never at the `spawn`: that is what keeps a nursery whose tasks are all cancelled
/// (`break` out of `parallel:`) faultless, and what keeps the `parallel:` body's own output ahead of the
/// fault. White-box because the only program-level trigger is a module global deeper than
/// `MAX_STRUCTURAL_DEPTH`, and `to_snap`'s slow arm is O(n^2) on such a chain (~5100 links) — minutes in a
/// debug build, on `main` as much as here, so it cannot live in the test suite.
#[test]
fn a_carried_snapshot_build_error_is_raised_at_task_preparation() {
    let program = crate::compiler::compile_module_standalone(
        &crate::parser::parse(crate::lexer::tokenize("x := 1\n").expect("lex")).expect("parse"),
    )
    .expect("compile");
    let mut vm = Vm::new(Arc::new(program));
    let span = Span {
        line: 7,
        col: 3,
        file: 0,
    };
    let carried = vm.err("simulated snapshot build failure".to_string(), span);
    // Open a nursery by hand (`Op::EnterNursery`'s lockstep stacks) and queue a task whose pin FAILED.
    vm.nurseries.push(Vec::new());
    vm.mn_scopes.push(None);
    vm.nursery_defer_floors.push(0);
    vm.eager_scheds.push(None);
    vm.nurseries[0].push(QueuedTask {
        call: PendingCall::Call {
            callee: Value::nil(),
            args: Vec::new(),
            span,
        },
        snap: Err(carried.clone()),
        cell_ids: Vec::new(),
    });
    let raised = vm
        .join_nursery()
        .expect_err("the carried build error must surface at the join, which prepares the task");
    assert_eq!(raised.message, carried.message);
    assert_eq!(raised.span, span, "the error keeps its spawn-site span");
}

/// W7-21 — a module GLOBAL holding a fn VALUE, CALLED through the module (`l.BARE()`). The checker
/// used to reject this (it read only `ModuleSig::functions`) while the runtime always supported it:
/// `Op::CallMethod` on an `Obj::Module` looks the member up in the slot table and calls whatever
/// value it finds. This test PASSES PRE-FIX (`run_file` does not run the checker) and is not the
/// fence — the fence is `checker::tests::module_global_of_fn_type_is_callable_qualified`. What it
/// locks is the other half of the claim: that the bytecode/VM path really executes the form the
/// checker now accepts, identically on both engines. Both ancestors print 1: CPython `m.G()` where
/// `G = _one`, Go `pkg.G()` where `var G = one`.
#[test]
fn module_global_fn_value_call_runs_both_engines() {
    let dir = std::env::temp_dir().join(format!("chezzi_vm_w721_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("k.chz"), "fn one() -> int:\n    return 1\n").unwrap();
    std::fs::write(dir.join("l.chz"), "import k\nBARE := k.one\n").unwrap();
    let entry = dir.join("main.chz");
    std::fs::write(
        &entry,
        "import l\nprint(l.BARE())\nz := l.BARE\nprint(z())\n",
    )
    .unwrap();
    let (vm_out, _e, vm_res, _) = run_file(&entry);
    let (par_out, _pe, par_res, _) =
        run_file_parallel(&entry, crate::native::HostConfig::default());
    let _ = std::fs::remove_dir_all(&dir);
    assert!(vm_res.is_ok(), "serial faulted: {vm_res:?}");
    assert!(par_res.is_ok(), "M:N faulted: {par_res:?}");
    assert_eq!(vm_out, "1\n1\n");
    assert_eq!(
        vm_out, par_out,
        "serial and M:N diverged on a module fn-value call"
    );
}
