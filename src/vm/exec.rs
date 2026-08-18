// vm::exec — split out of vm/mod.rs. `super::*` == the `vm` module.
// VM core: construction, frames, generators, run/run_until/step dispatch.

use super::*;

impl Vm {
    /// The stdout sink. DEFAULT (`host.stream == false`) = append to the captured `out` buffer:
    /// byte-identical to what every test helper and embedder has always seen. STREAM (`chezzi run`
    /// only) = hand the whole `print` to the stdout writer thread
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
        self.emit_out_bytes(s.as_bytes());
    }

    /// The byte-level stdout sink [`Vm::emit_out`] delegates to — same contract, no UTF-8 hop.
    /// `Writer.write_bytes` on an `io.stdout()` backing lands here so `b"\xff\xfe"` reaches the real
    /// handle unchanged (W6-9: it used to be decoded with `from_utf8_lossy` and emit two U+FFFD).
    /// The buffered sink is a `Vec<u8>` for the same reason; it decodes ONCE, at the Rust capture
    /// boundary ([`Vm::take_out`] and the `run_*` helpers) — never where a comparison is made (a
    /// lossy decode is not injective, so it would blind the CPython differential and the
    /// two-worker-count `tests/chz` gate, the surviving accidental-divergence detectors).
    pub(super) fn emit_out_bytes(&mut self, b: &[u8]) {
        if self.host.stream {
            // `write_out` counts the write itself, so `invoke_native` can ask "did THIS native emit
            // to stdout" — see [`Vm::stdout_writes`]. Only the streamed branch: off the CLI path
            // `stream_halt` is inert anyway, so a buffered write has nothing to gate.
            stream::write_out(self, b);
        } else {
            self.out.extend_from_slice(b);
        }
    }

    /// The fault to raise at a print site once the streamed stdout is dead — an ORDINARY recoverable
    /// `RuntimeError`, which is what composes correctly with everything the exit channel broke: it
    /// unwinds through defers, can be caught by `recover:`, loses to nothing at a cross-task join, and
    /// exits NON-ZERO with a trace on stderr (still live — `| head` closes only stdout). Python raises
    /// `BrokenPipeError` here for the same reason. Without a halt at all, `chezzi run x.chz | head -1`
    /// would spin forever on a dead pipe: Rust ignores SIGPIPE, and restoring it process-wide would
    /// break `std.net`'s EPIPE-as-an-error contract. (Note the "process-wide": Go scopes SIGPIPE by
    /// FD — fd 1/2 signal, every other fd returns `EPIPE` — and so has both. See the safe-direction
    /// observation under `gaps.md` W7-5e. Not ruled out, just not what we do: an in-VM fault composes
    /// with `defer`/`recover:`/task joins in ways a signal cannot, and `chezzi` is also library code.)
    ///
    /// ponytail: a defer that prints while a REAL fault is unwinding raises this fault too, so its
    /// message replaces the original's (the run still exits non-zero, with a trace — only the message
    /// names the pipe instead of the first cause). Threading an `in_unwind` flag through the unwind
    /// path would preserve it; not worth it until someone is actually confused by it.
    pub(super) fn stream_halt(&self, span: Span) -> Option<RuntimeError> {
        if !self.host.stream {
            return None;
        }
        stream::out_dead_reason().map(|why| RuntimeError {
            message: why,
            span,
            is_assert: false,
            is_over_memory: false,
            is_timed_out: false,
        })
    }

    /// The stderr sink — same contract as [`Vm::emit_out`], on a SEPARATE writer + lock (so a task's
    /// `print` and `eprint` can reorder relative to each other, exactly like Python's).
    pub(super) fn emit_err(&mut self, s: &str) {
        self.emit_err_bytes(s.as_bytes());
    }

    /// The byte-level stderr sink — the twin of [`Vm::emit_out_bytes`] (W6-9).
    pub(super) fn emit_err_bytes(&mut self, b: &[u8]) {
        if self.host.stream {
            stream::write_err(b);
        } else {
            self.stderr.extend_from_slice(b);
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
        Vm {
            program,
            heap: Heap::new(),
            stack: Vec::new(),
            frames: Vec::new(),
            out: Vec::new(),
            stderr: Vec::new(),
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
            nurseries: Vec::new(),
            mn_scopes: Vec::new(),
            mn_enlisted: 0,
            mn_enlist_sched: None,
            eager_scheds: Vec::new(),
            nursery_defer_floors: Vec::new(),
            executors: Vec::new(),
            exec_registry: Arc::new(Mutex::new(Vec::new())),
            sched_registry: Arc::new(Mutex::new(Vec::new())), // W7-56
            suspend: None,
            wait_suspend: None,
            send_suspend: None,
            offload: None,
            poll_park: None,
            pending_connect: None,
            wire_backref_missing: false, // W7-11
            poll_timed_out: false,
            poll_deadline: None,
            poll_partial: None,
            native_reentry: 0,
            walk_base: 0,
            eq_hook_off: false,
            stdout_writes: 0,
            reds: 0,             // D3 — set to CONTEXT_REDS per schedule-in (run_one_fiber)
            yield_now: false,    // D3
            gen_yielding: false, // experimental generators
            gen_host_ctx: Vec::new(),
            active_generators: Vec::new(),
            wid: 0,         // D5 owe #3 (Path C) — set in mn_worker_loop
            demoted: false, // D5 owe #3 (Path C)
            cancel: None,
            cancel_outer: Vec::new(),
            cancelled: false,
            eager_core: None,
            quiesce: Arc::new(crate::vm::quiesce::QuiesceState::default()),
            timeout_ms: 0,
            deadline: None,
            back_edge_tick: 0,
            deferring: 0,
            module_snapshot: None,
            module_faulted: Vec::new(),
            snapshot_memo: None,
            snapshot_rebuild: super::fxhash::FxHashMap::default(),
            snapshot_cells: std::sync::Arc::new(super::fxhash::FxHashMap::default()),
            snapshot_next_id: 0,
            snapshot_builds: 0,
            mn: None,
        }
    }

    pub(super) fn err(&self, message: String, span: Span) -> RuntimeError {
        RuntimeError {
            message,
            span,
            is_assert: false,
            is_over_memory: false,
            is_timed_out: false,
        }
    }

    /// The shared recoverable fault raised by every structural walker when recursion exceeds
    /// [`MAX_STRUCTURAL_DEPTH`] (cyclic data). Single source of truth for the message so it stays in
    /// sync with the constant; callers pass their own span.
    pub(super) fn depth_exceeded_err(&self, span: Span) -> RuntimeError {
        self.err(
            format!("maximum structural depth ({MAX_STRUCTURAL_DEPTH}) exceeded (cyclic data structure?)"),
            span,
        )
    }

    /// B3.4 — set this VM's nursery cancel flag (if it runs under one), so sibling workers abort.
    /// No-op when this VM runs under no nursery (`cancel` is `None`) — e.g. the top-level VM.
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
        // stdin is NOT swapped: it is ONE source every task shares (Go/Python) — see `Stdin` and
        // `spawn_worker`, which hands the M:N worker the same shared handle.
        //
        // Task 1 — the fiber's module-globals view swaps for EVERY fiber; those `GcRef`s index the
        // fiber's own heap, swapped just below, so they travel WITH the fiber. The parent's REAL
        // modules move into `ctx.module_objs` on its swap-out. W6-2 — `module_snapshot` + `snapshot_memo` swap WITH them: a snapshot describes a
        // module VIEW, and one shell drains fibers holding DIFFERENT views, so each must fault in (and
        // re-snapshot) from its OWN. They carry no `GcRef` (heap-independent `SnapValue`), so GC
        // rooting is unaffected.
        std::mem::swap(&mut self.module_objs, &mut ctx.module_objs);
        std::mem::swap(&mut self.module_faulted, &mut ctx.module_faulted);
        std::mem::swap(&mut self.module_snapshot, &mut ctx.module_snapshot);
        std::mem::swap(&mut self.snapshot_memo, &mut ctx.snapshot_memo);
        // W7-4a — the snapshot rebuild map describes the SAME view, so it travels with it. Unlike the
        // two `Arc<ModuleSnapshot>`s above it IS heap-keyed (`GcRef` values), exactly like
        // `module_objs` just above: for an M:N fiber it indexes the heap swapped below, for a fiber
        // with no heap of its own the shared heap. Either way it moves atomically with its view.
        std::mem::swap(&mut self.snapshot_rebuild, &mut ctx.snapshot_rebuild);
        // W7-4c — the snapshot cell registry is heap-keyed too (its KEYS are `GcRef`s into the heap the
        // snapshot was built from), so it travels with the same view for the same reason.
        std::mem::swap(&mut self.snapshot_cells, &mut ctx.snapshot_cells);
        // W7-4c — the counter travels WITH the registry it numbers; see `FiberCtx::snapshot_cells`.
        // Split them and a fiber resuming on a fresher shell re-mints ids its own registry already
        // uses, merging two unrelated bindings.
        std::mem::swap(&mut self.snapshot_next_id, &mut ctx.snapshot_next_id);
        // D2a — an M:N fiber (`Some`) owns its heap; swap it with the host's. A fiber with no heap of
        // its own (`None`) shares the single `Vm::heap` (decision A), so its heap is left untouched.
        // D2b — the same `Some` gate carries the fiber's remaining heap-keyed side state
        // (out/stderr/executors/intern), so they move atomically WITH the heap their `GcRef`s index.
        // A fiber with no heap of its own swaps none of that.
        // GC: this atomicity is why a swapped-OUT `module_objs` needs no pin — it parks in `ctx`
        // together with the heap it indexes, and `collect()` only ever walks the LIVE `self.heap`.
        // The one production `FiberCtx` (`ReadyWorker::into_fiber`) always carries `heap: Some`.
        if let Some(ctx_heap) = ctx.heap.as_mut() {
            std::mem::swap(&mut self.heap, ctx_heap);
            std::mem::swap(&mut self.out, &mut ctx.out);
            std::mem::swap(&mut self.stderr, &mut ctx.stderr);
            std::mem::swap(&mut self.executors, &mut ctx.executors);
            // M19 Phase 3 — the intern cache's `GcRef`s index this fiber's OWN heap, so it MUST travel
            // atomically with the heap (same heap-keyed argument as `module_objs`). A fiber with no
            // heap of its own (`heap: None`) never reaches here and keeps aliasing the shell's cache.
            std::mem::swap(&mut self.str_intern, &mut ctx.str_intern);
            // D6b — a mid-flight `connect` parked on writability swaps WITH its fiber (it owns the
            // connecting fd that the netpoller is watching; it must not be left on the shell where the
            // next fiber would inherit or drop it).
            std::mem::swap(&mut self.pending_connect, &mut ctx.pending_connect);
            // D6c — a socket timeout marker set by the poll thread (on the detached fiber's ctx) swaps
            // in here so the resumed socket op sees it at entry. M:N-only, like `pending_connect`.
            std::mem::swap(&mut self.poll_timed_out, &mut ctx.poll_timed_out);
            // B1 — the in-flight `read`'s latched deadline belongs to the parked fiber's op (which
            // re-executes on wake), so it swaps with the fiber exactly like `poll_timed_out`.
            std::mem::swap(&mut self.poll_deadline, &mut ctx.poll_deadline);
            // N3(a) — the taken-partial flag is set BEFORE the park and consulted at the re-entry, so
            // it must travel with the fiber exactly like `poll_deadline`.
            std::mem::swap(&mut self.poll_partial, &mut ctx.poll_partial);
        }
    }

    /// B1 / D3 — the running fiber paused mid-flight and its frames stay live to replay on resume:
    /// either a blocking `recv` parked it (`suspend`) or it exhausted its D3 reduction budget
    /// (`yield_now`). Both unwind every nested `run_until` / call site the SAME way — propagate up
    /// WITHOUT popping a result or pushing a sentinel — so every "callee paused" gate tests this, not
    /// `suspend` alone. (`yield_now` is only ever set under the M:N engine — the safepoint gates it on
    /// `mn.is_some()` — so a run with no M:N scheduler, where it is always false, is unchanged by
    /// construction.)
    pub(super) fn paused(&self) -> bool {
        self.suspend.is_some()
            || self.wait_suspend.is_some()
            || self.send_suspend.is_some()
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
        self.guarded_checkpoint()?;
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

    /// [`Vm::guarded`], plus: run `f` with [`Vm::walk_base`] set to `base`, the structural depth the
    /// ENCLOSING native walk had already consumed. Every `MAX_STRUCTURAL_DEPTH` guard tests
    /// `walk_base + depth`, so a chain of nested `eq`/`str` hooks shares ONE 10 000 allowance instead
    /// of each re-entry restarting at 0 — without this, hook-nesting depth × per-hook walk depth is
    /// unbounded and the process dies by host stack overflow (uncatchable, rc=134).
    ///
    /// The `catch_unwind` is load-bearing, not decoration: `guarded` catches the unwind, decrements
    /// `native_reentry`, and **resumes the unwind from inside itself**, so any `self.walk_base =
    /// saved` written AFTER the `guarded(...)` call would be skipped on a panic. Panics really do
    /// traverse this seam — `callback_trampoline`'s `catch_unwind` converts an FFI-callback panic
    /// into a recoverable error, and `run_one_fiber`'s turns a worker panic into `Disp::Finish` and
    /// keeps the shell `Vm` alive for the next fiber. A leaked `walk_base` on a shell would make
    /// every later fiber's `==`/`str` fault spuriously.
    ///
    /// Three exit paths, one assignment: `Ok` restores, `Err` restores (it rides inside `r`, not the
    /// unwind), a panic restores before the re-raise.
    pub(super) fn guarded_walk<T>(
        &mut self,
        base: usize,
        f: impl FnOnce(&mut Self) -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        let saved = std::mem::replace(&mut self.walk_base, base);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.guarded(f)));
        self.walk_base = saved;
        match r {
            Ok(v) => v,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    /// The throttled `--timeout` clock read for [`Vm::guarded_checkpoint`]'s deadline rung, kept
    /// OUT OF LINE — the one perf-sensitive thing about this whole rung.
    ///
    /// `#[cold] #[inline(never)]` is load-bearing and was arrived at by measurement, not taste.
    /// Written inline in `guarded_checkpoint` the rung cost **+4.5 % on `benches/chz/loop.chz`
    /// (1.451 s → 1.517 s), a bench that never calls `guarded` at all** — pure code-layout
    /// perturbation, reproducible across 20-run A/Bs and isolated by deleting only these lines.
    /// Moving the rungs out of the generic `guarded` body did NOT help (1.520 s); pushing this block
    /// out of line did: 1.446 s, level with the 1.450 s of the same build without the rung.
    /// If this is ever inlined back, re-run `hyperfine` on `loop` before believing it is free.
    #[cold]
    #[inline(never)]
    fn hof_deadline_tick(&mut self) -> Result<(), RuntimeError> {
        self.back_edge_tick = self.back_edge_tick.wrapping_add(1);
        if self.back_edge_tick.is_multiple_of(1024)
            && let Some(fr) = self.frames.last()
        {
            let span = fr.call_span;
            self.deadline_halt(span)?;
        }
        Ok(())
    }

    /// The three halt rungs [`Vm::guarded`] runs before re-entering user code — `--timeout`, then
    /// cancel, then a run-wide `os.exit`, the same order `jump_checked` uses.
    ///
    /// Non-generic and out of line: `guarded` is generic over its callback, so its body is
    /// monomorphized at every call site and these rungs would be duplicated into all of them.
    #[inline(never)]
    fn guarded_checkpoint(&mut self) -> Result<(), RuntimeError> {
        // CANCELLATION CHECKPOINT — a native that re-enters user code drives it from a RUST loop
        // (`list.map`/`filter`/`fold`, `sort`'s comparator, an operator overload, an `Executor`
        // handler: `for e in .. { self.guarded(|vm| vm.invoke_value(f, ..))? }`, call.rs). That Rust
        // loop emits no `Op::Jump`, so `jump_checked`'s back-edge never fires inside it and a
        // straight-line callback body has no back-edge of its own — a cancelled task would burn every
        // remaining element (with its prints / `Shared` writes / fs writes) to completion. The
        // per-element re-entry IS this loop's back-edge, so it is where the cancel is delivered; the
        // `?` on `guarded` aborts the native loop, exactly as the old every-instruction check did via
        // the callback's nested `run_until`. A DEFERRED call also runs through `guarded`
        // (`run_one_deferred`) — `cancel_requested` is false while `deferring > 0`, so the defer body
        // itself is never killed here (that bug swallowed the LIFO-first defer of any task that
        // returned normally / faulted on its own under a tripped scope flag).
        // `chezzi test --timeout` — FIRST, because a wall-clock cap outranks both cancel and exit
        // (W7-18/W7-17), which is the rung order `jump_checked` uses. Without it this checkpoint was
        // asymmetric with that one: `--timeout=500` killed a plain loop at 505 ms but let the same work
        // written as `xs.map(..).fold(..)` run to 1985 ms and report **PASS**. A cap that green-lights a
        // test which blew through it by 4× is worse than a cap that is merely late — it teaches
        // distrust of every green run.
        //
        // **Not** suppressed while `deferring > 0`, unlike the cancel and exit rungs below — matching
        // `jump_checked`'s deadline rung exactly. A cap a `defer` can outrun is the same hole one rung
        // down, and `--timeout` is the backstop those two suppressions are allowed to lean on.
        //
        // This fn runs per ELEMENT of every `map`/`filter`/`fold`/`sort_by`, so the tick + clock read
        // live in the out-of-line [`Vm::hof_deadline_tick`] (see it — the placement is measured, not
        // stylistic) and are throttled 1/1024 on the shared `back_edge_tick`. The `deadline.is_some()`
        // gate is checked BEFORE the call, so a run with the cap off — the common case, and every
        // `chezzi run` — neither ticks nor reads the clock, same as `jump_checked`. `deadline_halt`
        // produces the `.timed_out()`-marked error the runner reports as `TIMED-OUT`, rather than
        // re-deriving the message here.
        if self.deadline.is_some() {
            self.hof_deadline_tick()?;
        }
        if self.cancel_requested()
            && let Some(fr) = self.frames.last()
        {
            let span = fr.call_span;
            self.cancelled = true;
            return Err(self.err("cancelled".to_string(), span));
        }
        // gaps.md W7-57 — the run-wide `os.exit` rung, BELOW cancel exactly as in `jump_checked`. The
        // per-element re-entry is this Rust loop's only back-edge, so without it a party with no
        // applicable cancel flag — a top-level `main`, an eager `Executor` job — runs the whole HOF to
        // completion after the exit. Measured on the release binary: `range(0, 10_000_000).map(f)
        // .fold(0, g)` on `main` beside a job exiting at 50 ms ran the full 2154 ms AND printed its
        // completion line, versus Go's immediate `os.Exit`.
        //
        // (`--timeout` had the identical gap at this checkpoint and is closed by the rung ABOVE — the
        // two were found together and are fixed together; see it for why it is first and unthrottled by
        // `deferring`.)
        //
        // [`Vm::exit_halt`], not `run_exit_err`: it is suppressed while `deferring > 0` — which is the
        // whole reason this fn raises that counter before calling here — and it sends a fiber that
        // holds a cancel flag down the `Cancelled` path so its `defer`s still run.
        if self.quiesce.exit_pending()
            && let Some(fr) = self.frames.last()
        {
            let span = fr.call_span;
            if let Some(e) = self.exit_halt(span) {
                return Err(e);
            }
        }
        Ok(())
    }

    // ----- experimental generators (VM-only) -----

    /// Swap the live execution context (frames/stack/depth/base/handlers) with a parked [`GenCtx`].
    /// Smaller sibling of [`Vm::swap_ctx`]: a generator shares the host heap (the same share-by-ref,
    /// decision A, as a fiber with no heap of its own) and cannot open nurseries/spawn
    /// (checker-forbidden), so none of the
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
        Value::obj(self.heap.alloc(Obj::Generator(Box::new(core))))
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
        // NURSERY-FLOOR REBASE. `nurseries` is NOT part of `GenCtx` (never swapped by `swap_gen_ctx`),
        // so `self.nurseries` is the RESUMING driver's stack, while a parked frame's / recover
        // handler's `nursery_len` was captured (absolutely) against whatever floor existed when the
        // generator was FIRST driven — a different, possibly deeper, driver's floor (or, across the
        // airlock, a stale SENDER floor). A generator provably opens NO nursery of its own (`spawn` /
        // `parallel:` are checker-banned inside a generator, recover blocks included), so its
        // frame-return drain (`do_return` → `drain_escaped_nursery(frame.nursery_len)`) and its
        // recover-catch drain (`drain_escaped_nursery(handler.nursery_len)`) MUST be no-ops — they
        // must never truncate the driver's own live nurseries. Rebase both to the current floor so
        // every generator-internal drain is a no-op. Identity when the drive floor is unchanged (the
        // common case: existing generators drive at a fixed depth), so it is behaviour-preserving
        // there; it fixes a latent same-heap over-drain when a generator is resumed deeper than it
        // was first driven, and makes a cross-airlock handler's stale `nursery_len` safe.
        // On the first (Pending) drive `self.frames`/`self.handlers` are empty here (the body frame is
        // pushed just below), so this is a no-op then; the pushed frame gets the correct floor from
        // `push_frame`.
        let floor = self.nurseries.len();
        for f in &mut self.frames {
            f.nursery_len = floor;
        }
        for hd in &mut self.handlers {
            hd.nursery_len = floor;
        }
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
    /// error that exits the program.
    pub(super) fn top_level_error(&self, v: Value, span: Span) -> Option<RuntimeError> {
        let h = v.as_obj()?;
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
        self.reset_for_invoke();
        let home = self.entry_home();
        self.run_proto(proto, home, None, Vec::new(), true, false, Span::RUNTIME)?;
        Ok(())
    }

    /// The per-invocation reset EVERY `chezzi test` entry point shares — [`Vm::invoke_test`],
    /// [`Vm::invoke_suite_method`] (test methods AND all four lifecycle hooks) and
    /// [`Vm::build_suite_instance`]. One `Vm` serves the whole test FILE, so whatever one invocation
    /// latches must be dropped before the next: the heap `over_cap` latch, the wall-clock deadline,
    /// and the run-wide `os.exit` cell.
    ///
    /// **The `os.exit` reset lives here, not at one call site.** W7-47's defect #1 put
    /// `clear_exit` in `invoke_test` alone, so an `os.exit` inside a SUITE method latched for the rest
    /// of the file: every later method and every `after_each`/`after_all` hook died with `exit`,
    /// falsifying `test_runner`'s "after_each always runs, even on failure, like `defer`". W7-57's
    /// back-edge rung escalated it from "later tests that BLOCK" to anything containing a loop or a
    /// native HOF (measured: `B::b_loops`, a bare `while`, went `PASS` → `ERROR … exit`).
    fn reset_for_invoke(&mut self) {
        self.reset_over_memory();
        self.quiesce.clear_exit();
        self.arm_deadline();
    }

    /// Bare `chezzi run` with a `module:function` manifest entrypoint — invoke a named top-level
    /// function of the entry module after `run()` has initialized all modules. Looks the name up in
    /// the entry module's namespace (so a re-exported import works too) and calls it with no args.
    /// A missing name (or a non-callable binding) is a clear runtime error rather than a silent no-op.
    pub fn invoke_entrypoint(&mut self, fn_name: &str) -> Result<(), RuntimeError> {
        let span = Span::RUNTIME;
        let home = self.entry_home();
        // Read the binding by name from the entry module's slot table (mirrors `module_define`).
        let callee = match self.heap.get(home) {
            Obj::Module(m) => m.index.get(fn_name).map(|&i| m.slots[i as usize]),
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
            callee.as_obj(),
            Some(h) if matches!(
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
        // Symmetric with the unhandled-top-level-Err rule: if the entry fn returns `Err(..)`/`None`,
        // surface it as "unhandled error: <detail>" (rc=1) rather than silently discarding it. This
        // lets a manifest entrypoint legitimately be `-> T!` and use `?`.
        let ret = self.invoke_value(callee, Vec::new(), span)?;
        if let Some(e) = self.top_level_error(ret, span) {
            return Err(e);
        }
        Ok(())
    }

    /// `chezzi test` — invoke a suite method/lifecycle hook proto with `self` bound to `recv` (a
    /// suite instance). Returns the method's value (ignored by the runner) or its fault.
    pub fn invoke_suite_method(
        &mut self,
        proto: ProtoId,
        recv: Value,
    ) -> Result<Value, RuntimeError> {
        self.reset_for_invoke();
        let home = self.entry_home();
        self.run_proto(proto, home, None, vec![recv], true, false, Span::RUNTIME)
    }

    /// `chezzi test` — construct a suite instance via its synthetic zero-arg `__new_<Suite>` thunk.
    pub fn build_suite_instance(&mut self, new_thunk: ProtoId) -> Result<Value, RuntimeError> {
        self.reset_for_invoke();
        let home = self.entry_home();
        self.run_proto(
            new_thunk,
            home,
            None,
            Vec::new(),
            true,
            false,
            Span::RUNTIME,
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

    /// `chezzi test --max-heap=<N>` — set the per-test live-heap cap in bytes (`0` = OFF, the
    /// default). A test whose live heap exceeds `N` is hard-aborted (bypassing `recover:`) and
    /// bucketed `OverMemory`. Deterministic-in-VM (not OS RSS). The trip point is a per-heap
    /// high-water check, so a real runaway (unbounded growth) aborts on whichever heap runs it. A
    /// CONCURRENT test near the boundary can still slip through — each M:N worker isolates its own
    /// heap, so per-fiber allocations that only sum over the cap may not trip — a documented per-heap
    /// limit, see `docs/future.md §3b`.
    pub fn set_max_heap(&mut self, cap: usize) {
        self.heap.set_mem_cap(cap);
    }

    /// `chezzi test --timeout=<MS>` — set the per-test wall-clock cap in ms (`0` = OFF, the default).
    /// A test running longer than `MS` is hard-aborted (bypassing `recover:`) and bucketed `TimedOut`.
    /// A wall-clock trip is non-deterministic, which is why it is OFF unless a test asks for it.
    /// Observed at the loop back-edge (+ spawned fibers), and —
    /// for the ops a back-edge cannot reach — in `block_halt_check` (blocked in place),
    /// `block_until_deadline` (a wait whose deadline we own, W7-16) and at `chan_recv_step` /
    /// `op_wait_poll`'s PARK, which is the only path to a parked fiber (W7-17).
    pub fn set_timeout(&mut self, ms: u64) {
        self.timeout_ms = ms;
    }

    /// Thread an already-armed absolute deadline onto this VM (an M:N worker inherits the parent's).
    pub(super) fn set_deadline(&mut self, dl: Option<std::time::Instant>) {
        self.deadline = dl;
    }

    /// Arm the per-test wall-clock deadline for the test about to run: `now + timeout_ms`, or `None`
    /// when the cap is OFF (so the back-edge check reads no clock). Called at every invoke entry point
    /// beside [`Vm::reset_over_memory`].
    fn arm_deadline(&mut self) {
        self.deadline = if self.timeout_ms == 0 {
            None
        } else {
            Some(std::time::Instant::now() + std::time::Duration::from_millis(self.timeout_ms))
        };
        self.back_edge_tick = 0;
    }

    /// Clear the heap `over_cap` latch so a tripped test never taints the next on this reused VM.
    /// Called at the top of every test/hook/construction invoke entry point.
    fn reset_over_memory(&mut self) {
        self.heap.clear_over_cap();
    }

    /// `chezzi test` — take + clear whatever a test printed to stdout, resetting the buffer so the
    /// next test starts clean. The documented LOSSY embedder accessor: kept for any consumer that
    /// wants a `String` and accepts the `from_utf8_lossy` round-trip (e.g. a substring match against
    /// captured output). The byte-exact sibling is [`Vm::take_out_bytes`] — `chezzi test
    /// --show-output` uses that one so a test's raw stdout reaches fd 1 unchanged (W6-9r item 4).
    ///
    /// The buffered sink is bytes (W6-9); this is one of the CAPTURE boundaries where Rust needs a
    /// `String`, so it decodes lossily here. `chezzi run` (the path a program's stdout actually
    /// reaches an fd) never passes through it and stays byte-exact. Not for a byte-level comparison —
    /// that takes `vm::run_file_bytes` (see `vm::RunOutputRaw`).
    pub fn take_out(&mut self) -> String {
        String::from_utf8_lossy(&self.take_out_bytes()).into_owned()
    }

    /// The byte-exact sibling of [`Vm::take_out`] — take + clear whatever a test printed to stdout,
    /// with no UTF-8 hop. `chezzi test --show-output` uses this so non-UTF-8 captured stdout
    /// (`b"\xff\xfe"`) reaches fd 1 unchanged, matching `chezzi run` (W6-9) and `go test`.
    pub fn take_out_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// `chezzi test` — drain anything the program left running (e.g. an Executor a test forgot to
    /// shut down), mirroring the ordinary run's graceful reap. Best-effort: ignore drain faults so a
    /// stray resource doesn't mask the test verdict.
    pub fn reap_after_tests(&mut self) {
        let _ = self.drain_live_executors();
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
        let mod_obj = self.heap.alloc(Obj::Module(Box::new(ModuleData {
            name: m.label.clone().into_boxed_str(),
            slots: vec![Value::nil(); m.global_slots.len()],
            index,
        })));
        debug_assert_eq!(self.module_objs.len(), idx);
        self.module_objs.push(mod_obj);

        // A native std module: populate its globals with Rust `NativeFn`s + float constants. It then
        // FALLS THROUGH to `run_proto` below so a HYBRID native module's BODIED Chezzi fns (e.g.
        // `math.divmod`) get their globals bound by running the module toplevel — the compiler emits
        // `MakeFunc`/`DefineGlobalSlot` for them into their own reserved slots, distinct from the
        // name-keyed native members appended here. A pure-native module has no bodied decls, so its
        // compiled toplevel is empty and `run_proto` is a no-op. A hybrid native file may also `import`,
        // so the import loop below binds those deps (empty/no-op for a pure-native module).
        if let Some(name) = m.native {
            for (mname, func, kind) in crate::native::native_members(name) {
                let nat = self.heap.alloc(Obj::Native {
                    name: (*mname).into(),
                    func: *func,
                    kind: *kind,
                });
                self.module_define(mod_obj, mname, Value::obj(nat));
            }
            for (cname, cval) in crate::native::native_consts(name) {
                let fv = self.box_float(*cval);
                self.module_define(mod_obj, cname, fv);
            }
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
            Span::RUNTIME,
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
                self.module_define(into, &name, Value::obj(target_obj));
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
                            "Shared" | "RwShared" | "Atomic" | "AtomicInt" | "Executor"
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
                    // to `Ty::Socket`. Skip them — the native module has no such global by design;
                    // without this, `import Socket from std.net` faults.
                    if self.module_name(target_obj) == "std.net"
                        && matches!(member.as_str(), "Socket" | "Listener")
                    {
                        continue;
                    }
                    // R2 — `std.io`'s `Writer` is a TYPE-only import with NO runtime module-member value
                    // (a `Writer` value comes from `create`/`append`/`stdout`/…; the type resolves
                    // directly to `Ty::Writer`). Skip it — the module has no such global. Its openers
                    // (`create`/`append`/`stdout`/`stderr`/`buffered`) DO bind normally (real MEMBERS
                    // values). Without this, `import Writer from std.io` faults.
                    // R2b — same for `std.io`'s `Reader` TYPE (a value comes from `open`; the type
                    // resolves directly to `Ty::Reader`). Without this, `import Reader from std.io` faults.
                    if self.module_name(target_obj) == "std.io"
                        && (member == "Writer" || member == "Reader")
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
            return Ok(Value::nil());
        }
        Ok(self.stack.pop().unwrap_or(Value::nil()))
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
        // How many arguments actually arrived, BEFORE the nil-reservation below makes that
        // unknowable. A short count means the caller omitted trailing defaulted parameters and the
        // callee's own prologue must fill them (`Op::JumpIfProvided`).
        let argc = self.stack.len() - base;
        // Reserve the remaining (non-parameter) local slots.
        while self.stack.len() < base + n_slots {
            self.stack.push(Value::nil());
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
            argc,
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
                // Rendered, not raw: a W7-51 default-argument provider's internal name is
                // deliberately unspellable (`$def$2$f.x$`) and would be unreadable in a trace.
                function: crate::desugar::display_fn_name(&self.program.protos[f.proto].name)
                    .into_owned(),
                span: f.call_span,
            })
            .collect()
    }

    // ----- the dispatch loop -----

    pub(super) fn run_until(&mut self, base_level: usize) -> Result<(), RuntimeError> {
        // M19 — hoist the per-entry `Arc::clone(&self.program)`: borrow the program by raw
        // pointer instead of bumping the refcount. `self.program` is an immutable
        // `Arc<Program>` set once in `Vm::new` and NEVER reassigned (M:N workers each build
        // their own `Vm`; `swap_ctx` swaps heap/frames/stack, not `program`), so the pointee
        // outlives this loop and the borrow is disjoint from
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
                // `chezzi test --max-heap` — if this test's live heap tripped the cap during the
                // sweep just run, hard-abort it. Modeled on the `self.cancelled` funnel below: unwind
                // with `report = false` so `recover:` CANNOT catch it. The check is RE-OBSERVED like a
                // cancel checkpoint — no latch — so a `defer` that itself allocates runaway during the
                // abort's cleanup unwind is bounded too (its own nested `run_until` re-trips here and
                // aborts it at its first GC boundary). `should_collect()` resets after each collect, so
                // a non-allocating defer never re-checks and runs to completion; only an allocating one
                // re-trips. Zero cost when the cap is off (`over_cap()` is false forever when
                // `mem_cap == 0`, and this only runs after an actual — infrequent — collect).
                if self.heap.over_cap() {
                    let over_rte = self
                        .err(
                            format!("test exceeded --max-heap ({} bytes)", self.heap.mem_cap()),
                            Span::default(),
                        )
                        .over_memory();
                    // FORCE the `is_over_memory` marker onto whatever error emerges (a `defer` may
                    // fault mid-unwind and replace it), so the marker travels WITH the error through
                    // every enclosing `run_until` (native re-entry) and the worker→parent fault
                    // boundary — that is what keeps the abort un-catchable by `recover:` (the Err
                    // funnel below bypasses `recover:` whenever the marker is set) and correctly
                    // bucketed `OverMemory`.
                    let rte = self
                        .unwind_deferred(base_level, false)
                        .map(RuntimeError::over_memory)
                        .unwrap_or(over_rte);
                    return Err(rte);
                }
            }
            // CANCELLATION POINTS (the every-instruction cancel check that used to sit here is GONE).
            // A cancel is observed only at CHECKPOINTS: loop back-edges (`jump_checked`, below) and
            // blocking/park ops (`chan_recv_step` / `op_wait_poll` / the blocking-native offload).
            // Consequence, intended (Trio-style structured concurrency): a STARTED task always runs
            // its straight-line prologue, so a `defer` it registers is ALWAYS registered before
            // anything can kill it — "does my cleanup run?" no longer depends on scheduler timing.
            // The cancel still unwinds like an uncaught fault that bypasses `recover:` — see the
            // post-step funnel below (`self.cancelled` ⇒ `unwind_deferred(base_level, false)`).
            //
            // D3: reduction-counting preemption — gated on `self.mn` (an M:N worker shell running fibers
            // off the shared queue); a shell with no sched in scope is never preempted. Decrement the budget per dispatched op; at
            // exhaustion yield this worker so a queued sibling runs (round-robin fairness). A yielded
            // cancelled fiber observes the cancel at its next checkpoint. The
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
                // A cancellation CHECKPOINT: a backward `Op::Jump` is a loop back-edge (see
                // `jump_checked`) — keep in lock-step with `step`'s arm.
                Op::Jump(t) => self.jump_checked(*t, span),
                Op::JumpIfFalse(t) => {
                    if self.pop().as_bool() == Some(false) {
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
                //
                // `--max-heap`: an over-memory hard-abort raised in a NESTED `run_until` (a HOF
                // callback / operator overload / deferred call) bubbles here as an ordinary `?`-error
                // carrying the `is_over_memory` marker. It gets the SAME bypass-recover unwind as a
                // cancel — the loop-top check only fires within its own `run_until` level, so without
                // this sibling arm an outer `recover:` would catch the fault and defeat the guard. Re-
                // stamp the marker onto whatever emerges (a mid-unwind `defer` fault) so it keeps
                // travelling up.
                //
                // `--timeout`: identical treatment for the `is_timed_out` marker — a wall-clock abort
                // raised at a back-edge in a nested `run_until` (a HOF callback's own loop) bubbles
                // here and must keep bypassing `recover:` the same way.
                //
                // W7-3 CARVE-OUT — the (a) `self.cancelled` marker ONLY. `self.cancelled` is a
                // task-wide LATCH that stays set while the cancelled task's `defer`s run, but "a
                // `defer` is never itself cancelled" (docs/concurrency.md) and `cancel_suppressed`
                // already carries the same `deferring > 0` suppression. So a `recover:` owned by
                // THIS nested dispatch loop (`frame_len > base_level` — i.e. installed inside the
                // defer body, not outside it) catches while `deferring > 0`. A `recover:` OUTSIDE
                // the defer sits at/below `base_level`, so it still cannot defeat the cancel, and
                // once the defer body finishes the pending cancel resumes travelling up.
                // (b) `is_over_memory` and (c) `is_timed_out` are NOT gated: a `--max-heap` /
                // `--timeout` abort stays recover-proof everywhere, including inside a defer.
                //
                // `caught_here` is the CONSERVATIVE arm, not a load-bearing one: measured on the real
                // binary, dropping it (`!(self.deferring > 0)`) leaves every test in
                // `tests/chz/spec/cancel_defer_recover_test.chz` byte-identical — with
                // no handler above `base_level` the fault returns `Err` either way. It is kept because
                // it preserves the bypass in MORE cases (a cancelled task is more likely to die), which
                // is the safe direction. Do not "simplify" it away without re-deriving that.
                let caught_here =
                    matches!(self.handlers.last().copied(), Some(h) if h.frame_len > base_level);
                let cancel_bypass = self.cancelled && !(self.deferring > 0 && caught_here);
                if cancel_bypass || rte.is_over_memory || rte.is_timed_out {
                    let over_mem = rte.is_over_memory;
                    let timed = rte.is_timed_out;
                    let rte = self.unwind_deferred(base_level, false).unwrap_or(rte);
                    let rte = if over_mem { rte.over_memory() } else { rte };
                    let rte = if timed { rte.timed_out() } else { rte };
                    return Err(rte);
                }
                // Capture the stack trace of an uncaught fault now, while the frames are still intact
                // (the unwind below drops them). The deepest fault wins: the original fault captures
                // first, and a deeper deferred-call fault (run while its frame is still live) replaces
                // it. A fault this loop CAN catch resets the capture below, so no stale trace survives
                // a `recover:`.
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
                        // never ran) — the nursery list is always reclaimed on unwind.
                        // TASK B: route through `drain_escaped_nursery` so a `?` caught by `recover:`
                        // cancels its tasks IDENTICALLY to an uncaught `?`.
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
    pub(super) fn collect(&mut self) {
        let mut work: Vec<GcRef> = Vec::new();
        for v in &self.stack {
            if let Some(h) = v.child_gcref() {
                work.push(h);
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
                if let Some(h) = v.child_gcref() {
                    work.push(h);
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
        // W7-4a — the snapshot rebuild map's cells, heap-keyed like `str_intern` (so this roots them
        // for *this* heap). BELT AND BRACES today, and deliberately kept: every entry is currently
        // also reachable from the module global it was just `module_define`d into, and MEASURED —
        // `airlock_cross_module_shared_binding_is_one_cell` still passes with this line deleted. The
        // map now outlives a single fault, though, so the moment any entry enters it before something
        // else roots it, this is the only thing holding it. Cheap; do not "clean it up".
        work.extend(self.snapshot_rebuild.values().copied());
        // W7-4c — the snapshot cell registry's KEYS. Unlike the map above this root is LOAD-BEARING:
        // an unrooted key could be swept and its slot recycled to a different cell, which the seeding
        // in `deep_clone_all`/`lower_task` would then identify with the dead cell's id and merge into
        // the wrong binding. A rooted key is never swept, so never recycled.
        work.extend(self.snapshot_cells.keys().copied());
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

    /// The loop-BACK-EDGE cancellation checkpoint (the only per-instruction cancel check left; the
    /// old every-instruction one at the dispatch-loop top is gone — see `run_until`). The frame's
    /// stored `ip` is already `ip + 1` for the op being dispatched, so `target < ip` is exactly a
    /// backward jump, i.e. a loop back-edge (pinned by `compiler::back_edge_tests`). A long-running
    /// CPU loop therefore stays promptly cancellable, while straight-line code (a task's prologue,
    /// its `defer` registration) runs to completion first.
    ///
    /// `!self.cancelled` latches on the first observation: a `defer` containing a loop must not
    /// re-fire the check and skip the remaining defers while the cancel unwind is already in flight.
    /// The `Err` is funnelled by `run_until`'s post-step handler into `unwind_deferred(base_level,
    /// false)` — defers run, `recover:` inside the task is bypassed (a cancelled task must die).
    pub(super) fn jump_checked(&mut self, target: usize, span: Span) -> Result<(), RuntimeError> {
        let ip = self.frames.last().unwrap().ip;
        if target < ip {
            // `chezzi test --timeout` — observe the wall-clock deadline at THE loop back-edge, the
            // hottest engine-independent checkpoint. It covers both the top-level test body
            // (`invoke_test → run_proto → run_until`, which runs OUTSIDE the fiber scheduler) AND a
            // `spawn`ed fiber (its loop routes through here too), so a single check catches both.
            // Zero clock reads when the cap is OFF: the `Some` guard short-circuits before any
            // `Instant::now()`. Throttled to one read per 1024 back-edges (the read is the cost). The
            // `is_timed_out` marker alone drives the recover-bypass — no `self.cancelled` latch.
            // The 1/1024 sample is shared by both wall-clock rungs below. Hoisted OUT of the `deadline`
            // guard (W7-57) because the exit rung needs it even when `--timeout` is off; the clock read
            // itself stays behind `Some(dl)`, so an uncapped run still reads no clock.
            self.back_edge_tick = self.back_edge_tick.wrapping_add(1);
            let sampled = self.back_edge_tick.is_multiple_of(1024);
            if sampled
                && let Some(dl) = self.deadline
                && std::time::Instant::now() >= dl
            {
                return Err(self
                    .err(
                        format!("test exceeded --timeout ({}ms)", self.timeout_ms),
                        span,
                    )
                    .timed_out());
            }
            if self.cancel_requested() {
                self.cancelled = true;
                return Err(self.err("cancelled".to_string(), span));
            }
            // gaps.md W7-57 — a run-wide `os.exit` from another party. This is the checkpoint for the
            // shapes NO blocking wait can reach: a spinning top-level `main` (`cancel == None`,
            // `cancel_outer` empty — it hung forever) and a spinning eager `Executor` job (whose
            // `cancel` is its executor's `shutdown_now` token, which an `os.exit` must NOT trip). So the
            // rung is deliberately NOT gated on "has no cancel flag" — the job HAS one, it is just the
            // wrong one.
            //
            // BELOW cancel, and routed through [`Vm::exit_halt`] rather than `run_exit_err` — that
            // helper is what makes a SCOPED fiber unwind as `Cancelled` (defers intact) while a
            // flagless one gets the exit sentinel, and what suppresses both inside a `defer`.
            if sampled && let Some(e) = self.exit_halt(span) {
                return Err(e);
            }
        }
        self.frames.last_mut().unwrap().ip = target;
        Ok(())
    }

    /// THE cancel predicate — every cancellation checkpoint (`jump_checked`'s loop back-edge,
    /// `guarded`'s native-HOF re-entry, the blocking-native offload, `chan_recv_step`, `op_wait_poll`,
    /// [`Vm::demote_block_socket`], [`Vm::join_eager_jobs`]) asks exactly this. Two
    /// suppressions, both load-bearing:
    ///
    /// * `!self.cancelled` — latch: once the cancel unwind is in flight, a checkpoint inside it must
    ///   not re-fire and skip the remaining `defer`s.
    /// * `self.deferring == 0` — a deferred call is the cleanup the cancel exists to run. Defers drain
    ///   on the normal-return / own-fault paths too, where `cancelled` is still false while the scope
    ///   flag is already tripped by a faulted sibling; without this, the first checkpoint inside the
    ///   first deferred call returns `cancelled` and the defer body never executes.
    pub(super) fn cancel_requested(&self) -> bool {
        !self.cancel_suppressed() && self.cancel_flags().any(|c| c.load(Ordering::Relaxed))
    }

    /// gaps.md W7-57 — how the two CPU-side checkpoints ([`Vm::jump_checked`]'s loop back-edge,
    /// [`Vm::guarded`]'s native-HOF re-entry) deliver a run-wide `os.exit`. These two are the only
    /// halts a spinning party ever reaches, and they need one thing the blocking rungs' bare
    /// [`Vm::run_exit_err`] does not give them:
    ///
    /// **A fiber that HOLDS a cancel flag unwinds as `Cancelled`, not as an exit** — so it runs its
    /// `defer`s exactly as it does for a sibling fault, matching the pre-W7-57 behaviour. Ordering the publication (`request_exit` → `halt_all_scheds` → flag) does NOT
    /// achieve this and the claim that it did was wrong: an `Acquire` load orders only the reads that
    /// FOLLOW it, and both sites read cancel BEFORE exit, so `cancel == false` + `exit == true` is a
    /// legal interleaving. Measured as a nondeterministic `defer`: 2/8 runs, and one killed mid-body.
    /// Deciding it HERE, from the flag's mere presence, makes it deterministic instead.
    ///
    /// A party with NO flag at all still gets `pending_exit` + the `"exit"` sentinel: a top-level
    /// `main` (`cancel == None`, `cancel_outer` empty), which is precisely the shape that hung
    /// forever. An eager `Executor` job DOES hold one (its executor's `shutdown_now` token), so it
    /// takes the cancel path — it still dies promptly, and its submitter's join reports the code.
    ///
    /// `cancel_suppressed()` gates both arms, so a `defer` body is never entered *or* truncated by
    /// this rung; and `pending()` — the `Mutex` cell, the authority — is confirmed before either arm,
    /// because the atomic is only a lock-free HINT and a `chezzi test` reset clears the cell.
    fn exit_halt(&mut self, span: Span) -> Option<RuntimeError> {
        if self.cancel_suppressed()
            || !self.quiesce.exit_pending()
            || self.quiesce.pending().is_none()
        {
            return None;
        }
        if self.cancel_flags().next().is_some() {
            self.cancelled = true;
            return Some(self.err("cancelled".to_string(), span));
        }
        self.run_exit_err(span)
    }

    /// The flags `cancel_requested()` reads: this fiber's own scope flag plus every ENCLOSING scope's
    /// (structured concurrency — an outer cancel cancels this one too; a nested nursery keeps its own
    /// flag for its own faults, and `cancel_outer` is normally empty).
    ///
    /// SINGLE SOURCE — `demote_cancel_flags` (the N4 watch set) MUST stay exactly the flags the resume
    /// path re-reads, so both go through this + [`Vm::cancel_suppressed`] rather than hand-copying the
    /// set. (A watch that misses a flag ⇒ a cancel-wakeable demoted fiber is called a deadlock and its
    /// cleanup is truncated; a watch that misses a suppression ⇒ the veto never lifts and a genuine
    /// deadlock hangs silently.)
    fn cancel_flags(&self) -> impl Iterator<Item = &Arc<AtomicBool>> {
        self.cancel.iter().chain(self.cancel_outer.iter())
    }

    /// The two suppressions of the cancel predicate — a tripped flag does NOT cancel this fiber while
    /// either holds, and neither can change while it is blocked in place.
    fn cancel_suppressed(&self) -> bool {
        self.cancelled || self.deferring > 0
    }

    /// N4 (M:N) — the cancel flags a DEMOTED fiber must be watched on, i.e. the exact flags
    /// `cancel_requested()` reads. EMPTY when a cancel could not wake it at all (`cancel_suppressed`:
    /// already unwinding, or inside a `defer`) — a fiber a cancel can never wake is exactly the one
    /// that IS a genuine deadlock. Handed to `SchedCore::watch_demoted_cancel`
    /// (`demote_recv_block` / `demote_wait_block`).
    pub(super) fn demote_cancel_flags(&self) -> Vec<Arc<AtomicBool>> {
        if self.cancel_suppressed() {
            return Vec::new();
        }
        self.cancel_flags().cloned().collect()
    }

    /// The cancel-flag chain a nursery CREATED FROM THIS VM must inherit: the scopes enclosing it, i.e.
    /// this VM's own scope flag appended to its own ancestors. Empty at the top level.
    ///
    /// …and EMPTY inside a `defer` (`deferring > 0`): a `parallel:`/`spawn` opened by a cancelled task's
    /// CLEANUP is the cleanup's own work and must not inherit the already-tripped enclosing flag, or its
    /// children die at their first checkpoint and the cleanup is silently truncated through the back
    /// door. The `deferring > 0` suppression that makes a defer uncancellable is per-VM and does NOT
    /// cross the airlock into a worker fiber (a fresh `Vm` with `deferring == 0`), so the severance has
    /// to happen HERE, where the child's flag chain is built. The defer's OWN nursery still gets a
    /// fresh flag, so it can cancel its own children.
    /// `Op::EnterNursery` — outlined from the dispatch `match` deliberately.
    ///
    /// §2c1 grew this arm from three lines to a page, and `run_until`'s hot loop pays for every arm's
    /// code size whether or not the program ever reaches it: `benches/loop.chz` executes no nursery
    /// opcode at all and still measured **+3.1%** with the arm inline (1437 → 1481 ms, medians of 80
    /// runs, minima non-overlapping), the same shape `W7-57` measured at +4.5% on this same bench.
    /// `#[inline(never)]` keeps the arm one call instruction.
    #[inline(never)]
    fn op_enter_nursery(&mut self) {
        // W6-2 — invalidation rule 2: a nursery's tasks must see module globals as of THIS open,
        // and a global holding a mutable aggregate can have been mutated IN PLACE (`q.push(1)`,
        // `m[k] = v`, `p.x = 1`) since the cached snapshot was built, with no module-slot write
        // for rule 1 (`set_global_slot`/`module_define`) to catch. So drop a non-`reusable` cache
        // entry here; an all-immutable view keeps its one snapshot for the whole run (a
        // nursery-in-a-loop program builds exactly one). See `ModuleSnapshot::reusable`.
        if self.snapshot_memo.as_ref().is_some_and(|s| !s.reusable) {
            self.snapshot_memo = None;
            // W7-4c — the registry numbers that snapshot; drop it with the cache.
            self.snapshot_cells = std::sync::Arc::new(super::fxhash::FxHashMap::default());
        }
        self.nurseries.push(Vec::new());
        self.mn_scopes.push(None); // lockstep — set Some(scope_id) only if early-enlisted
        // TASK B — capture this parallel body's defer floor so a recover-scoped `?` can run
        // the body's defers before the nursery reclaim (see `nursery_defer_floors`).
        let floor = self.frames.last().map(|f| f.deferred.len()).unwrap_or(0);
        self.nursery_defer_floors.push(floor);
        // §2c1 — EVERY nursery on the M:N engine activates an EAGER sched NOW, so a `spawn`
        // in the body injects a LIVE fiber that runs concurrently with the rest of the body.
        // That is Go's `go f()`: the task starts at the `spawn`, and the join keeps its own
        // (orthogonal) job of guaranteeing COMPLETION by the barrier (`docs/future.md` §2b/§2c1).
        //
        // `mn.is_some()` WAS the defect and is gone: a top-level nursery has no worker shell,
        // so it was lazy by construction and its tasks could not start until the join. A
        // nursery entered on a thread that already has an eager scope open does not build a
        // sched at all — it registers a SCOPE on that one (`activate_eager_nursery`), which is
        // what keeps sibling nurseries mutually visible.
        //
        // `worker_count() >= 2` STAYS, and only for `mn.is_some()` — a nursery entered inside a
        // spawned task, the one shape that still builds a private sched with its own dedicated
        // raw drainer thread. That is the case its original rationale was written for (an eager
        // inner join blocking its parent's OUTER worker while a handler needs an outer sibling
        // to progress — impossible when the outer nursery has one worker), and it is also the
        // only per-nursery THREAD source left: dropping it broke `pool.rs`'s documented bound
        // that live threads stay at `N + joiners` "regardless of `parallel:` nesting depth" —
        // measured, depth 7 / 128 leaves at `--threads=1` went 3 threads → 130.
        //
        // A top-level nursery has no outer worker to starve and creates exactly ONE drainer per
        // thread, so it is unconditional.
        let eager = self.mn.is_none() || worker_count() >= 2;
        // `flatten` — `activate_eager_nursery` returns `None` if the OS refuses its drainer
        // thread, which falls back to the lazy queue-at-join path rather than leaving a
        // worker-less eager scope that would hang a blocking body.
        let scope = eager.then(|| self.activate_eager_nursery()).flatten();
        self.eager_scheds.push(scope);
    }

    pub(super) fn scope_ancestors(&self) -> Vec<Arc<AtomicBool>> {
        if self.deferring > 0 {
            return Vec::new();
        }
        let mut a = self.cancel_outer.clone();
        if let Some(c) = &self.cancel {
            a.push(Arc::clone(c));
        }
        a
    }

    pub(super) fn push(&mut self, v: Value) {
        self.stack.push(v);
    }

    pub(super) fn pop(&mut self) -> Value {
        self.stack.pop().expect("operand stack underflow")
    }

    pub(super) fn step(&mut self, op: &Op, span: Span) -> Result<(), RuntimeError> {
        match op {
            Op::ConstInt(n) => {
                // A source literal may exceed ±2^62 (still within i64) → box it.
                let v = self.make_int(*n);
                self.push(v);
            }
            Op::ConstFloat(x) => {
                let v = self.box_float(*x);
                self.push(v);
            }
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
                self.push(Value::obj(h));
            }
            Op::ConstBytes(b) => {
                // Not interned in v1 (unlike `ConstStr`): allocate a fresh `bytes` heap object per
                // push, like a list literal. Bytes literals are not a hot path.
                let h = self.heap.alloc(Obj::Bytes(b.clone()));
                self.push(Value::obj(h));
            }
            Op::True => self.push(Value::bool(true)),
            Op::False => self.push(Value::bool(false)),
            Op::Nil => self.push(Value::nil()),
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
                // `msg` (if present) was evaluated lazily just before us — `msg` is only
                // evaluated when the assertion fails.
                let message = if *has_msg {
                    let m = self.pop();
                    match self.val_str(m) {
                        Some(s) => format!("assertion failed: {s}"),
                        None => "assertion failed".to_string(),
                    }
                } else {
                    "assertion failed".to_string()
                };
                // The ONE site that flags an assert failure — the `test fn` runner buckets this as
                // FAIL, every other (`is_assert: false`) fault as ERROR.
                return Err(RuntimeError {
                    message,
                    span,
                    is_assert: true,
                    is_over_memory: false,
                    is_timed_out: false,
                });
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
                // nested missing parent capture is stored as `Value::nil()` (byte-identical to the old
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
                let r = self.neg_value(v, span)?;
                self.push(r);
            }
            Op::Not => {
                let v = self.pop();
                match v.as_bool() {
                    Some(b) => self.push(Value::bool(!b)),
                    None => {
                        return Err(
                            self.err(format!("cannot apply Not to {}", self.type_name(v)), span)
                        );
                    }
                }
            }
            Op::Lt | Op::LtEq | Op::Gt | Op::GtEq => self.compare_op(op, span)?,
            // `return`, like `Op::Contains` below: `eq_operator` may re-enter user code (a struct/enum
            // `eq`), so keeping its String/Vec temporaries out of `step`'s frame matters on the deep
            // `step → run_proto → run_until → step` path.
            Op::Eq => return self.eq_operator(false, span),
            Op::NotEq => return self.eq_operator(true, span),
            Op::BitAnd | Op::BitOr | Op::BitXor | Op::Shl | Op::Shr => self.bitwise(op, span)?,
            // `return` (not `?`): keeps `step`'s frame from materializing an extra `RuntimeError`
            // temporary, which would bloat the deep re-entrant recursion path (`str(self)`-style
            // infinite recursion must hit the 10_000 call-depth limit before exhausting the host
            // stack — `self_referential_stringable_hits_depth_limit` guards exactly this).
            Op::Contains => return self.op_contains(span),
            Op::AsBool => {
                let v = *self.stack.last().unwrap();
                if v.as_bool().is_none() {
                    return Err(
                        self.err(format!("expected bool, found {}", self.type_name(v)), span)
                    );
                }
            }
            Op::AsInt => {
                let v = *self.stack.last().unwrap();
                if !self.is_integral(v) {
                    return Err(
                        self.err(format!("expected int, found {}", self.type_name(v)), span)
                    );
                }
            }
            Op::CoerceFloat => {
                // One-way int→float widening (idempotent on Float). Reuses `builtin_float`'s
                // `n as f64`; any non-numeric top is a runtime error (the checker guarantees numeric).
                let v = *self.stack.last().unwrap();
                if let Some(n) = self.int_val(v) {
                    let f = self.box_float(n as f64);
                    *self.stack.last_mut().unwrap() = f;
                } else if v.is_float() {
                    // already a float — idempotent no-op
                } else {
                    return Err(self.err(
                        format!("expected number, found {}", self.type_name(v)),
                        span,
                    ));
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
            // Cancellation checkpoint — lock-step with the inlined arm in `run_until`.
            Op::Jump(t) => self.jump_checked(*t, span)?,
            Op::JumpIfFalse(t) => {
                if self.pop().as_bool() == Some(false) {
                    self.jump(*t);
                }
            }
            Op::JumpIfFalseKeep(t) => {
                if self.stack.last().unwrap().as_bool() == Some(false) {
                    self.jump(*t);
                }
            }
            Op::JumpIfTrueKeep(t) => {
                if self.stack.last().unwrap().as_bool() == Some(true) {
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
            // M24 static witness: the type key rides on top of the args as a `str`. Pop it, then
            // dispatch exactly like `CallStatic`. The compiler only emits this reading a `$w:T`
            // local it itself initialized from a `ConstStr`, so a non-`str` here is an internal
            // invariant break — surfaced as a clear runtime error, never a panic.
            Op::CallStaticDyn { method, argc } => {
                let w = self.pop();
                let Some(key) = self.val_str(w) else {
                    return Err(self.err(
                        format!("internal: static witness for '{method}' is not a type key"),
                        span,
                    ));
                };
                self.do_static_call(&key, method, *argc, span)?
            }
            Op::CallBuiltin(name, argc) => self.do_builtin(name, *argc, span)?,
            Op::LoadBuiltin(name) => {
                let h = self.heap.alloc(Obj::Builtin(name.as_str().into()));
                self.push(Value::obj(h));
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
                self.push(Value::obj(h));
            }
            Op::NewTuple(n) => {
                let at = self.stack.len() - *n;
                let items: Vec<Value> = self.stack.split_off(at);
                let h = self.heap.alloc(Obj::Tuple(items));
                self.push(Value::obj(h));
            }
            Op::NewMap(n) => {
                // Build an insertion-ordered hash map with last-key-wins upsert. Phase 1 hashes
                // every key while ALL operands are still rooted on the stack (a struct key's hash()
                // re-enters the VM and can GC); phase 2 dedups against the half-built LOCAL map,
                // which since M23 may dispatch a user `eq` (another re-entry) — both phases read
                // keys/values from the still-rooted stack, so the operands survive either collection.
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
                    match self.map_slot(&map.entries, map.candidates(hk), k, span)? {
                        Some(p) => map.entries[p].2 = v,
                        None => map.push(hk, k, v),
                    }
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Map(map));
                self.push(Value::obj(h));
            }
            Op::NewSet(n) => {
                // Insertion-ordered hash set, dedup keeping first occurrence. Same two-phase rooting
                // as NewMap (elements stay rooted on the stack across BOTH the re-entrant `hash`
                // and the dedup's possibly-re-entrant `eq`).
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
                    if self
                        .set_slot(&set.entries, set.candidates(he), e, span)?
                        .is_none()
                    {
                        set.push(he, e);
                    }
                }
                self.stack.truncate(at);
                let h = self.heap.alloc(Obj::Set(set));
                self.push(Value::obj(h));
            }
            Op::NewStruct(name, argc) => self.new_struct(name, *argc, span)?,
            Op::NewType(type_key) => {
                let inner = self.pop();
                let h = self.heap.alloc(Obj::NewType {
                    type_key: type_key.as_str().into(),
                    inner,
                });
                self.push(Value::obj(h));
            }
            // ----- cells (uniform by-reference capture, Task A — unwired) -----
            Op::NewCell => {
                // `v` moves straight into the `Obj` (no rooting window — alloc never GCs mid-op,
                // same profile as `Op::NewType`).
                let v = self.pop();
                let h = self.heap.alloc(Obj::Cell(v));
                self.push(Value::obj(h));
            }
            Op::CellLoad => {
                let ch = self.pop();
                let Some(h) = ch.as_obj() else {
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
                let Some(h) = ch.as_obj() else {
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
                self.push(Value::obj(h));
            }
            // A default-argument provider declared in a module this one does not import — see
            // [`Op::MakeFuncIn`]. `home` comes from the DEFINER's module index, not this frame's, so
            // the default resolves in its own namespace with no import edge.
            //
            // `.get()` rather than `[]`: `module_objs` grows as modules RUN (and a worker's copy holds
            // only the modules that had run when it was snapshotted), so a not-yet-initialized definer
            // must be a clean `RuntimeError`, never an index panic on a pool thread.
            // The module's globals themselves fault in lazily — the provider's own `GetGlobalSlot`
            // calls `ensure_module_faulted` — so nothing is forced here.
            // Callee-side default fill: the argument WAS supplied, so skip the compiled default.
            // Falling through runs it and stores it into the slot.
            Op::JumpIfProvided(slot, target) => {
                if self.frames.last().unwrap().argc > *slot as usize {
                    self.jump(*target);
                }
            }
            Op::MakeFuncIn(id) => {
                let (proto, midx) = self.program.providers[*id as usize];
                let Some(&home) = self.module_objs.get(midx) else {
                    return Err(self.err(
                        format!(
                            "the module that declares {} has not been initialized yet",
                            crate::desugar::display_fn_name(&self.program.protos[proto].name)
                        ),
                        span,
                    ));
                };
                let h = self.heap.alloc(Obj::Func { proto, home });
                self.push(Value::obj(h));
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
                            .unwrap_or(Value::nil()),
                    };
                    captured.push(v);
                }
                let h = self.heap.alloc(Obj::Closure {
                    proto: *proto,
                    captured,
                    home,
                });
                self.push(Value::obj(h));
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
                self.push(Value::obj(h));
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
                self.push(Value::obj(h));
            }
            Op::ListClone => {
                // Normalise a `for` iterand to an index-iterable list: a list is cloned (so a body
                // that mutates it doesn't disturb iteration); a map yields its keys (gap #14).
                let v = self.pop();
                match v.view() {
                    ValueView::Obj(h) => match self.heap.get(h) {
                        Obj::List(items) => {
                            let cloned = items.clone();
                            let nh = self.heap.alloc(Obj::List(cloned));
                            self.push(Value::obj(nh));
                        }
                        Obj::Map(m) => {
                            let keys: Vec<Value> = m.entries.iter().map(|(_, k, _)| *k).collect();
                            let nh = self.heap.alloc(Obj::List(keys));
                            self.push(Value::obj(nh));
                        }
                        Obj::Set(s) => {
                            let elems: Vec<Value> = s.entries.iter().map(|(_, e)| *e).collect();
                            let nh = self.heap.alloc(Obj::List(elems));
                            self.push(Value::obj(nh));
                        }
                        // A string iterates as 1-char strings (Python-style; gap: char type).
                        Obj::Str(s) => {
                            // Collect `char`s (Copy — no per-char `String`) to release the heap borrow,
                            // then box each in one alloc via `alloc_char`.
                            let chars: Vec<char> = s.chars().collect();
                            let items: Vec<Value> =
                                chars.into_iter().map(|c| self.alloc_char(c)).collect();
                            let nh = self.heap.alloc(Obj::List(items));
                            self.push(Value::obj(nh));
                        }
                        // `bytes`/`bytearray` iterate as `int`s (0–255). Snapshots to a list of ints —
                        // mutating the `bytearray` during iteration does not change the loop sequence.
                        Obj::Bytes(b) => {
                            let items: Vec<Value> =
                                b.iter().map(|&x| Value::int(x as i64)).collect();
                            let nh = self.heap.alloc(Obj::List(items));
                            self.push(Value::obj(nh));
                        }
                        Obj::ByteArray(b) => {
                            let items: Vec<Value> =
                                b.iter().map(|&x| Value::int(x as i64)).collect();
                            let nh = self.heap.alloc(Obj::List(items));
                            self.push(Value::obj(nh));
                        }
                        // A cursor (`for x in pure_iterable_struct` lowered via `IterableToCursor`)
                        // snapshots its REMAINING items to the index-iterable list.
                        Obj::Iter { items, pos } => {
                            let cloned = items[(*pos).min(items.len())..].to_vec();
                            let nh = self.heap.alloc(Obj::List(cloned));
                            self.push(Value::obj(nh));
                        }
                        _ => {
                            return Err(self
                                .err(format!("cannot iterate over {}", self.type_name(v)), span));
                        }
                    },
                    _ => {
                        return Err(
                            self.err(format!("cannot iterate over {}", self.type_name(v)), span)
                        );
                    }
                }
            }
            Op::ArrLen => {
                let v = self.pop();
                let len = match v.view() {
                    ValueView::Obj(h) => match self.heap.get(h) {
                        Obj::List(items) => items.len() as i64,
                        _ => unreachable!("ArrLen on non-list"),
                    },
                    _ => unreachable!("ArrLen on non-list"),
                };
                self.push(Value::int(len));
            }
            Op::IsStruct => {
                let v = self.pop();
                let is_struct =
                    matches!(v.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Struct { .. }));
                self.push(Value::bool(is_struct));
            }
            Op::IsGenerator => {
                let v = self.pop();
                let is_gen =
                    matches!(v.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Generator(_)));
                self.push(Value::bool(is_gen));
            }
            Op::IsCursor => {
                let v = self.pop();
                let is_cursor =
                    matches!(v.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Iter { .. }));
                self.push(Value::bool(is_cursor));
            }
            Op::IterableToCursor => {
                // One-time `for`-entry conversion: a PURE-`Iterable` struct (has `iter`, lacks `next`)
                // becomes its cursor (so the seq path drains it); everything else passes through.
                // Shared with `drain_iterable` (`List()`/`Set()`/`Map()`/`.iter()`) so every consumer
                // of the checker's `Iterable` admission set accepts the same witnesses.
                let v = self.pop();
                let cursor = self.iterable_to_cursor(v, span)?;
                self.push(cursor);
            }
            Op::Yield => {
                // Experimental generator suspend. The yielded value is already on the stack top; flag
                // the request and let `run_until` return to the host `.next()` after this op (the
                // frame `ip` has already advanced past the `Yield`, so resume continues after it).
                self.gen_yielding = true;
            }
            Op::IsMap => {
                let v = self.pop();
                let is_map =
                    matches!(v.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Map(_)));
                self.push(Value::bool(is_map));
            }
            Op::IsChannel => {
                let v = self.pop();
                let is_chan =
                    matches!(v.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Channel(_)));
                self.push(Value::bool(is_chan));
            }
            Op::ChanRecvOrClosed => {
                // `for v in ch:` step: pop a value (parking on empty-open exactly like `recv`) and push
                // `Some(v)`, or push `None` once the channel is closed-and-drained (the loop's clean
                // exit). Runs at the loop top, never inside a native callback (`native_reentry == 0`),
                // so it takes the snapshot-park / block-in-place / fault paths — never the demote path.
                let v = self.pop();
                let Some(h) = v.as_obj() else {
                    return Err(self.err("`for` over a non-channel value".to_string(), span));
                };
                match self.chan_recv_step(h, span)? {
                    RecvStep::Got(w) => {
                        self.wake_senders(h); // `for v in ch:` freed a slot — wake a parked bounded sender
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
                if !matches!(v.as_obj(), Some(h) if matches!(self.heap.get(h), Obj::Enum { .. })) {
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
                let variant = match v.view() {
                    ValueView::Obj(h) => match self.heap.get(h) {
                        Obj::Enum { variant_id, .. } => self.enum_names(*variant_id).1.to_string(),
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                return Err(self.err(format!("no match arm for variant '{variant}'"), span));
            }
            Op::EnterNursery => self.op_enter_nursery(),
            Op::JoinNursery => self.join_nursery()?,
            // TASK B — `break`/`continue` leaving a `parallel:` scope: cancel its
            // tasks and pop exactly that one level (the compiler emits one per escaped scope).
            Op::ReclaimNursery => {
                let from = self.nurseries.len().saturating_sub(1);
                self.drain_escaped_nursery(from);
            }
            Op::SpawnCall(argc) => self.do_spawn(None, *argc, span)?,
            Op::SpawnMethod(name, argc) => self.do_spawn(Some(name.clone()), *argc, span)?,
            Op::SpawnBlock(proto, entries) => self.do_spawn_block(*proto, entries, span)?,
            Op::WaitPoll(meta) => self.op_wait_poll(meta, span)?,
            Op::NewChannel(has_cap) => {
                let cap = if *has_cap {
                    let cap_v = self.pop();
                    let n = self.int_of(cap_v);
                    if n <= 0 {
                        return Err(self.err(
                            "Channel capacity must be > 0 (use Channel[T]() for an unbounded channel)"
                                .to_string(),
                            span,
                        ));
                    }
                    Some(n as usize)
                } else {
                    None
                };
                let h = self.heap.alloc(Obj::Channel(Arc::new(ChannelCore {
                    cap,
                    ..Default::default()
                })));
                self.push(Value::obj(h));
            }
            Op::NewShared => {
                let init = self.pop();
                // The box holds the wire form (single serialization == the old deep_clone-in). A
                // non-sendable init (a frame-holding generator / module/native/FFI handle) faults
                // gracefully with this Op's span — the box is a shared cross-thread cell.
                let init = self.to_wire_crossable(init, span)?;
                let h = self.heap.alloc(Obj::Shared(Arc::new(SharedCore {
                    v: Mutex::new(init),
                    ..Default::default()
                })));
                self.push(Value::obj(h));
            }
            Op::NewRwShared => {
                let init = self.pop();
                // The box holds the wire form (single serialization == the old deep_clone-in). A
                // non-sendable init (a frame-holding generator / module/native/FFI handle) faults
                // gracefully with this Op's span — the box is a shared cross-thread cell.
                let init = self.to_wire_crossable(init, span)?;
                let h = self.heap.alloc(Obj::RwShared(Arc::new(RwSharedCore {
                    v: RwLock::new(init),
                    ..Default::default()
                })));
                self.push(Value::obj(h));
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
            Op::NewAtomicInt => {
                let v = self.new_atomic_int(span)?;
                self.push(v);
            }
            Op::NewTimer => {
                let v = self.new_timer(span)?;
                self.push(v);
            }
            Op::NewExecutor => {
                let core = Arc::new(ExecutorCore {
                    // W7-39 — an executor created INSIDE a running eager job inherits that job's
                    // cancel chain, so an outer `shutdown_now()` reaches the jobs this executor will
                    // dispatch. Captured HERE, at creation, not at `submit`: the handle crosses the
                    // airlock by `Arc`, so the submitter can belong to an unrelated executor.
                    creator_cancel: match self.eager_core.is_some() {
                        true => self.scope_ancestors(),
                        false => Vec::new(),
                    },
                    ..Default::default()
                });
                // Heap-independent registration for the program-exit join (W7-5b) — this is the one
                // that survives its creating task/heap. Creation order across the whole run.
                self.exec_registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(Arc::clone(&core));
                let h = self.heap.alloc(Obj::Executor(core));
                // The handle is also a GC root here, so the executor's queued work survives even
                // after every in-program handle is gone. `self.executors` is no longer itself walked
                // to drain anything — that was the deleted `--serial` engine's own reap mechanism
                // (draining through the handle, re-rooted on the operand stack); today it exists
                // purely as this heap's GC root, and the program-exit join walks `exec_registry`
                // (registered just above) instead.
                self.executors.push(h);
                self.push(Value::obj(h));
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
        vm.push(Value::int(7));
        vm.step(&Op::NewCell, span).unwrap();
        vm.step(&Op::CellLoad, span).unwrap();
        assert_eq!(vm.pop(), Value::int(7));
    }

    /// HARD contract: operands are pushed value-THEN-handle (stack `[val, handle]`), so `CellStore`
    /// pops the handle FIRST, then the value, and writes the value into the cell in place.
    #[test]
    fn cellstore_pops_handle_first() {
        let mut vm = new_vm();
        let span = Span::default();
        vm.push(Value::int(7));
        vm.step(&Op::NewCell, span).unwrap();
        let h = vm.pop();
        // Push value THEN handle: [Int(9), handle].
        vm.push(Value::int(9));
        vm.push(h);
        vm.step(&Op::CellStore, span).unwrap();
        // Reload through the same handle → observes the stored value.
        vm.push(h);
        vm.step(&Op::CellLoad, span).unwrap();
        assert_eq!(vm.pop(), Value::int(9));
    }
}
