// vm::exec — split out of vm/mod.rs. `super::*` == the `vm` module.
// VM interpreter core: construction, frames, generators, run/run_until/step dispatch.

use super::*;

impl Vm {
    /// The stdout sink. DEFAULT (`host.stream == false`) = append to the captured `out` buffer:
    /// byte-identical to what every test helper, embedder and the serial-vs-M:N parity oracle has
    /// always seen. STREAM (`chezzi run` only) = hand the whole `print` to the stdout writer thread
    /// ([`stream`]), which turns it into ONE `write_all` on the real handle: output appears when it
    /// happens, a `print` is line-atomic across tasks, and the fiber NEVER blocks in `write(2)` (a
    /// stalled reader must not pin a core worker — the D5 invariant).
    ///
    /// If stdout has DIED (a closed reader, a full disk), the writer thread has marked it and dropped
    /// the bytes. Writing is then a NO-OP: `emit_out` never touches control flow. The halt is raised
    /// separately, at the print SITE, by [`Vm::stream_halt`].
    ///
    /// It MUST NOT signal the death through `pending_exit`: that is the `std.os.exit` channel, and it
    /// OUTRANKS a fault everywhere (`run_file_with_entry` returns `Ok(())` + the code and DISCARDS the
    /// `Err`; `classify_mn_outcome` ranks `Exit` above `Fault` at the join). Routing a dead pipe
    /// through it turned a genuinely FAULTING run into a silent `exit 0`: `chezzi run x.chz | head -1`
    /// on a program that then indexes out of bounds reported SUCCESS with no trace — a red CI going
    /// green. A dead stdout is not an exit request, so it does not borrow the exit channel.
    pub(super) fn emit_out(&mut self, s: &str) {
        if self.host.stream {
            stream::write_out(s);
        } else {
            self.out.push_str(s);
        }
    }

    /// The fault to raise at a print site once the streamed stdout is dead — an ORDINARY recoverable
    /// `RuntimeError`, which is what composes correctly with everything the exit channel broke: it
    /// unwinds through defers, can be caught by `recover:`, loses to nothing at a cross-task join, and
    /// exits NON-ZERO with a trace on stderr (still live — `| head` closes only stdout). Python raises
    /// `BrokenPipeError` here for the same reason. Without a halt at all, `chezzi run x.chz | head -1`
    /// would spin forever on a dead pipe: Rust ignores SIGPIPE, and restoring it would break
    /// `std.net`'s EPIPE-as-an-error contract.
    ///
    /// ponytail: a defer that prints while a REAL fault is unwinding raises this fault too, so its
    /// message replaces the original's (the run still exits non-zero, with a trace — only the message
    /// names the pipe instead of the first cause). Threading an `in_unwind` flag through the unwind
    /// path would preserve it; not worth it until someone is actually confused by it.
    pub(super) fn stream_halt(&self, span: Span) -> Option<RuntimeError> {
        if !self.host.stream {
            return None;
        }
        stream::out_dead_reason().map(|why| RuntimeError { message: why, span })
    }

    /// The stderr sink — same contract as [`Vm::emit_out`], on a SEPARATE writer + lock (so a task's
    /// `print` and `eprint` can reorder relative to each other, exactly like Python's).
    pub(super) fn emit_err(&mut self, s: &str) {
        if self.host.stream {
            stream::write_err(s);
        } else {
            self.stderr.push_str(s);
        }
    }

    pub(super) fn new(program: Arc<Program>) -> Self {
        let field_ic = vec![IcCell::EMPTY; program.field_ic_sites as usize];
        let method_ic = vec![MethodIcSite::EMPTY; program.method_ic_sites as usize];
        // M19 Tier-2 quickening: prefix-sum the per-proto code lengths into `quicken_base`, and size
        // `quicken` to the program's total instruction count (one Q_COLD cell per site). Cheap — one
        // pass at startup, which has ~11× headroom vs CPython.
        let mut quicken_base = Vec::with_capacity(program.protos.len());
        let mut acc: u32 = 0;
        for p in &program.protos {
            quicken_base.push(acc);
            acc += p.code.len() as u32;
        }
        let quicken = vec![Q_COLD; acc as usize];
        // Option B — cache whether the program contains ANY generator body, so the module-global
        // generator reachability gate short-circuits to zero cost for generator-free programs.
        let has_generators = program.protos.iter().any(|p| p.is_generator);
        Vm {
            program,
            heap: Heap::new(),
            stack: Vec::new(),
            frames: Vec::new(),
            out: String::new(),
            stderr: String::new(),
            host: crate::native::HostConfig::default(),
            call_depth: 0,
            module_objs: Vec::new(),
            str_intern: fxhash::FxHashMap::default(),
            field_ic,
            method_ic,
            quicken,
            quicken_base,
            cur_base: 0,
            handlers: Vec::new(),
            pending_exit: None,
            fault_trace: None,
            fault_trace_depth: 0,
            gc_stress: false,
            has_generators,
            parallel: false,
            nurseries: Vec::new(),
            mn_scopes: Vec::new(),
            mn_enlisted: 0,
            mn_enlist_sched: None,
            eager_scheds: Vec::new(),
            nursery_defer_floors: Vec::new(),
            executors: Vec::new(),
            suspend: None,
            wait_suspend: None,
            offload: None,
            poll_park: None,
            pending_connect: None,
            poll_timed_out: false,
            native_reentry: 0,
            reds: 0,             // D3 — set to CONTEXT_REDS per schedule-in (run_one_fiber)
            yield_now: false,    // D3
            gen_yielding: false, // experimental generators
            gen_host_ctx: Vec::new(),
            active_generators: Vec::new(),
            wid: 0,         // D5 owe #3 (Path C) — set in mn_worker_loop
            demoted: false, // D5 owe #3 (Path C)
            scheduler_stack: Vec::new(),
            cancel: None,
            cancelled: false,
            module_snapshot: None,
            module_faulted: Vec::new(),
            snapshot_memo: None,
            mn: None,
        }
    }

    pub(super) fn err(&self, message: String, span: Span) -> RuntimeError {
        RuntimeError { message, span }
    }

    /// B3.4 — set this VM's nursery cancel flag (if it runs under one), so sibling workers abort.
    /// No-op on the cooperative engine / top-level VM (`cancel` is `None`).
    pub(super) fn trip_cancel(&self) {
        if let Some(c) = &self.cancel {
            c.store(true, Ordering::Relaxed);
        }
    }

    /// Swap the live per-execution `Vm` fields with `ctx` (B1). Used by the nursery scheduler to
    /// schedule a fiber in (its saved context becomes live) or out (the running context is parked
    /// back into the fiber). Exactly the fields [`FiberCtx`] holds — `pending_exit` stays global.
    pub(super) fn swap_ctx(&mut self, ctx: &mut FiberCtx) {
        std::mem::swap(&mut self.frames, &mut ctx.frames);
        std::mem::swap(&mut self.stack, &mut ctx.stack);
        std::mem::swap(&mut self.call_depth, &mut ctx.call_depth);
        std::mem::swap(&mut self.cur_base, &mut ctx.cur_base);
        std::mem::swap(&mut self.handlers, &mut ctx.handlers);
        std::mem::swap(&mut self.nurseries, &mut ctx.nurseries);
        std::mem::swap(&mut self.mn_scopes, &mut ctx.mn_scopes);
        std::mem::swap(
            &mut self.nursery_defer_floors,
            &mut ctx.nursery_defer_floors,
        );
        std::mem::swap(&mut self.eager_scheds, &mut ctx.eager_scheds);
        std::mem::swap(&mut self.fault_trace, &mut ctx.fault_trace);
        std::mem::swap(&mut self.fault_trace_depth, &mut ctx.fault_trace_depth);
        // stdin is NOT swapped: it is ONE source every task shares (Go/Python), so a cooperative
        // fiber reads the same `Vm::host.stdin` the entry task does — see `Stdin` and `spawn_worker`,
        // which hands the M:N worker the same shared handle.
        //
        // D2a — an M:N fiber (`Some`) owns its heap; swap it with the host's. A cooperative fiber
        // (`None`) shares the single `Vm::heap` (decision A), so its heap is left untouched and the
        // cooperative engine stays byte-identical by construction. D2b — the same `Some` gate carries
        // the fiber's heap-keyed side state (out/stderr/module roots/executors), so they move
        // atomically WITH the heap their `GcRef`s index. A cooperative fiber swaps none of it.
        if let Some(ctx_heap) = ctx.heap.as_mut() {
            debug_assert!(
                self.parallel,
                "cooperative fiber must never carry its own heap (decision A)"
            );
            std::mem::swap(&mut self.heap, ctx_heap);
            std::mem::swap(&mut self.out, &mut ctx.out);
            std::mem::swap(&mut self.stderr, &mut ctx.stderr);
            std::mem::swap(&mut self.module_objs, &mut ctx.module_objs);
            std::mem::swap(&mut self.module_faulted, &mut ctx.module_faulted);
            std::mem::swap(&mut self.executors, &mut ctx.executors);
            // M19 Phase 3 — the intern cache's `GcRef`s index this fiber's OWN heap, so it MUST travel
            // atomically with the heap (same heap-keyed argument as `module_objs`). A cooperative fiber
            // (`heap: None`) never reaches here and keeps aliasing the shell's cache.
            std::mem::swap(&mut self.str_intern, &mut ctx.str_intern);
            // D6b — a mid-flight `connect` parked on writability swaps WITH its fiber (it owns the
            // connecting fd that the netpoller is watching; it must not be left on the shell where the
            // next fiber would inherit or drop it).
            std::mem::swap(&mut self.pending_connect, &mut ctx.pending_connect);
            // D6c — a socket timeout marker set by the poll thread (on the detached fiber's ctx) swaps
            // in here so the resumed socket op sees it at entry. M:N-only, like `pending_connect`.
            std::mem::swap(&mut self.poll_timed_out, &mut ctx.poll_timed_out);
        }
    }

    /// B1 / D3 — the running fiber paused mid-flight and its frames stay live to replay on resume:
    /// either a blocking `recv` parked it (`suspend`) or it exhausted its D3 reduction budget
    /// (`yield_now`). Both unwind every nested `run_until` / call site the SAME way — propagate up
    /// WITHOUT popping a result or pushing a sentinel — so every "callee paused" gate tests this, not
    /// `suspend` alone. (`yield_now` is only ever set under the M:N engine — the safepoint gates it on
    /// `mn.is_some()` — so the cooperative engine, where it is always false, is unchanged by
    /// construction.)
    pub(super) fn paused(&self) -> bool {
        self.suspend.is_some()
            || self.wait_suspend.is_some()
            || self.yield_now
            || self.offload.is_some()
            || self.poll_park.is_some()
    }

    /// Run `f` with the native-reentry guard raised (B1). A blocking `recv` reached while the guard
    /// is up cannot park (its caller's loop/recursion state lives on the Rust stack, not in a
    /// [`Fiber`]), so it faults `deadlock` instead of suspending. Wraps every site that re-enters
    /// Chezzi code from native Rust.
    pub(super) fn guarded<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        self.native_reentry += 1;
        // The guard counter MUST return to its entry value on every exit path, including an unwind:
        // it gates park-vs-demote for all blocking concurrency ops, and a re-entered FFI callback's
        // Rust panic is caught one frame up (`callback_trampoline`'s `catch_unwind`) and re-raised as
        // a recoverable error — so a plain `-= 1` after `f(self)` would be skipped on panic and leak
        // the counter at +1 for the VM's lifetime. A `Drop`-based guard can't be used here (it would
        // alias `self` across `f(self)`), so catch the unwind, decrement, then resume it unchanged.
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(self)));
        self.native_reentry -= 1;
        match r {
            Ok(v) => v,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    // ----- experimental generators (VM-only) -----

    /// Swap the live execution context (frames/stack/depth/base/handlers) with a parked [`GenCtx`].
    /// Smaller sibling of [`Vm::swap_ctx`]: a generator shares the host heap (like a cooperative
    /// fiber, decision A) and cannot open nurseries/spawn (checker-forbidden), so none of the
    /// heap-keyed or nursery state moves.
    pub(super) fn swap_gen_ctx(&mut self, ctx: &mut GenCtx) {
        std::mem::swap(&mut self.frames, &mut ctx.frames);
        std::mem::swap(&mut self.stack, &mut ctx.stack);
        std::mem::swap(&mut self.call_depth, &mut ctx.call_depth);
        std::mem::swap(&mut self.cur_base, &mut ctx.cur_base);
        std::mem::swap(&mut self.handlers, &mut ctx.handlers);
    }

    /// Allocate a not-yet-started generator object over a generator proto + its call args. Calling a
    /// `yield`-ing function lands here instead of running the body (see `do_call`/`invoke_value`).
    pub(super) fn alloc_generator(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        args: Vec<Value>,
    ) -> Value {
        let core = GeneratorCore {
            proto,
            home,
            closure,
            state: GenState::Pending(args),
            ctx: GenCtx::default(),
        };
        Value::Obj(self.heap.alloc(Obj::Generator(Box::new(core))))
    }

    /// Resume a generator until its next `yield` (→ `Some(v)`) or until its body ends (→ `None`,
    /// state `Done`). Driven intrinsically by `.next()` (see `do_method_call`). The generator runs in
    /// its own private base-0 context swapped into the live `Vm`; the host context is parked in
    /// `gen_host_ctx` (GC-rooted) for the duration. Runs `guarded`, so a would-be blocking op inside a
    /// generator faults `deadlock` rather than parking the host.
    pub(super) fn generator_next(&mut self, h: GcRef, span: Span) -> Result<Value, RuntimeError> {
        // A RE-ENTRANT resume — `.next()` (or a `for`) on a generator that is already running, from
        // inside its own body — is a fault, not an answer. This MUST come before the state take
        // below: while a generator runs, its heap `state` is parked as the `Done` placeholder, so a
        // re-entrant call would otherwise hit the `Done` short-circuit and silently report a live
        // generator as EXHAUSTED (`None`). `active_generators` is the resume path's own root list,
        // pushed/popped around the run, so it is self-clearing on every unwind path (yield,
        // exhaustion, an early consumer `break`, a fault in the body, a fault caught by `recover:`)
        // — a generator can never get poisoned as permanently "running". Python: `ValueError:
        // generator already executing`.
        if self.active_generators.contains(&h) {
            return Err(self.err("generator already running".to_string(), span));
        }
        // Take the generator's lifecycle state + parked context out of the heap object. `state` is
        // left as `Done` and `ctx` as empty; the real state is written back after the run. An
        // already-`Done` generator short-circuits to `None`.
        let (proto, home, closure, mut gen_ctx, state) = {
            let Obj::Generator(g) = self.heap.get_mut(h) else {
                return Err(self.err("`.next()` on a non-generator value".to_string(), span));
            };
            if matches!(g.state, GenState::Done) {
                return Ok(self.alloc_enum("Option", "None", Vec::new()));
            }
            let state = std::mem::replace(&mut g.state, GenState::Done);
            let ctx = std::mem::take(&mut g.ctx);
            (g.proto, g.home, g.closure, ctx, state)
        };

        // Park the host context (rooted via `gen_host_ctx`) and install the generator's private
        // context into the live `Vm`. Two swaps: host out to a temp, then the generator in.
        let mut host = GenCtx::default();
        self.swap_gen_ctx(&mut host); // self.* now default-empty; `host` holds the real host context
        self.swap_gen_ctx(&mut gen_ctx); // self.* now the generator's (suspended) context; gen_ctx empty
        self.gen_host_ctx.push(host);
        self.active_generators.push(h);

        // First `.next()` builds the initial frame over the pending args (private stack starts empty,
        // so the frame lands at base 0). A resumed generator's frames are already in `self`.
        let first_call = matches!(state, GenState::Pending(_));
        let push_res = if let GenState::Pending(args) = state {
            self.push_frame(proto, home, closure, args, true, false, span)
        } else {
            Ok(())
        };

        // Run to the next suspension / end (guarded: no parking inside a generator).
        self.gen_yielding = false;
        let run = push_res.and_then(|()| self.guarded(|s| s.run_until(0)));
        let yielded = self.gen_yielding;
        self.gen_yielding = false;

        // Pull the generator's now-updated context back out and restore the host.
        let mut new_ctx = GenCtx::default();
        self.swap_gen_ctx(&mut new_ctx); // new_ctx = generator's current context; self.* now empty
        let mut host = self.gen_host_ctx.pop().expect("generator host context");
        self.swap_gen_ctx(&mut host); // self.* = host restored
        self.active_generators.pop();
        let _ = first_call;

        // A fault inside the generator: leave it `Done` (already set) with an empty ctx, propagate.
        run?;

        if yielded {
            // The yielded value sits on the generator's private stack top. Park the rest to resume.
            let v = new_ctx
                .stack
                .pop()
                .expect("yielded value on the generator stack");
            if let Obj::Generator(g) = self.heap.get_mut(h) {
                g.ctx = new_ctx;
                g.state = GenState::Suspended;
            }
            Ok(self.alloc_enum("Option", "Some", vec![v]))
        } else {
            // Body returned / fell off → exhausted. Drop the (drained) context.
            if let Obj::Generator(g) = self.heap.get_mut(h) {
                g.ctx = GenCtx::default();
                g.state = GenState::Done;
            }
            Ok(self.alloc_enum("Option", "None", Vec::new()))
        }
    }

    /// If `v` is an unhandled error (`Err(..)`/`None`) reaching the top level, build the runtime
    /// error that exits the program. Mirrors `interp::top_level_error` — message must be identical.
    pub(super) fn top_level_error(&self, v: Value, span: Span) -> Option<RuntimeError> {
        let Value::Obj(h) = v else { return None };
        let Obj::Enum {
            variant_id,
            payload,
        } = self.heap.get(h)
        else {
            return None;
        };
        // Builtin `Result`/`Option` only — a user enum that shadows `Err`/`None` gets a DISTINCT id
        // (natives hold the fixed `VID_ERR`/`VID_NONE_VARIANT`), so the int compare is exactly the
        // "is this the builtin unhandled-error variant" gate (more precise than the old name compare).
        let unhandled =
            *variant_id == crate::vm::op::VID_ERR || *variant_id == crate::vm::op::VID_NONE_VARIANT;
        if !unhandled {
            return None;
        }
        let detail = match payload.first() {
            Some(p) => self.display(*p),
            None => self.display(v),
        };
        Some(self.err(format!("unhandled error: {detail}"), span))
    }

    // ----- top-level drivers -----

    /// Run every module in dependency order, then the entry's `main()`.
    pub(super) fn run(&mut self) -> Result<(), RuntimeError> {
        for idx in 0..self.program.modules.len() {
            self.run_module(idx)?;
        }
        Ok(())
    }

    /// The entry module's runtime object (its globals/home), valid after `run()` has initialized the
    /// modules. The entry is the last module in dependency order. The `chezzi test` runner uses it as
    /// the home for free `test fn`s and suite construction thunks.
    pub(super) fn entry_home(&self) -> GcRef {
        *self
            .module_objs
            .last()
            .expect("modules initialized before invoking tests")
    }

    /// `chezzi test` — invoke one zero-arg test proto (a free `test fn` or a suite construction
    /// thunk) on this already-initialized VM, returning its result. The VM stays reusable after a
    /// fault, so the runner keeps going after a failing test. `Err` carries the fault's `span` (the
    /// `assert`'s line) for `file:line` reporting.
    pub fn invoke_test(&mut self, proto: ProtoId) -> Result<(), RuntimeError> {
        debug_assert!(
            self.program.protos[proto].is_test,
            "invoke_test called on a non-test proto"
        );
        let home = self.entry_home();
        self.run_proto(
            proto,
            home,
            None,
            Vec::new(),
            true,
            false,
            Span { line: 1, col: 1 },
        )?;
        Ok(())
    }

    /// Bare `chezzi run` with a `module:function` manifest entrypoint — invoke a named top-level
    /// function of the entry module after `run()` has initialized all modules. Looks the name up in
    /// the entry module's namespace (so a re-exported import works too) and calls it with no args.
    /// A missing name (or a non-callable binding) is a clear runtime error rather than a silent no-op.
    pub fn invoke_entrypoint(&mut self, fn_name: &str) -> Result<(), RuntimeError> {
        let span = Span { line: 1, col: 1 };
        let home = self.entry_home();
        // Read the binding by name from the entry module's slot table (mirrors `module_define`).
        let callee = match self.heap.get(home) {
            Obj::Module { slots, index, .. } => index.get(fn_name).map(|&i| slots[i as usize]),
            _ => None,
        };
        let callee = callee.ok_or_else(|| {
            self.err(
                format!(
                    "entrypoint function `{fn_name}` not found in module `{}`",
                    self.module_name(home)
                ),
                span,
            )
        })?;
        // Guard with a clear message before `invoke_value`'s generic "not callable" fault.
        let callable = matches!(
            callee,
            Value::Obj(h) if matches!(
                self.heap.get(h),
                Obj::Func { .. } | Obj::Closure { .. } | Obj::Native { .. } | Obj::Cffi(_)
            )
        );
        if !callable {
            return Err(self.err(
                format!(
                    "entrypoint `{fn_name}` in module `{}` is not a function (it is a {})",
                    self.module_name(home),
                    self.type_name(callee)
                ),
                span,
            ));
        }
        self.invoke_value(callee, Vec::new(), span)?;
        Ok(())
    }

    /// `chezzi test` — invoke a suite method/lifecycle hook proto with `self` bound to `recv` (a
    /// suite instance). Returns the method's value (ignored by the runner) or its fault.
    pub fn invoke_suite_method(
        &mut self,
        proto: ProtoId,
        recv: Value,
    ) -> Result<Value, RuntimeError> {
        let home = self.entry_home();
        self.run_proto(
            proto,
            home,
            None,
            vec![recv],
            true,
            false,
            Span { line: 1, col: 1 },
        )
    }

    /// `chezzi test` — construct a suite instance via its synthetic zero-arg `__new_<Suite>` thunk.
    pub fn build_suite_instance(&mut self, new_thunk: ProtoId) -> Result<Value, RuntimeError> {
        let home = self.entry_home();
        self.run_proto(
            new_thunk,
            home,
            None,
            Vec::new(),
            true,
            false,
            Span { line: 1, col: 1 },
        )
    }

    /// `chezzi test` — initialize all modules (run top-levels once) so globals/functions exist before
    /// tests are invoked. A thin public wrapper over `run` for the runner.
    pub fn init_for_tests(&mut self) -> Result<(), RuntimeError> {
        self.run()
    }

    /// `chezzi test` — construct a fresh VM over a compiled program (the runner owns the lifecycle).
    pub fn for_program(program: Arc<Program>) -> Self {
        Vm::new(program)
    }

    /// `chezzi test` — take + clear whatever a test printed to stdout, resetting the buffer so the
    /// next test starts clean (the runner currently discards it; the report is Rust-formatted).
    pub fn take_out(&mut self) -> String {
        std::mem::take(&mut self.out)
    }

    /// `chezzi test` — drain anything the program left running (e.g. an Executor a test forgot to
    /// shut down), mirroring the ordinary run's graceful reap. Best-effort: ignore drain faults so a
    /// stray resource doesn't mask the test verdict.
    pub fn reap_after_tests(&mut self) {
        let _ = self.drain_live_executors(Span { line: 1, col: 1 });
    }

    pub(super) fn run_module(&mut self, idx: usize) -> Result<(), RuntimeError> {
        let m = self.program.modules[idx].clone();
        // M19 Phase 2b: pre-size the namespace to the compiler's slot count and build its name→slot
        // index from `global_slots`, so `DefineGlobalSlot(i)` / bind-import writes land in the slot
        // the compiler chose. Native modules carry no slots (members injected by name below).
        let index: std::collections::HashMap<Box<str>, u32> = m
            .global_slots
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str().into(), i as u32))
            .collect();
        let mod_obj = self.heap.alloc(Obj::Module {
            name: m.label.clone().into_boxed_str(),
            slots: vec![Value::Nil; m.global_slots.len()],
            index,
        });
        debug_assert_eq!(self.module_objs.len(), idx);
        self.module_objs.push(mod_obj);

        // A native std module: populate its globals with Rust `NativeFn`s + float constants and
        // skip running a toplevel. Mirrors the interpreter's `eval_module` native arm.
        if let Some(name) = m.native {
            for (mname, func) in crate::native::native_members(name) {
                let nat = self.heap.alloc(Obj::Native {
                    name: (*mname).into(),
                    func: *func,
                });
                self.module_define(mod_obj, mname, Value::Obj(nat));
            }
            for (cname, cval) in crate::native::native_consts(name) {
                self.module_define(mod_obj, cname, Value::Float(*cval));
            }
            return Ok(());
        }

        // Bind imports (dependencies already ran, so their namespaces are populated).
        for imp in &m.imports {
            self.bind_import(mod_obj, imp)?;
        }

        // Run the module body once. No module auto-runs `main` — it's an ordinary function the
        // program calls itself (scripting-language model). An unhandled `Err`/`None` reaching the
        // top level (via `PopExprStmt` or a top-level `?`) exits during this call.
        self.run_proto(
            m.toplevel,
            mod_obj,
            None,
            Vec::new(),
            false,
            true,
            Span { line: 1, col: 1 },
        )?;
        Ok(())
    }

    pub(super) fn bind_import(
        &mut self,
        into: GcRef,
        imp: &crate::resolver::ResolvedImport,
    ) -> Result<(), RuntimeError> {
        use crate::ast::Import;
        let target_idx = self
            .program
            .module_index(&imp.target)
            .expect("resolver guarantees the import target is in the graph");
        let target_obj = self.module_objs[target_idx];
        match &imp.import {
            Import::Module { path, alias, .. } => {
                let name = alias
                    .clone()
                    .unwrap_or_else(|| path.last().cloned().unwrap_or_default());
                self.module_define(into, &name, Value::Obj(target_obj));
            }
            Import::From { names, .. } => {
                for (member, alias) in names {
                    // `std.ffi`'s exported FFI marshalling TYPE names — the fixed-width integers
                    // (`import int32 from std.ffi`) and the opaque `ptr` handle — carry NO runtime
                    // value: they are compile-time type imports the checker resolves. Skip them here
                    // (the module has no such global by design); any other missing member is a genuine
                    // error.
                    if self.module_name(target_obj) == "std.ffi"
                        && (crate::native::ffi::TYPE_NAMES.contains(&member.as_str())
                            || member == "ptr")
                    {
                        continue;
                    }
                    // `std.concurrency`'s four exported ctor/TYPE names (`Shared`/`RwShared`/`Atomic`/
                    // `Executor`) likewise carry NO runtime value: they are checker-resolved type
                    // imports, and the ctor is resolved by the compiler's name→opcode dispatch (not a
                    // bound module member). Skip them here — the file-less native module has no such
                    // global by design. (Without this, `import Shared from std.concurrency` faults.)
                    if self.module_name(target_obj) == "std.concurrency"
                        && matches!(
                            member.as_str(),
                            "Shared" | "RwShared" | "Atomic" | "Executor"
                        )
                    {
                        continue;
                    }
                    // `std.time`'s `timer` is an opcode-backed builtin with NO runtime module-member
                    // value (the call lowers via the compiler's name→opcode dispatch). Skip it — std.time
                    // is a REAL native module, so this MUST be `timer`-specific, not a blanket std.time
                    // skip (now/monotonic/sleep_ms/format DO bind normally). Without this, `import timer
                    // from std.time` faults `module 'std.time' has no member 'timer'`.
                    if self.module_name(target_obj) == "std.time" && member == "timer" {
                        continue;
                    }
                    // `std.net`'s `Socket`/`Listener` are TYPE-only imports with NO runtime module-member
                    // value: a `Socket` value comes from `connect`/`listen` and the type resolves directly
                    // to `Ty::Socket`. Skip them — the native module has no such global by design. Mirrors
                    // the interp `bind_import` skip (parity); without it `import Socket from std.net` faults.
                    if self.module_name(target_obj) == "std.net"
                        && matches!(member.as_str(), "Socket" | "Listener")
                    {
                        continue;
                    }
                    // Bind the member's runtime value if the target module exports one (a fn/value).
                    // A `from`-imported USER type (struct/enum/alias) carries NO runtime value — it
                    // resolves through the program-global type tables by name — so a member with no
                    // global that IS a known type name is skipped (not an error). A member that is
                    // neither a value nor a type is a genuine "no member". The value bind is tried
                    // FIRST so a fn named like a type IN ANOTHER MODULE is still bound here.
                    match self.module_global(target_obj, member) {
                        Some(value) => {
                            self.module_define(into, alias.as_ref().unwrap_or(member), value);
                        }
                        None if self.program.type_names.contains(member) => {}
                        None => {
                            let tname = self.module_name(target_obj);
                            return Err(self.err(
                                format!("module '{tname}' has no member '{member}'"),
                                imp.span,
                            ));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Push a frame for `proto` and run the dispatch loop until it returns; yield its result.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_proto(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        args: Vec<Value>,
        counted: bool,
        is_toplevel: bool,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let base_level = self.frames.len();
        self.push_frame(proto, home, closure, args, counted, is_toplevel, span)?;
        self.run_until(base_level)?;
        // B1/D3: the call paused mid-flight — a blocking `recv` parked it, or it exhausted its D3
        // reduction budget and is yielding its worker. The frames stay live (they replay on resume);
        // propagate the signal up without popping a result — the caller gates on `paused()` before
        // using the (sentinel) return value.
        if self.paused() {
            return Ok(Value::Nil);
        }
        Ok(self.stack.pop().unwrap_or(Value::Nil))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn push_frame(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        args: Vec<Value>,
        counted: bool,
        is_toplevel: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.frame_depth_guard(counted, span)?;
        let base = self.stack.len();
        for a in args {
            self.stack.push(a);
        }
        self.finish_frame(proto, home, closure, base, counted, is_toplevel, span);
        Ok(())
    }

    /// P1 — push a frame whose `argc` parameters are *already* contiguous on the operand stack at
    /// `[base..base + argc]` (the bytecode `Op::Call` fast path leaves them there to avoid the
    /// per-call `Vec<Value>` round-trip). Identical to [`Vm::push_frame`] minus the arg copy; never a
    /// top-level frame. The depth guard runs after the args are positioned — on overflow the args
    /// stay on the stack, but a `recover:` handler truncates to its saved `stack_len` and an uncaught
    /// overflow aborts, so the leftover slots are unobservable (same end state as the `Vec` path).
    pub(super) fn push_frame_in_place(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        base: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.frame_depth_guard(true, span)?;
        self.finish_frame(proto, home, closure, base, true, false, span);
        Ok(())
    }

    /// Bump + bound-check the call-depth counter (the infinite-recursion guard). Shared by the
    /// `Vec` and in-place frame-entry paths so both raise the identical overflow error.
    pub(super) fn frame_depth_guard(
        &mut self,
        counted: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        if counted {
            self.call_depth += 1;
            if self.call_depth > MAX_CALL_DEPTH {
                self.call_depth -= 1;
                return Err(self.err(
                    format!("maximum call depth ({MAX_CALL_DEPTH}) exceeded (infinite recursion?)"),
                    span,
                ));
            }
        }
        Ok(())
    }

    /// Reserve the non-parameter local slots above `[base..]` and push the `CallFrame`. Assumes the
    /// `argc` parameters are already on the stack starting at `base`. Shared frame-install tail.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn finish_frame(
        &mut self,
        proto: ProtoId,
        home: GcRef,
        closure: Option<GcRef>,
        base: usize,
        counted: bool,
        is_toplevel: bool,
        span: Span,
    ) {
        let n_slots = self.program.protos[proto].n_slots;
        // Reserve the remaining (non-parameter) local slots.
        while self.stack.len() < base + n_slots {
            self.stack.push(Value::Nil);
        }
        self.frames.push(CallFrame {
            proto,
            ip: 0,
            base,
            home,
            closure,
            counted,
            is_toplevel,
            deferred: Vec::new(),
            defer_markers: Vec::new(),
            nursery_len: self.nurseries.len(),
            has_implicit_nursery: self.program.protos[proto].has_implicit_nursery,
            call_span: span,
        });
        self.cur_base = base;
    }

    /// Build a stack trace from the live frames (innermost first), skipping module-toplevel frames.
    /// Valid only while the frames are intact — i.e. on the uncaught-error path, before unwinding.
    pub(super) fn capture_trace(&self) -> Vec<TraceFrame> {
        self.frames
            .iter()
            .rev()
            .filter(|f| !f.is_toplevel)
            .map(|f| TraceFrame {
                function: self.program.protos[f.proto].name.clone(),
                span: f.call_span,
            })
            .collect()
    }

    // ----- the dispatch loop -----

    pub(super) fn run_until(&mut self, base_level: usize) -> Result<(), RuntimeError> {
        // M19 — hoist the per-entry `Arc::clone(&self.program)`: borrow the program by raw
        // pointer instead of bumping the refcount. `self.program` is an immutable
        // `Arc<Program>` set once in `Vm::new` and NEVER reassigned (cooperative `spawn` /
        // `--parallel` workers each build their own `Vm`; `swap_ctx` swaps heap/frames/stack,
        // not `program`), so the pointee outlives this loop and the borrow is disjoint from
        // the `&mut self` fields `step` mutates (`step` only reads program data). Post-flatten
        // this entry is hit per top-level run + per native re-entry (HOF callbacks, operator
        // overloads, deferred calls) + per fiber resume — so the saved atomic shows on
        // callback-heavy code (see `benches/chz/hof.chz`).
        let program: *const Program = Arc::as_ptr(&self.program);
        while self.frames.len() > base_level {
            // Collect at instruction boundaries only: here every live value is reachable from the
            // VM roots (operand stack, frame slots, frame homes/closures, module namespaces) —
            // there are no mid-opcode temporaries off the stack to miss.
            if self.gc_stress || self.heap.should_collect() {
                self.collect();
            }
            // B3.4: a `--parallel` worker observes its nursery's cancel flag at this back-edge (the
            // same boundary `gc_stress` is checked at). A sibling having faulted / `os.exit`d set it;
            // unwind the whole worker so this still-running task aborts promptly instead of burning
            // cycles to completion. Cancellation behaves like an uncaught fault that bypasses
            // `recover:`: `unwind_deferred(base_level)` runs every frame's `defer`s (Go semantics)
            // AND drops their handlers, so a `recover:` inside the task cannot catch the cancel and
            // resume it (a cancelled task must die). `!self.cancelled` latches on the first
            // observation: while the cancel unwind runs the task's `defer`s back through this loop,
            // re-firing would skip them — so we stop observing once cancellation is in flight.
            if !self.cancelled
                && self
                    .cancel
                    .as_ref()
                    .is_some_and(|c| c.load(Ordering::Relaxed))
            {
                self.cancelled = true;
                let span = self.frames[self.frames.len() - 1].call_span;
                let rte = self.err("cancelled".to_string(), span);
                // B3.4 cancel: no cancel-report — a cancelled task's pending nurseries are torn down
                // silently (the parent that set the flag already escaped and reported its own).
                let rte = self.unwind_deferred(base_level, false).unwrap_or(rte);
                return Err(rte);
            }
            // D3: reduction-counting preemption (M:N engine only — the cooperative engine is the
            // frozen parity oracle and never preempts). Decrement the budget per dispatched op; at
            // exhaustion yield this worker so a queued sibling runs (round-robin fairness). Placed
            // AFTER the cancel check so cancel wins (a cancelled fiber unwinds, never yields). The
            // `native_reentry == 0` guard mirrors `recv`-park: a yield inside a native callback can't
            // save the caller's Rust-stack state, so we defer it (leave `reds` at 0 and re-check next
            // op, once the reentry unwinds). Reuses the suspend/rewind contract — frames stay intact,
            // resume re-enters `run_until(0)` — but carries no channel handle (a voluntary park).
            if self.mn.is_some() {
                if self.reds == 0 {
                    if self.native_reentry == 0 {
                        self.yield_now = true;
                        return Ok(());
                    }
                } else {
                    self.reds -= 1;
                }
            }
            // Experimental generators — an `Op::Yield` (handled last iteration) asked us to suspend:
            // hand control back to the host `.next()` with the generator's frames/stack intact. The
            // yielded value sits on the stack top for `generator_next` to take. Only ever true inside
            // the private `run_until` that `generator_next` drives, so the host loop is unaffected.
            if self.gen_yielding {
                return Ok(());
            }
            let fi = self.frames.len() - 1;
            let pid = self.frames[fi].proto;
            let ip = self.frames[fi].ip;
            self.frames[fi].ip = ip + 1;
            // Borrow the instruction (no per-step clone — the hot path must not allocate).
            // SAFETY: `program` points into `self.program`'s immutable, never-reassigned
            // `Arc<Program>` (see the loop-entry note); the pointee outlives the loop and `op`
            // borrows program data disjoint from the `&mut self` fields `step` touches.
            let proto_ref = &unsafe { &*program }.protos[pid];
            let op = &proto_ref.code[ip];
            let span = proto_ref.lines[ip];
            // M19 Phase 7 — inline the hottest opcodes here so they skip the per-op `self.step(op, span)`
            // call + its big match jump-table; the long tail delegates to `step`. The inlined arms call
            // the SAME helpers as `step` (or copy its 1–3-line body verbatim), so there is one source of
            // truth per op — keep these in lock-step with `step` if either is edited. `fi` is the current
            // frame index (valid for the `Jump` ip write: jumps never change the frame; `Call`/`Return`
            // re-read frames in their helpers).
            let step_result = match op {
                Op::GetLocal(slot) => {
                    let v = self.stack[self.cur_base + slot];
                    self.stack.push(v);
                    Ok(())
                }
                Op::SetLocal(slot) => {
                    let v = self.pop();
                    self.stack[self.cur_base + slot] = v;
                    Ok(())
                }
                Op::BinLocalLocal { a, b, kind } => self.op_bin_local_local(*a, *b, *kind, span),
                Op::BinLocalConst { slot, val, kind } => {
                    self.op_bin_local_const(*slot, *val, *kind, span)
                }
                Op::IncLocal { slot, delta } => self.op_inc_local(*slot, *delta, span),
                Op::Jump(t) => {
                    self.frames[fi].ip = *t;
                    Ok(())
                }
                Op::JumpIfFalse(t) => {
                    if let Value::Bool(false) = self.pop() {
                        self.frames[fi].ip = *t;
                    }
                    Ok(())
                }
                Op::Call(argc) => self.do_call(*argc, span),
                Op::Return => self.do_return(false),
                // M19 Tier-2 — inline the index ops (hot in the `map` bench) so they skip the `step`
                // call + big-match jump; the helpers carry the Int-key fast path. One source of truth.
                Op::GetIndex => self.get_index(span),
                Op::SetIndex => self.set_index(span),
                // M19 Tier-2 — adaptive opcode quickening (PEP 659). These are the UN-FUSED generic
                // binop arms: `Add..GtEq` here are reached only by stack-operand binops (the
                // `local⊕local`/`local⊕const` windows already fused to superinstructions); `Eq`/`NotEq`
                // are never fused. Each consults a per-site (proto,ip) deopt cell and takes an int/int
                // fast path once warm. Handled here (not in `step`) because the site id needs `pid`+`ip`,
                // which only `run_until` has. The slow path is byte-identical to the kept `step` arms.
                Op::Add => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Add,
                    span,
                ),
                Op::Sub => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Sub,
                    span,
                ),
                Op::Mul => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Mul,
                    span,
                ),
                Op::Div => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Div,
                    span,
                ),
                Op::Mod => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Mod,
                    span,
                ),
                Op::Lt => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Lt,
                    span,
                ),
                Op::LtEq => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::LtEq,
                    span,
                ),
                Op::Gt => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::Gt,
                    span,
                ),
                Op::GtEq => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::vm::op::BinKind::GtEq,
                    span,
                ),
                Op::Eq => self.q_eq(self.quicken_base[pid] as usize + ip, false, span),
                Op::NotEq => self.q_eq(self.quicken_base[pid] as usize + ip, true, span),
                other => self.step(other, span),
            };
            if let Err(rte) = step_result {
                // `std.os.exit(code)` is a hard halt: unwind past every `recover:` to the top.
                if self.pending_exit.is_some() {
                    return Err(rte);
                }
                // B3.4: a cancel observed deeper in this step (a blocking `recv` that woke on the
                // nursery cancel flag set `self.cancelled` and returned the sentinel) unwinds the
                // whole worker — run defers, bypass `recover:`, mirroring the loop-top check. A
                // cancelled task must not be caught and resumed.
                if self.cancelled {
                    let rte = self.unwind_deferred(base_level, false).unwrap_or(rte);
                    return Err(rte);
                }
                // Capture the stack trace of an uncaught fault now, while the frames are still intact
                // (the unwind below drops them). The deepest fault wins: the original fault captures
                // first, and a deeper deferred-call fault (run while its frame is still live) replaces
                // it. A fault this loop CAN catch resets the capture below, so no stale trace survives
                // a `recover:`.
                let caught_here =
                    matches!(self.handlers.last().copied(), Some(h) if h.frame_len > base_level);
                if !caught_here && self.frames.len() > self.fault_trace_depth {
                    self.fault_trace = Some(self.capture_trace());
                    self.fault_trace_depth = self.frames.len();
                }
                // The nearest `recover:` boundary owned by THIS dispatch loop catches the fault; a
                // handler at/below `base_level` belongs to an outer loop, so we unwind to
                // `base_level` and propagate. Either way, every frame discarded on the way runs its
                // deferred calls first (Go: defers run as the panic unwinds, before recover regains
                // control). A fault inside a deferred call supersedes the original.
                let target = match self.handlers.last().copied() {
                    Some(h) if h.frame_len > base_level => h.frame_len,
                    _ => base_level,
                };
                // A genuine fault (not a B3.4 cancel / `std.os.exit`, both handled above) cancels-and-
                // reports each unwound frame's escaped nurseries — emitted PER FRAME, BEFORE that
                // frame's `defer`s, matching the interp oracle (whose `exec_parallel` /
                // `leave_implicit_nursery` report as the body unwinds, then `finish_frame` runs the
                // defers). `unwind_deferred` does the interleaving; this covers BOTH the uncaught arm
                // (no handler) and the frames discarded above a catching `recover:`.
                let rte = self.unwind_deferred(target, true).unwrap_or(rte);
                // A deferred `std.os.exit` turns the unwind into a hard halt.
                if self.pending_exit.is_some() {
                    return Err(rte);
                }
                match self.handlers.last().copied() {
                    Some(h) if h.frame_len > base_level => {
                        self.handlers.pop();
                        // This `recover:` caught the fault — discard any trace captured deeper in (it
                        // belongs to a fault that is now handled), so a later uncaught fault re-captures.
                        self.fault_trace = None;
                        self.fault_trace_depth = 0;
                        // `unwind_deferred` already dropped frames down to `h.frame_len`; restore the
                        // operand stack / call-depth / ip to the boundary's snapshot.
                        self.stack.truncate(h.stack_len);
                        self.call_depth = h.call_depth;
                        self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
                        self.frames[h.frame_len - 1].ip = h.ip;
                        // `unwind_deferred` ran the defers of frames ABOVE the boundary, but the
                        // boundary frame's own (recover-block) defers remain — drain them now, before
                        // binding the result. A fault in one supersedes the original.
                        let rte = self.drain_frame_to(h.defer_len).unwrap_or(rte);
                        // Drop the scope markers of any defer scopes opened inside the recover block:
                        // the fault jumped past their `LeaveDeferScope`s, so they would otherwise leak
                        // and corrupt later drains in this frame.
                        self.frames[h.frame_len - 1]
                            .defer_markers
                            .truncate(h.markers_len);
                        // Reclaim any `parallel:` nursery the fault unwound past (its `JoinNursery`
                        // never ran) — mirrors the interpreter always reclaiming its nursery list.
                        // TASK B: route through `drain_escaped_nursery` so a `?` caught by `recover:`
                        // cancels-and-reports its unstarted tasks IDENTICALLY to an uncaught `?`.
                        self.drain_escaped_nursery(h.nursery_len);
                        if self.pending_exit.is_some() {
                            return Err(rte);
                        }
                        // Convert the fault message (a `str`, i.e. an `Error`) into `Err(msg)`; the
                        // boundary's `done` label receives a ready `Result`.
                        let msg = self.alloc_str(rte.message);
                        let err = self.alloc_enum("Result", "Err", vec![msg]);
                        self.push(err);
                    }
                    // Uncaught: `unwind_deferred(target, true)` above already cancelled-and-reported
                    // every unwound frame's escaped nurseries (the toplevel module nursery preserved).
                    _ => return Err(rte),
                }
            }
            // B1/D3: the running fiber paused — a blocking `recv` parked it, or (D3) it exhausted its
            // reduction budget at the safepoint above and is yielding. Stop the dispatch loop WITHOUT
            // unwinding (frames + defers stay intact to replay on resume) and hand control back up.
            // For a yield detected in a NESTED `run_until`, this is how each outer level bails after
            // its in-flight call op returns — propagating the yield all the way to the worker loop.
            if self.paused() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Mark-sweep collection. Roots: the whole operand stack (which contains every frame's local
    /// slots *and* any in-flight expression temporaries), each frame's home module + backing
    /// closure, and the module namespace cache. Everything else is garbage.
    /// Collect the GC roots held in a parked fiber context (B1): operand-stack objects, each frame's
    /// home/closure and pending deferred calls, and not-yet-run nursery tasks. Mirrors the
    /// live-context rooting in [`Vm::collect`].
    pub(super) fn root_ctx(ctx: &FiberCtx, work: &mut Vec<GcRef>) {
        for v in &ctx.stack {
            if let Value::Obj(h) = v {
                work.push(*h);
            }
        }
        for f in &ctx.frames {
            work.push(f.home);
            if let Some(c) = f.closure {
                work.push(c);
            }
            for d in &f.deferred {
                work.extend(d.roots());
            }
        }
        for nursery in &ctx.nurseries {
            for task in nursery {
                work.extend(task.roots());
            }
        }
    }

    pub(super) fn collect(&mut self) {
        let mut work: Vec<GcRef> = Vec::new();
        for v in &self.stack {
            if let Value::Obj(h) = v {
                work.push(*h);
            }
        }
        for f in &self.frames {
            work.push(f.home);
            if let Some(c) = f.closure {
                work.push(c);
            }
            for d in &f.deferred {
                work.extend(d.roots());
            }
        }
        // Pending `spawn` tasks (C4): their captured callee/receiver/args are roots until the task
        // runs at the nursery's join.
        for nursery in &self.nurseries {
            for task in nursery {
                work.extend(task.roots());
            }
        }
        // Experimental generators — while a generator body runs in the live `Vm` fields above, the
        // host(s) it suspended are parked in `gen_host_ctx` (their frames/stack are not in `self`), so
        // root them here exactly like a parked fiber. The running generators' own handles are roots so
        // their objects survive to be written back (each generator's PARKED ctx, if any, is empty
        // while it runs, so `children` adds nothing extra). Both are empty outside a `generator_next`.
        for host in &self.gen_host_ctx {
            for v in &host.stack {
                if let Value::Obj(h) = v {
                    work.push(*h);
                }
            }
            for f in &host.frames {
                work.push(f.home);
                if let Some(c) = f.closure {
                    work.push(c);
                }
                for d in &f.deferred {
                    work.extend(d.roots());
                }
            }
        }
        work.extend(self.active_generators.iter().copied());
        // Live executors (C5 / A2): their queued work must survive to the program-exit auto-drain
        // even when no in-program handle remains.
        work.extend(self.executors.iter().copied());
        work.extend(self.module_objs.iter().copied());
        // M19 Phase 3 — interned `ConstStr` handles are roots: they're cached for reuse across pushes
        // of the same op, so they must never be swept out from under a later push. Heap-keyed, so this
        // roots the cache for *this* heap (an M:N fiber's cache swapped in with its heap).
        work.extend(self.str_intern.values().copied());
        // Parked fibers in active cooperative schedulers (B1/B2): each level's joining-fiber context
        // plus every child fiber's context are roots while the children run. The CURRENTLY running
        // fiber's context is the live `self.{stack,frames,nurseries}` already rooted above; a parked
        // fiber's context lives in its `FiberCtx` (or, for a not-yet-started child, in its `Pending`
        // task). Without this, a blocked fiber's locals would be swept while it waits.
        // D2a — `scheduler_stack` is the COOPERATIVE engine's parked fibers, which all alias this
        // single `self.heap` (decision A), so `root_ctx` traces their roots into it directly. They
        // carry no heap of their own (`ctx.heap == None`); a parked M:N fiber (D2b) instead owns a
        // share-nothing heap that lives off this `Vm` and is quiescent while parked — it is NEVER
        // traced cross-heap here, only collected when that fiber is next scheduled in and runs its
        // own `run_until` safepoint. (A `--parallel` worker `Vm` has an empty `scheduler_stack`, so
        // this loop is a no-op on workers.)
        for nursery in &self.scheduler_stack {
            debug_assert!(
                nursery.parent.heap.is_none(),
                "a cooperative parked fiber must not own a heap (decision A)"
            );
            Self::root_ctx(&nursery.parent, &mut work);
            for child in &nursery.children {
                debug_assert!(
                    child.ctx.heap.is_none(),
                    "a cooperative child fiber must not own a heap (decision A)"
                );
                Self::root_ctx(&child.ctx, &mut work);
                if let FiberState::Pending(task) = &child.state {
                    work.extend(task.roots());
                }
            }
        }

        while let Some(h) = work.pop() {
            if self.heap.mark(h) {
                work.extend(self.heap.children(h));
            }
        }
        self.heap.sweep();
    }

    pub(super) fn base(&self) -> usize {
        self.cur_base
    }

    pub(super) fn jump(&mut self, target: usize) {
        self.frames.last_mut().unwrap().ip = target;
    }

    pub(super) fn push(&mut self, v: Value) {
        self.stack.push(v);
    }

    pub(super) fn pop(&mut self) -> Value {
        self.stack.pop().expect("operand stack underflow")
    }

    pub(super) fn step(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        match op {
            Op::ConstInt(n) => self.push(Value::Int(*n)),
            Op::ConstFloat(x) => self.push(Value::Float(*x)),
            Op::ConstStr(s) => {
                // M19 Phase 3 — intern by data pointer (stable for the program's lifetime, since the
                // literal lives in the immutable `Arc<Program>`). First push of this op allocs the
                // heap `Obj::Str`; every later push reuses the cached, GC-rooted handle — no clone,
                // no alloc. Sound because strings are immutable and there is no identity operator.
                let key = s.as_ptr() as usize;
                let h = match self.str_intern.get(&key) {
                    Some(&h) => h,
                    None => {
                        let h = self.heap.alloc(Obj::Str(s.as_str().into()));
                        self.str_intern.insert(key, h);
                        h
                    }
                };
                self.push(Value::Obj(h));
            }
            Op::ConstBytes(b) => {
                // Not interned in v1 (unlike `ConstStr`): allocate a fresh `bytes` heap object per
                // push, like a list literal. Bytes literals are not a hot path.
                let h = self.heap.alloc(Obj::Bytes(b.clone()));
                self.push(Value::Obj(h));
            }
            Op::True => self.push(Value::Bool(true)),
            Op::False => self.push(Value::Bool(false)),
            Op::Nil => self.push(Value::Nil),
            Op::Pop => {
                self.pop();
            }
            Op::PopExprStmt => {
                let v = self.pop();
                // An unhandled `Err`/`None` at the top level exits the program.
                if self.frames.last().unwrap().is_toplevel
                    && let Some(e) = self.top_level_error(v, span)
                {
                    return Err(e);
                }
            }
            Op::Assert { has_msg } => {
                // Reached only on the failing path: the compiler emits `Op::Assert` after a
                // `JumpIfFalse` that already consumed (and tested) `cond`, so this op always faults.
                // `msg` (if present) was evaluated lazily just before us — matching the interpreter,
                // which only evaluates `msg` when the assertion fails.
                let message = if *has_msg {
                    let m = self.pop();
                    match self.val_str(m) {
                        Some(s) => format!("assertion failed: {s}"),
                        None => "assertion failed".to_string(),
                    }
                } else {
                    "assertion failed".to_string()
                };
                return Err(self.err(message, span));
            }
            Op::GetLocal(slot) => {
                let v = self.stack[self.base() + slot];
                self.push(v);
            }
            Op::SetLocal(slot) => {
                let v = self.pop();
                let at = self.base() + slot;
                self.stack[at] = v;
            }
            Op::GetGlobalSlot(slot) => {
                let home = self.frames.last().unwrap().home;
                self.ensure_module_faulted(home); // D1: lazily reconstruct the worker's home module
                let v = self.global_slot(home, *slot);
                self.push(v);
            }
            Op::DefineGlobalSlot(slot) => {
                let v = self.pop();
                let home = self.frames.last().unwrap().home;
                self.set_global_slot(home, *slot, v);
            }
            Op::SetGlobalSlot(slot) => {
                let v = self.pop();
                let home = self.frames.last().unwrap().home;
                self.set_global_slot(home, *slot, v);
            }
            Op::GetCaptured(slot) => {
                // Lever #3: hot path is a pure `captured[slot]` index — no string hash. The slot is
                // always in range (one capture per snapshot entry, populated at MakeClosure), and a
                // nested missing parent capture is stored as `Value::Nil` (byte-identical to the old
                // `get(name) -> Some(Nil)`).
                let frame = self.frames.last().unwrap();
                let (clo, home) = (frame.closure, frame.home);
                let v = clo.and_then(|h| match self.heap.get(h) {
                    Obj::Closure { captured, .. } => captured.get(*slot as usize).copied(),
                    _ => None,
                });
                match v {
                    Some(v) => self.push(v),
                    None => {
                        // Cold path: not a closure frame, or slot out of range. Recover the name from
                        // the proto's capture_names and fall back to a home global (D1 lazy fault).
                        self.ensure_module_faulted(home);
                        let proto = self.frames.last().unwrap().proto;
                        let name = self.program.protos[proto]
                            .capture_names
                            .get(*slot as usize)
                            .cloned();
                        let v = name
                            .as_deref()
                            .and_then(|n| self.module_global(home, n))
                            .ok_or_else(|| {
                                let label = name.unwrap_or_else(|| format!("capture#{slot}"));
                                self.err(format!("undefined name '{label}'"), span)
                            })?;
                        self.push(v);
                    }
                }
            }
            Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Mod => self.arith(op, span)?,
            Op::Neg => {
                let v = self.pop();
                let r = match v {
                    Value::Int(n) => n.checked_neg().map(Value::Int).ok_or_else(|| {
                        self.err("integer overflow in negation".to_string(), span)
                    })?,
                    Value::Float(f) => Value::Float(-f),
                    // M22: unary `-` on a struct/enum dispatches to its `neg(self) -> Self` method
                    // (the `Neg` protocol). Mirrors `struct_arith`, but self-only (no `other`).
                    Value::Obj(h)
                        if matches!(self.heap.get(h), Obj::Struct { .. } | Obj::Enum { .. }) =>
                    {
                        let (proto, home) = self.resolve_overload_method(v, "neg", span)?;
                        self.guarded(|vm| {
                            vm.run_proto(proto, home, None, vec![v], true, false, span)
                        })?
                    }
                    other => {
                        return Err(self.err(
                            format!("cannot apply Neg to {}", self.type_name(other)),
                            span,
                        ));
                    }
                };
                self.push(r);
            }
            Op::Not => {
                let v = self.pop();
                match v {
                    Value::Bool(b) => self.push(Value::Bool(!b)),
                    other => {
                        return Err(self.err(
                            format!("cannot apply Not to {}", self.type_name(other)),
                            span,
                        ));
                    }
                }
            }
            Op::Lt | Op::LtEq | Op::Gt | Op::GtEq => self.compare_op(op, span)?,
            Op::Eq => {
                let r = self.pop();
                let l = self.pop();
                let eq = self.values_equal_guarded(l, r, 0, span)?;
                self.push(Value::Bool(eq));
            }
            Op::NotEq => {
                let r = self.pop();
                let l = self.pop();
                let eq = self.values_equal_guarded(l, r, 0, span)?;
                self.push(Value::Bool(!eq));
            }
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr => self.bitwise(op, span)?,
            // `return` (not `?`): keeps `step`'s frame from materializing an extra `RuntimeError`
            // temporary, which would bloat the deep re-entrant recursion path (`str(self)`-style
            // infinite recursion must hit the 10_000 call-depth limit before exhausting the host
            // stack — `self_referential_stringable_hits_depth_limit` guards exactly this).
            Op::Contains => return self.op_contains(span),
            Op::AsBool => {
                let v = *self.stack.last().unwrap();
                if !matches!(v, Value::Bool(_)) {
                    return Err(
                        self.err(format!("expected bool, found {}", self.type_name(v)), span)
                    );
                }
            }
            Op::AsInt => {
                let v = *self.stack.last().unwrap();
                if !matches!(v, Value::Int(_)) {
                    return Err(
                        self.err(format!("expected int, found {}", self.type_name(v)), span)
                    );
                }
            }
            Op::CoerceFloat => {
                // One-way int→float widening (idempotent on Float). Reuses `builtin_float`'s
                // `n as f64`; any non-numeric top is a runtime error (the checker guarantees numeric).
                let top = self.stack.last_mut().unwrap();
                match *top {
                    Value::Int(n) => *top = Value::Float(n as f64),
                    Value::Float(_) => {}
                    other => {
                        return Err(self.err(
                            format!("expected number, found {}", self.type_name(other)),
                            span,
                        ));
                    }
                }
            }
            // ----- M19 superinstructions. Bodies live in `#[inline(never)]` helpers so `step`'s own
            // stack frame stays lean. Plain calls no longer recurse the host stack (call-flattening:
            // `Op::Call` pushes a frame and the running `run_until` loop executes it), but the
            // HOF/method/deferred re-entrant path still cycles `step → run_proto → run_until → step`,
            // so a fat `step` frame would still bloat that recursion. -----
            Op::BinLocalLocal { a, b, kind } => self.op_bin_local_local(*a, *b, *kind, span)?,
            Op::BinLocalConst { slot, val, kind } => {
                self.op_bin_local_const(*slot, *val, *kind, span)?
            }
            Op::IncLocal { slot, delta } => self.op_inc_local(*slot, *delta, span)?,
            Op::PushHandler(target) => self.handlers.push(Handler {
                stack_len: self.stack.len(),
                frame_len: self.frames.len(),
                call_depth: self.call_depth,
                ip: *target,
                defer_len: self.frames.last().map(|f| f.deferred.len()).unwrap_or(0),
                markers_len: self
                    .frames
                    .last()
                    .map(|f| f.defer_markers.len())
                    .unwrap_or(0),
                nursery_len: self.nurseries.len(),
            }),
            Op::PopHandler => {
                self.handlers.pop();
            }
            Op::Jump(t) => self.jump(*t),
            Op::JumpIfFalse(t) => {
                if let Value::Bool(false) = self.pop() {
                    self.jump(*t);
                }
            }
            Op::JumpIfFalseKeep(t) => {
                if let Value::Bool(false) = *self.stack.last().unwrap() {
                    self.jump(*t);
                }
            }
            Op::JumpIfTrueKeep(t) => {
                if let Value::Bool(true) = *self.stack.last().unwrap() {
                    self.jump(*t);
                }
            }
            Op::Call(argc) => self.do_call(*argc, span)?,
            Op::CallMethod { name, argc, ic } => self.do_method_call(name, *argc, *ic, span)?,
            Op::CallStatic {
                type_key,
                method,
                argc,
            } => self.do_static_call(type_key, method, *argc, span)?,
            Op::CallBuiltin(name, argc) => self.do_builtin(name, *argc, span)?,
            Op::LoadBuiltin(name) => {
                let h = self.heap.alloc(Obj::Builtin(name.as_str().into()));
                self.push(Value::Obj(h));
            }
            Op::CallPrint(argc) => self.do_print(*argc, span)?,
            Op::CallPrintSep { argc } => self.do_print_sep(*argc, span)?,
            Op::Return => self.do_return(false)?,
            Op::DeferCall(argc) => self.do_defer(None, *argc, span),
            Op::DeferMethod(name, argc) => self.do_defer(Some(name.clone()), *argc, span),
            Op::EnterDeferScope => {
                let frame = self.frames.last_mut().unwrap();
                let marker = frame.deferred.len();
                frame.defer_markers.push(marker);
            }
            Op::LeaveDeferScope => {
                if let Some(e) = self.leave_defer_scope() {
                    return Err(e);
                }
            }
            Op::DrainHandlerDefers => {
                // The live recover handler is still installed (its `PopHandler` follows). Drain the
                // block's defers down to its marker; a fault propagates and is caught by that same
                // handler (becoming the recover's `Err`).
                if let Some(marker) = self.handlers.last().map(|h| h.defer_len)
                    && let Some(e) = self.drain_frame_to(marker)
                {
                    return Err(e);
                }
            }
            Op::Try => self.do_try(span)?,
            Op::JsonDecode(desc) => {
                let desc = desc.clone();
                self.json_decode(&desc, span)?;
            }
            Op::NewList(n) => {
                let at = self.stack.len() - *n;
                let items: Vec<Value> = self.stack.split_off(at);
                let h = self.heap.alloc(Obj::List(items));
                self.push(Value::Obj(h));
            }
            Op::NewTuple(n) => {
                let at = self.stack.len() - *n;
                let items: Vec<Value> = self.stack.split_off(at);
                let h = self.heap.alloc(Obj::Tuple(items));
                self.push(Value::Obj(h));
            }
            Op::NewMap(n) => {
                // Build an insertion-ordered hash map with last-key-wins upsert. Phase 1 hashes
                // every key while ALL operands are still rooted on the stack (a struct key's hash()
                // re-enters the VM and can GC); phase 2 then builds the map with no further re-entry
                // (so no GC), reading keys/values from the still-rooted stack.
                let count = *n;
                let at = self.stack.len() - 2 * count;
                let mut hashes = Vec::with_capacity(count);
                for j in 0..count {
                    let k = self.stack[at + 2 * j];
                    hashes.push(self.hash_value(k, span)?);
                    // Snapshot a struct/enum/newtype key (Go value-key model) and overwrite its
                    // still-rooted stack slot, so phase 2 stores the snapshot (a later mutation of
                    // the caller's original can't corrupt the map). Landing it in the rooted slot
                    // keeps it alive across the NEXT element's re-entrant `hash_value` GC; values
                    // (odd slots) are left by-reference.
                    let snap = self.snapshot_key(k);
                    self.stack[at + 2 * j] = snap;
                }
                let mut map = MapData::default();
                for (j, &hk) in hashes.iter().enumerate() {
                    let (k, v) = (self.stack[at + 2 * j], self.stack[at + 2 * j + 1]);
                    match map
                        .candidates(hk)
                        .iter()
                        .copied()
                        .find(|&p| self.values_equal(map.entries[p].1, k))
                    {
                        Some(p) => map.entries[p].2 = v,
                        None => map.push(hk, k, v),
                    }
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Map(map));
                self.push(Value::Obj(h));
            }
            Op::NewSet(n) => {
                // Insertion-ordered hash set, dedup keeping first occurrence. Same two-phase rooting
                // as NewMap (phase 1 hashes all elements rooted; phase 2 builds GC-free).
                let count = *n;
                let at = self.stack.len() - count;
                let mut hashes = Vec::with_capacity(count);
                for j in 0..count {
                    hashes.push(self.hash_value(self.stack[at + j], span)?);
                    // Snapshot a struct/enum/newtype element (Go value-key model) into its still-
                    // rooted stack slot, so phase 2 stores the snapshot.
                    let snap = self.snapshot_key(self.stack[at + j]);
                    self.stack[at + j] = snap;
                }
                let mut set = SetData::default();
                for (j, &he) in hashes.iter().enumerate() {
                    let e = self.stack[at + j];
                    if !set
                        .candidates(he)
                        .iter()
                        .copied()
                        .any(|p| self.values_equal(set.entries[p].1, e))
                    {
                        set.push(he, e);
                    }
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Set(set));
                self.push(Value::Obj(h));
            }
            Op::NewStruct(name, argc) => self.new_struct(name, *argc, span)?,
            Op::NewType(type_key) => {
                let inner = self.pop();
                let h = self.heap.alloc(Obj::NewType {
                    type_key: type_key.as_str().into(),
                    inner,
                });
                self.push(Value::Obj(h));
            }
            // ----- cells (uniform by-reference capture, Task A — unwired) -----
            Op::NewCell => {
                // `v` moves straight into the `Obj` (no rooting window — alloc never GCs mid-op,
                // same profile as `Op::NewType`).
                let v = self.pop();
                let h = self.heap.alloc(Obj::Cell(v));
                self.push(Value::Obj(h));
            }
            Op::CellLoad => {
                let ch = self.pop();
                let Value::Obj(h) = ch else {
                    unreachable!("CellLoad on a non-handle value");
                };
                let Obj::Cell(v) = self.heap.get(h) else {
                    unreachable!("CellLoad on a non-cell object");
                };
                let v = *v;
                self.push(v);
            }
            Op::CellStore => {
                // HARD contract: pop the HANDLE first, then the value (operands are `[val, handle]`).
                let ch = self.pop();
                let v = self.pop();
                let Value::Obj(h) = ch else {
                    unreachable!("CellStore on a non-handle value");
                };
                if let Obj::Cell(slot) = self.heap.get_mut(h) {
                    *slot = v;
                } else {
                    unreachable!("CellStore on a non-cell object");
                }
            }
            Op::NewEnum {
                variant,
                variant_id,
                argc,
            } => self.new_enum(variant, *variant_id, *argc, span)?,
            Op::MakeFunc(proto) => {
                let home = self.frames.last().unwrap().home;
                let h = self.heap.alloc(Obj::Func {
                    proto: *proto,
                    home,
                });
                self.push(Value::Obj(h));
            }
            // Body in an `#[inline(never)]` helper so `step`'s frame stays small (the deep-recursion
            // depth-guard test overflows in debug if `step` grows — same discipline as `ToStrFmt`).
            Op::MakeCffi(id) => self.op_make_cffi(*id, span)?,
            Op::MakeClosure(proto, entries) => {
                // Lever #3: build the captured env *positionally* — slot i is the i-th entry (the
                // snapshot order the child proto's `capture_names` mirrors). A nested capture reads
                // the enclosing closure's value by its positional `parent_slot`.
                let frame = self.frames.last().unwrap();
                let (base, home, enclosing) = (frame.base, frame.home, frame.closure);
                let mut captured = Vec::with_capacity(entries.len());
                for e in entries {
                    let v = match e.src {
                        CapSrc::Slot(i) => self.stack[base + i],
                        CapSrc::Captured(parent_slot) => enclosing
                            .and_then(|h| match self.heap.get(h) {
                                Obj::Closure { captured, .. } => {
                                    captured.get(parent_slot as usize).copied()
                                }
                                _ => None,
                            })
                            .unwrap_or(Value::Nil),
                    };
                    captured.push(v);
                }
                let h = self.heap.alloc(Obj::Closure {
                    proto: *proto,
                    captured,
                    home,
                });
                self.push(Value::Obj(h));
            }
            Op::GetField { name, ic } => self.get_field(name, *ic, span)?,
            Op::GetIndex => self.get_index(span)?,
            Op::GetSlice => self.get_slice(span)?, // Phase 4
            Op::SetField { name, ic } => self.set_field(name, *ic, span)?,
            Op::SetIndex => self.set_index(span)?,
            Op::Dup => {
                let top = *self.stack.last().expect("Dup on empty stack");
                self.push(top);
            }
            Op::Dup2 => {
                let n = self.stack.len();
                let a = self.stack[n - 2];
                let b = self.stack[n - 1];
                self.push(a);
                self.push(b);
            }
            Op::ToStr => {
                let v = self.stack[self.stack.len() - 1]; // leave rooted; stringify may run user code
                let s = self.stringify(v, span, 0)?;
                self.pop();
                let h = self.heap.alloc(Obj::Str(s.into()));
                self.push(Value::Obj(h));
            }
            // Body in an `#[inline(never)]` helper so `step`'s frame stays small (the deep-recursion
            // depth-guard test overflows in debug if `step` grows — see commit 1450077).
            Op::ToStrFmt(spec) => self.op_to_str_fmt(spec, span)?,
            Op::BuildStr(n) => {
                let at = self.stack.len() - *n;
                // Stringify in place so each interpolated part stays rooted while a `str` method runs.
                let mut s = String::new();
                for i in 0..*n {
                    let p = self.stack[at + i];
                    self.stringify_into(&mut s, p, span, 0)?; // one buffer, no per-part String
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Str(s.into()));
                self.push(Value::Obj(h));
            }
            Op::ListClone => {
                // Normalise a `for` iterand to an index-iterable list: a list is cloned (so a body
                // that mutates it doesn't disturb iteration); a map yields its keys (gap #14).
                let v = self.pop();
                match v {
                    Value::Obj(h) => match self.heap.get(h) {
                        Obj::List(items) => {
                            let cloned = items.clone();
                            let nh = self.heap.alloc(Obj::List(cloned));
                            self.push(Value::Obj(nh));
                        }
                        Obj::Map(m) => {
                            let keys: Vec<Value> = m.entries.iter().map(|(_, k, _)| *k).collect();
                            let nh = self.heap.alloc(Obj::List(keys));
                            self.push(Value::Obj(nh));
                        }
                        Obj::Set(s) => {
                            let elems: Vec<Value> = s.entries.iter().map(|(_, e)| *e).collect();
                            let nh = self.heap.alloc(Obj::List(elems));
                            self.push(Value::Obj(nh));
                        }
                        // A string iterates as 1-char strings (Python-style; gap: char type).
                        Obj::Str(s) => {
                            // Collect `char`s (Copy — no per-char `String`) to release the heap borrow,
                            // then box each in one alloc via `alloc_char`.
                            let chars: Vec<char> = s.chars().collect();
                            let items: Vec<Value> =
                                chars.into_iter().map(|c| self.alloc_char(c)).collect();
                            let nh = self.heap.alloc(Obj::List(items));
                            self.push(Value::Obj(nh));
                        }
                        // `bytes`/`bytearray` iterate as `int`s (0–255). Snapshots to a list of ints —
                        // mutating the `bytearray` during iteration does not change the loop sequence.
                        Obj::Bytes(b) => {
                            let items: Vec<Value> =
                                b.iter().map(|&x| Value::Int(x as i64)).collect();
                            let nh = self.heap.alloc(Obj::List(items));
                            self.push(Value::Obj(nh));
                        }
                        Obj::ByteArray(b) => {
                            let items: Vec<Value> =
                                b.iter().map(|&x| Value::Int(x as i64)).collect();
                            let nh = self.heap.alloc(Obj::List(items));
                            self.push(Value::Obj(nh));
                        }
                        // A cursor (`for x in pure_iterable_struct` lowered via `IterableToCursor`)
                        // snapshots its REMAINING items to the index-iterable list.
                        Obj::Iter { items, pos } => {
                            let cloned = items[(*pos).min(items.len())..].to_vec();
                            let nh = self.heap.alloc(Obj::List(cloned));
                            self.push(Value::Obj(nh));
                        }
                        _ => {
                            return Err(self
                                .err(format!("cannot iterate over {}", self.type_name(v)), span));
                        }
                    },
                    other => {
                        return Err(self.err(
                            format!("cannot iterate over {}", self.type_name(other)),
                            span,
                        ));
                    }
                }
            }
            Op::ArrLen => {
                let v = self.pop();
                let len = match v {
                    Value::Obj(h) => match self.heap.get(h) {
                        Obj::List(items) => items.len() as i64,
                        _ => unreachable!("ArrLen on non-list"),
                    },
                    _ => unreachable!("ArrLen on non-list"),
                };
                self.push(Value::Int(len));
            }
            Op::IsStruct => {
                let v = self.pop();
                let is_struct =
                    matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Struct { .. }));
                self.push(Value::Bool(is_struct));
            }
            Op::IsGenerator => {
                let v = self.pop();
                let is_gen =
                    matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Generator(_)));
                self.push(Value::Bool(is_gen));
            }
            Op::IsCursor => {
                let v = self.pop();
                let is_cursor =
                    matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Iter { .. }));
                self.push(Value::Bool(is_cursor));
            }
            Op::IterableToCursor => {
                // One-time `for`-entry conversion: a PURE-`Iterable` struct (has `iter`, lacks `next`)
                // becomes its cursor (so the seq path drains it); everything else passes through.
                let v = self.pop();
                let convert = if let Value::Obj(h) = v
                    && let Obj::Struct { name, .. } = self.heap.get(h)
                {
                    let name = name.clone();
                    self.program.structs.get(name.as_ref()).map(|d| {
                        (
                            !d.methods.contains_key("next") && d.methods.contains_key("iter"),
                            d.methods.get("iter").copied(),
                            d.module_idx,
                        )
                    })
                } else {
                    None
                };
                match convert {
                    Some((true, Some(proto), module_idx)) => {
                        let home = self.module_objs[module_idx];
                        // Re-enter the VM to run `iter(self)`; it returns the cursor (the body calls
                        // `self.xs.iter()`). Root the receiver across the call (guarded GC).
                        self.push(v);
                        let cursor = self.guarded(|vm| {
                            vm.run_proto(proto, home, None, vec![v], true, false, span)
                        })?;
                        self.pop(); // unroot receiver
                        self.push(cursor);
                    }
                    // Not a pure-Iterable struct (a struct with `next`, a generator, a collection, …):
                    // unchanged. (A pure-Iterable struct whose `iter` is somehow missing is impossible
                    // — the checker bound it via `struct_iterable_elem`, which requires `iter`.)
                    _ => self.push(v),
                }
            }
            Op::Yield => {
                // Experimental generator suspend. The yielded value is already on the stack top; flag
                // the request and let `run_until` return to the host `.next()` after this op (the
                // frame `ip` has already advanced past the `Yield`, so resume continues after it).
                self.gen_yielding = true;
            }
            Op::IsMap => {
                let v = self.pop();
                let is_map = matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Map(_)));
                self.push(Value::Bool(is_map));
            }
            Op::IsChannel => {
                let v = self.pop();
                let is_chan =
                    matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Channel(_)));
                self.push(Value::Bool(is_chan));
            }
            Op::ChanRecvOrClosed => {
                // `for v in ch:` step: pop a value (parking on empty-open exactly like `recv`) and push
                // `Some(v)`, or push `None` once the channel is closed-and-drained (the loop's clean
                // exit). Runs at the loop top, never inside a native callback (`native_reentry == 0`),
                // so it takes the snapshot-park / cooperative-park / fault paths — never the demote path.
                let v = self.pop();
                let Value::Obj(h) = v else {
                    return Err(self.err("`for` over a non-channel value".to_string(), span));
                };
                match self.chan_recv_step(h, span)? {
                    RecvStep::Got(w) => {
                        let val = self.from_wire(w);
                        let opt = self.alloc_enum("Option", "Some", vec![val]);
                        self.push(opt);
                    }
                    RecvStep::ClosedEmpty => {
                        let opt = self.alloc_enum("Option", "None", vec![]);
                        self.push(opt);
                    }
                    // `chan_recv_step` re-rooted the handle + set `suspend`; `run_until`'s `paused()`
                    // gate returns to the scheduler, and the op re-runs (rewound `ip`) on resume.
                    RecvStep::Parked => {}
                }
            }
            Op::EnsureEnum(slot) => {
                let v = self.stack[self.base() + *slot];
                if !matches!(v, Value::Obj(h) if matches!(self.heap.get(h), Obj::Enum { .. })) {
                    return Err(self.err(format!("cannot match on {}", self.type_name(v)), span));
                }
            }
            Op::MatchArm {
                scrut,
                variant,
                variant_id,
                enum_name,
                nbind,
                bind_start,
                next,
            } => self.match_arm(
                *scrut,
                variant,
                *variant_id,
                enum_name.as_deref(),
                *nbind,
                *bind_start,
                *next,
                span,
            )?,
            Op::MatchNoArm(slot) => {
                let v = self.stack[self.base() + *slot];
                let variant = match v {
                    Value::Obj(h) => match self.heap.get(h) {
                        Obj::Enum { variant_id, .. } => self.enum_names(*variant_id).1.to_string(),
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                return Err(self.err(format!("no match arm for variant '{variant}'"), span));
            }
            Op::EnterNursery => {
                self.nurseries.push(Vec::new());
                self.mn_scopes.push(None); // lockstep — set Some(scope_id) only if early-enlisted
                // TASK B — capture this parallel body's defer floor so a recover-scoped `?` can run
                // the body's defers before the cancel-report (see `nursery_defer_floors`).
                let floor = self.frames.last().map(|f| f.deferred.len()).unwrap_or(0);
                self.nursery_defer_floors.push(floor);
                // Per-connection spawn — a NESTED nursery under `--parallel` (entered inside a live
                // fiber, `mn.is_some()`) activates an EAGER sched NOW so `spawn`s in the body inject
                // handlers that run concurrently with the accept loop. The top-level nursery
                // (`mn.is_none()`) and the cooperative engine stay lazy (queue-at-join → `None`).
                //
                // Gated on ≥2 hardware threads: an eager inner join blocks the parent's OUTER worker
                // (decision B — parent participates) while it waits for handlers, and a handler that
                // services an OUTER sibling (a client) needs that sibling to make progress — which it
                // can't if the outer nursery has only ONE worker (a 1-core box → every nursery is
                // single-worker → deadlock). With ≥2 hw threads the outer nursery has a spare worker.
                // On a single core we fall back to the lazy queue-at-join path (handlers drain at the
                // join), which still serves a realistic parallel-client server and never deadlocks —
                // and `--parallel` on one core is already a degenerate config.
                let eager = self.parallel && self.mn.is_some() && worker_count() >= 2;
                let scope = eager.then(|| self.activate_eager_nursery());
                self.eager_scheds.push(scope);
            }
            Op::JoinNursery => self.join_nursery()?,
            // TASK B — `break`/`continue` leaving a `parallel:` scope: cancel-and-report its unstarted
            // tasks and pop exactly that one level (the compiler emits one per escaped scope).
            Op::ReclaimNursery => {
                let from = self.nurseries.len().saturating_sub(1);
                self.drain_escaped_nursery(from);
            }
            Op::SpawnCall(argc) => self.do_spawn(None, *argc, span)?,
            Op::SpawnMethod(name, argc) => self.do_spawn(Some(name.clone()), *argc, span)?,
            Op::SpawnBlock(proto, entries) => self.do_spawn_block(*proto, entries, span)?,
            Op::WaitPoll(meta) => self.op_wait_poll(meta, span)?,
            Op::NewChannel => {
                let h = self
                    .heap
                    .alloc(Obj::Channel(Arc::new(ChannelCore::default())));
                self.push(Value::Obj(h));
            }
            Op::NewShared => {
                let init = self.pop();
                // The box holds the wire form (single serialization == the old deep_clone-in). A
                // non-sendable init (a frame-holding generator) faults gracefully with this Op's span.
                let init = self.to_wire_at(init, span)?;
                let h = self.heap.alloc(Obj::Shared(Arc::new(SharedCore {
                    v: Mutex::new(init),
                    ..Default::default()
                })));
                self.push(Value::Obj(h));
            }
            Op::NewRwShared => {
                let init = self.pop();
                // The box holds the wire form (single serialization == the old deep_clone-in). A
                // non-sendable init (a frame-holding generator) faults gracefully with this Op's span.
                let init = self.to_wire_at(init, span)?;
                let h = self.heap.alloc(Obj::RwShared(Arc::new(RwSharedCore {
                    v: RwLock::new(init),
                    ..Default::default()
                })));
                self.push(Value::Obj(h));
            }
            // `NewAtomic`/`NewTimer` delegate to `#[inline(never)]` helpers so their locals (the timer's
            // `Instant`/`Duration` math) do NOT inflate `step`'s stack frame — `step` is on the per-op
            // recursion path, so a fatter frame here multiplies across a deep call chain (debug builds
            // don't reuse match-arm stack slots) and can overflow the host stack before the
            // `MAX_CALL_DEPTH` guard fires. Keep these cold constructors out of line.
            Op::NewAtomic => {
                let v = self.new_atomic(span)?;
                self.push(v);
            }
            Op::NewTimer => {
                let v = self.new_timer(span)?;
                self.push(v);
            }
            Op::NewExecutor => {
                let h = self
                    .heap
                    .alloc(Obj::Executor(Arc::new(ExecutorCore::default())));
                // Register for the program-exit auto-drain; the handle is also a GC root, so the
                // executor's queued work survives even after every in-program handle is gone.
                self.executors.push(h);
                self.push(Value::Obj(h));
            }
        }
        Ok(())
    }

    // ----- arithmetic / comparison -----
}

#[cfg(test)]
mod cell_ops_tests {
    use super::*;
    use crate::vm::tests::empty_program;

    fn new_vm() -> Vm {
        Vm::new(Arc::new(empty_program()))
    }

    /// `NewCell` boxes the popped value on the heap; `CellLoad` reads the inner value back out.
    #[test]
    fn newcell_cellload_roundtrips() {
        let mut vm = new_vm();
        let span = Span::default();
        vm.push(Value::Int(7));
        vm.step(&Op::NewCell, span).unwrap();
        vm.step(&Op::CellLoad, span).unwrap();
        assert_eq!(vm.pop(), Value::Int(7));
    }

    /// HARD contract: operands are pushed value-THEN-handle (stack `[val, handle]`), so `CellStore`
    /// pops the handle FIRST, then the value, and writes the value into the cell in place.
    #[test]
    fn cellstore_pops_handle_first() {
        let mut vm = new_vm();
        let span = Span::default();
        vm.push(Value::Int(7));
        vm.step(&Op::NewCell, span).unwrap();
        let h = vm.pop();
        // Push value THEN handle: [Int(9), handle].
        vm.push(Value::Int(9));
        vm.push(h);
        vm.step(&Op::CellStore, span).unwrap();
        // Reload through the same handle → observes the stored value.
        vm.push(h);
        vm.step(&Op::CellLoad, span).unwrap();
        assert_eq!(vm.pop(), Value::Int(9));
    }
}
