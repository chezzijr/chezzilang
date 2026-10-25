//! VM tests: isolated worker VMs, home snapshot, method tasks.
//! Split out of `src/vm/tests.rs`. `super::*` is the parent module.

use super::*;

// ----- B3.2: isolated worker-VM construction (no threads) -----

/// Build a one-proto program + a parent `Vm`, plus a zero-arg closure over proto 0 with a dummy
/// home module (the test protos never read globals). Mirrors how `do_spawn_block` shapes a task.
fn worker_fixture(code: Vec<Op>) -> (Vm, PendingCall) {
    let sp = Span::RUNTIME;
    let proto = op::Proto {
        name: "task".into(),
        arity: 0,
        min_arity: 0,
        n_slots: 0,
        lines: vec![sp; code.len()],
        code,
        has_implicit_nursery: false,
        is_generator: false,
        is_test: false,
        decl_span: Span::RUNTIME,
        capture_names: Vec::new(),
        private_slots: 0,
        deep_slots: 0,
    };
    let program = Program {
        protos: vec![proto],
        ..empty_program()
    };
    let mut vm = Vm::new(Arc::new(program));
    let home = vm.heap.alloc(Obj::Module(Box::new(ModuleData::new(
        "<test>".into(),
        RVec::new(),
        Default::default(),
    ))));
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

/// M1.7 — the worker allocates into the run's one heap: a task that builds a fresh list runs to
/// completion in the worker, its objects are slots of the parent's core (garbage until the next
/// collection), and the result still crosses back as a `WireValue` (copy semantics) that
/// reconstructs to the expected value.
#[test]
fn worker_runs_on_the_runs_one_heap() {
    // () -> [1, 2]
    let (mut vm, task) = worker_fixture(vec![
        Op::ConstInt(1),
        Op::ConstInt(2),
        Op::NewList(2),
        Op::Return,
    ]);
    let before = vm.heap.live();
    let res = vm.run_task_isolated(task).expect("isolated task runs");
    assert!(
        vm.heap.live() > before,
        "the worker's objects are in the run's one heap"
    );
    let got = vm.from_wire(res.value);
    let want = Value::obj(
        vm.heap
            .alloc(Obj::List(vec![Value::int(1), Value::int(2)].into())),
    );
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

/// A worker's stdout is captured in ITS `out` and returned on the `IsolatedRun` (decision F:
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
    let m = vm.entry_home();
    let i = vm
        .slot_index(m, name)
        .unwrap_or_else(|| panic!("no global '{name}'"));
    vm.read_slot(m, i, sp())
        .unwrap_or_else(|e| panic!("global '{name}': {}", e.message))
}

fn sp() -> Span {
    Span::RUNTIME
}

/// A spawned task reads a module-level constant — needs the read-only `home` snapshot (B3.3c):
/// the worker's `home` is a reconstruction of the parent module's globals, not a fresh-empty one.
#[test]
fn worker_reads_module_global() {
    let mut vm = ran_program("answer := 42\nfn get_answer() -> int:\n    return answer\n");
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
    let mut vm = ran_program("a := 1\nb := 2\nc := 99\nfn get_c() -> int:\n    return c\n");
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
    let mut vm = ran_program(
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

/// TICKET-235 — a task queued past `Vm::task_entry` with a callee that pushes no frame is refused
/// at `start_task`, not run frameless. Goes red when the refusal is removed: the builtin then runs
/// with `self.frames` empty.
#[test]
fn start_task_refuses_a_callee_that_enters_no_frame() {
    let mut vm = Vm::new(Arc::new(empty_program()));
    let callee = Value::obj(
        vm.heap
            .alloc(Obj::Builtin(crate::base::builtin::Builtin::Print)),
    );
    let r = vm.start_task(PendingCall::Call {
        callee,
        args: Vec::new(),
        span: sp(),
    });
    let e = r.expect_err("a builtin callee enters no frame");
    assert!(
        e.message.contains("a task must start in a Chezzi frame"),
        "{}",
        e.message
    );
}

/// A struct method resolved through reconstructed `module_objs` — and its body **reads a module
/// global** (`scale`), so dispatch must resolve through the rebuilt home *contents*, not merely
/// index an in-bounds placeholder. `(3 + 4) * 10 == 70`. TICKET-235: a method task is a `Call`
/// task whose callee is the site's entry thunk, so this runs the program end to end.
#[test]
fn worker_method_on_struct() {
    assert_eq!(
        run_capture("scale := 10\nstruct Point:\n    x: int\n    y: int\n    fn weighted(self, out: Channel[int]):\n        out.send((self.x + self.y) * scale)\np := Point(3, 4)\nch := Channel[int](1)\nparallel:\n    spawn p.weighted(ch)\nprint(ch.recv())\n").unwrap(),
        "70\n"
    );
}

/// Cross-heap safety: a module global that is a **container of callables** (`[fn …]`) must have its
/// nested `Func` *rebuilt* in the worker heap, not carried across as a by-reference `Handle` (a
/// parent-heap `GcRef`). A task that calls through the list exercises the reconstructed funcs; a
/// smuggled `GcRef` would read a wrong/out-of-range worker slot. `bump(20) == 21`.
#[test]
fn worker_calls_through_global_fn_container() {
    let mut vm = ran_program(
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
    let mut vm = ran_program(
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
