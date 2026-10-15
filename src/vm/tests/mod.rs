//! VM unit tests, one file per topic; shared helpers and fixtures live here. `super::*` is the
//! `vm` module.
use super::*;

/// Assert an UNCAUGHT fault yields the expected stdout and that it actually faulted.
/// `run_capture` drops stdout on `Err`, so go through the
/// `(stdout, result)` harness directly.
#[cfg(test)]
fn assert_fault_parity(src: &str, expected_out: &str) {
    let (vm_out, vm_res) = run_program(src);
    assert!(vm_res.is_err(), "VM expected to fault, got {vm_out:?}");
    assert_eq!(vm_out, expected_out, "VM stdout");
}

/// Assert a program yields `expected`.
#[cfg(test)]
fn assert_mc_parity(src: &str, expected: &str) {
    assert_eq!(run(src), expected);
}

/// A rendezvous (cap-0) channel core (TICKET-028).
fn cap0_core() -> Arc<ChannelCore> {
    Arc::new(ChannelCore {
        cap: Some(0),
        ..Default::default()
    })
}

fn core_key(core: &Arc<ChannelCore>) -> usize {
    Arc::as_ptr(core) as usize
}

fn dl_err() -> RuntimeError {
    RuntimeError {
        message: DEADLOCK_MSG.to_string(),
        span: Span::RUNTIME,
        is_assert: false,
        is_over_memory: false,
        is_timed_out: false,
        is_deadlock: false,
        is_panic: false,
    }
}

fn empty_core() -> Arc<ChannelCore> {
    Arc::new(ChannelCore::default())
}

pub(crate) fn empty_program() -> Program {
    Program {
        protos: vec![],
        structs: Default::default(),
        range_key: "Range".to_string(),
        enum_methods: Default::default(),
        enum_home: Default::default(),
        providers: Default::default(),
        native_methods: Default::default(),
        native_home: Default::default(),
        fresh_calls: Vec::new(),
        variants: Default::default(),
        variants_by_id: Vec::new(),
        struct_names: Vec::new(),
        struct_widths: Vec::new(),
        any_eq_hook: false,
        hooks_struct: Vec::new(),
        hooks_enum: Vec::new(),
        modules: vec![],
        field_ic_sites: 0,
        method_ic_sites: 0,
        task_entry: 0,
        cffi_defs: vec![],
        tests: vec![],
        suites: vec![],
        type_names: Default::default(),
    }
}

fn mk_fiber(task_index: usize) -> Fiber {
    Fiber {
        ctx: FiberCtx::default(),
        state: FiberState::Ready,
        task_index,
        scope_id: 0,
        span: Span::RUNTIME,
        resume_native: None,
        pending: None,
        park_site: None,
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
        pending: None,
        park_site: None,
    }
}

fn mk_sched(total: usize) -> MnSched {
    // 4 worker slots by default — enough for the multi-`wid` steal tests; single-worker tests
    // just use `wid` 0.
    // `mem_cap` 0 = the `--max-heap` cap off, which is what every fixture here wants.
    MnSched::new(total, 4, Arc::new(AtomicBool::new(false)), dl_err(), 0)
}

/// Run `src` from a temp entry file (a real graph resolve, so `import … from
/// std.concurrency.pmap` pulls the embedded module), for the caller to assert against a literal
/// expected. Asserts the run itself was clean.
#[cfg(test)]
fn pmap_both(tag: &str, src: &str) -> String {
    let entry = write_temp_chz(tag, src);
    let (out, _e, res, _c) = run_file(&entry);
    let _ = std::fs::remove_file(&entry);
    assert!(res.is_ok(), "faulted: {res:?}");
    out
}

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

/// TICKET-181 — a demoted fiber's waiter on an empty `recv` of `core`, watching `cancel`.
fn recv_waiter(core: &Arc<ChannelCore>, cancel: Vec<Arc<AtomicBool>>) -> Waiter {
    Waiter {
        wait: Arc::new(quiesce::PartyWait::Recv(Arc::clone(core), None)),
        cancel,
        fiber: true,
    }
}

/// Run a program to completion, returning its stdout (panics on runtime error).
fn run(src: &str) -> String {
    run_capture(src).unwrap_or_else(|e| panic!("unexpected runtime error: {e}"))
}

/// Run a program expected to fail; return the runtime error message.
fn run_err(src: &str) -> String {
    match run_capture(src) {
        Ok(out) => panic!("expected a runtime error, got output: {out:?}"),
        Err(e) => e.message,
    }
}

/// Run a program and return its stdout, for the caller to assert against a literal expected.
fn run_parity(src: &str) -> String {
    run_capture(src).expect("vm run")
}

fn take_run(s: &MnSched) -> Fiber {
    // tick=1 → not a periodic-global-check schedule, so the normal own-local-then-global order
    // applies (what the existing unit tests assert).
    match s.take_runnable(0, 1, 0) {
        Take::Run(f, _) => f,
        Take::Stop => panic!("expected a runnable fiber, got Stop"),
    }
}

/// D5 test helper: write a Chezzi source to a uniquely-named temp `.chz` file and return its path
/// (so `run_file_with` resolves `import std.*` through the real module graph, unlike
/// `compile_module_standalone`). The caller removes it after the run.
fn write_temp_chz(tag: &str, src: &str) -> std::path::PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("chezzi_{tag}_{}_{seq}.chz", std::process::id()));
    std::fs::write(&path, src).expect("write temp .chz");
    path
}

/// Every production source file of the VM: the `.rs` files directly in `src/vm/` except the test
/// modules (each in-file `#[cfg(test)] mod NAME { .. }` block is removed), as `(file name, text)` in name order. A source-text rule reads these, so a code move
/// between VM files cannot carry a pattern out of the rule's sight.
fn vm_sources() -> Vec<(String, String)> {
    let dir = crate::native::std_embed::checkout_root().join("src/vm");
    let mut out: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .map(|p| {
            let name = p.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {name}: {e}"));
            // Production code only: each inline `#[cfg(test)] mod NAME { .. }` block is removed.
            (name, strip_inline_test_mods(&text))
        })
        .filter(|(name, _)| !name.ends_with("tests.rs"))
        .collect();
    out.sort();
    assert!(
        out.iter().any(|(n, _)| n == "mod.rs"),
        "src/vm/mod.rs not found"
    );
    out
}

/// `text` without its inline test modules: each `#[cfg(test)]` + `mod NAME {` block, through the
/// first line that starts with `}` at column 0 (its closing brace). A `mod NAME;` declaration stays.
fn strip_inline_test_mods(text: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        if skipping {
            if line.starts_with('}') {
                skipping = false;
            }
            continue;
        }
        let opens_test_mod = line == "#[cfg(test)]"
            && lines
                .peek()
                .is_some_and(|next| next.starts_with("mod ") && next.trim_end().ends_with('{'));
        if opens_test_mod {
            lines.next();
            skipping = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

mod autodrain;
mod channels_wire;
mod defaults_witness;
mod executor_cut;
mod executor_deadlock;
mod extern_mnsched;
mod goldens_display;
mod host_nursery;
mod lang_basics;
mod panic_wait;
mod parity;
mod perf_guards;
mod pipes_snapshots;
mod sched_unit;
mod sched_wake;
mod spawn_cancel;
mod worker_vm;
