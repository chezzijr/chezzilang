// vm::exec — split out of vm/mod.rs. `super::*` == the `vm` module.
// VM core: construction, frames, generators, run/run_until/step dispatch.

use super::block::Cut;
use super::*;

/// A plain runtime fault: no assert, cap, timeout, deadlock or panic flag. Behind [`Vm::err`] and
/// the copy walk (`WalkSrc`), which has no `Vm`.
pub(super) fn plain_err(message: String, span: Span) -> RuntimeError {
    RuntimeError {
        message,
        span,
        is_assert: false,
        is_over_memory: false,
        is_timed_out: false,
        is_deadlock: false,
        is_panic: false,
    }
}

/// TICKET-286 — the one `--timeout` hard halt: its text and its `timed_out` marker. The Vm rungs
/// ([`Vm::deadline_halt`], the back-edge sample, the socket resume) and the sched-side offloaded
/// sleep (`arm_timer_sleep`) build it here.
pub(super) fn timed_out_err(timeout_ms: u64, span: Span) -> RuntimeError {
    plain_err(format!("test exceeded --timeout ({timeout_ms}ms)"), span).timed_out()
}

/// Behind [`Vm::depth_exceeded_err`]; the one copy of the message.
pub(super) fn depth_exceeded(span: Span) -> RuntimeError {
    plain_err(
        format!(
            "maximum structural depth ({MAX_STRUCTURAL_DEPTH}) exceeded (cyclic data structure?)"
        ),
        span,
    )
}

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
            is_deadlock: false,
            is_panic: false,
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

    /// A root `Vm`: it makes its run's heap core. Every other `Vm` of the run is built by
    /// [`Vm::spawn_worker`], on a handle of that core.
    pub(super) fn new(program: Arc<Program>) -> Self {
        Vm::with_heap(program, Heap::new())
    }

    /// A `Vm` over the heap handle `heap`.
    pub(super) fn with_heap(program: Arc<Program>, heap: Heap) -> Self {
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
            heap,
            stack: Vec::new(),
            frames: Vec::new(),
            out: Vec::new(),
            stderr: Vec::new(),
            host: crate::native::HostConfig::default(),
            call_depth: 0,
            module_objs: Vec::new(),
            str_intern: fxhash::FxHashMap::default(),
            fn_values: fxhash::FxHashMap::default(),
            field_ic,
            method_ic,
            quicken,
            quicken_base,
            cur_base: 0,
            handlers: Vec::new(),
            pending_exit: None,
            exit_origin: false,
            box_chain: Vec::new(),
            box_lifted: Vec::new(),
            box_view_err: None,
            fault_trace: None,
            fault_trace_depth: 0,
            gc_stress: gc_door::gc_stress_env(),
            nurseries: Vec::new(),
            mn_scopes: Vec::new(),
            mn_enlisted: 0,
            mn_enlist_sched: None,
            eager_scheds: Vec::new(),
            fiber_scope: None,
            nursery_defer_floors: Vec::new(),
            nursery_spans: Vec::new(),
            executors: Vec::new(),
            exec_registry: Arc::new(Mutex::new(Vec::new())),
            sched_registry: Arc::new(Mutex::new(Vec::new())), // W7-56
            root_registry: RootRegistry::default(),           // M1.4
            gc_coord: Arc::new(gc_coord::GcCoord::new()),     // M1.5
            attached: None,
            detached: None,
            suspend: None,
            join_suspend: None,
            wait_suspend: None,
            send_suspend: None,
            park_site: None,
            pending: None,
            fault_park: None,
            offload: None,
            poll_park: None,
            pending_connect: None,
            wire_backref_missing: false, // W7-11
            copy_mark: false,
            poll_timed_out: false,
            poll_deadline: None,
            poll_partial: None,
            poll_written: None,
            native_reentry: 0,
            guard_token: {
                static GUARD_TOKENS: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                GUARD_TOKENS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1
            },
            walk_base: 0,
            eq_hook_off: false,
            stdout_writes: 0,
            reds: 0,             // D3 — set to CONTEXT_REDS per schedule-in (run_one_fiber)
            yield_now: false,    // D3
            gen_yielding: false, // experimental generators
            gen_host_ctx: Vec::new(),
            active_generators: Vec::new(),
            gen_fault_prefix: Vec::new(),
            repr_active: Vec::new(),
            wid: 0,         // D5 owe #3 (Path C) — set in mn_worker_loop
            demoted: false, // D5 owe #3 (Path C)
            slice_in_place: false,
            run_permit: None,
            cancel: None,
            cancel_outer: Vec::new(),
            cut: None,
            cancel_unwind_faulted: false,
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
            snapshot_nodes: std::sync::Arc::new(super::fxhash::FxHashMap::default()),
            snapshot_adopt: super::fxhash::FxHashMap::default(),
            adopt_active: false,
            snapshot_next_id: 0,
            snapshot_builds: 0,
            mn: None,
        }
    }

    pub(super) fn err(&self, message: String, span: Span) -> RuntimeError {
        plain_err(message, span)
    }

    /// The shared recoverable fault raised by every structural walker when recursion exceeds
    /// [`MAX_STRUCTURAL_DEPTH`] (cyclic data). Single source of truth for the message so it stays in
    /// sync with the constant; callers pass their own span.
    pub(super) fn depth_exceeded_err(&self, span: Span) -> RuntimeError {
        depth_exceeded(span)
    }

    /// B3.4 — set this VM's nursery cancel flag (if it runs under one), so sibling workers abort.
    /// No-op when this VM runs under no nursery (`cancel` is `None`) — e.g. the top-level VM.
    pub(super) fn trip_cancel(&self) {
        if let Some(c) = &self.cancel {
            crate::vm::trip_cancel_flag(c);
        }
    }

    /// M1.6 — a task switch: [`Vm::swap_ctx`] plus the task's output stream. A task owns its
    /// `out` / `stderr` (Decision F), so only a task switch moves them; a box call runs in its
    /// caller's stream and swaps with `swap_ctx` alone.
    ///
    /// M1.7 (brief item 4) — the running generators, their parked hosts and the open box calls
    /// are `Vm` fields that `FiberCtx` does not carry, so a task switch asserts they are empty: a
    /// generator body and a box closure run under [`Vm::reentered`], where a blocking op demotes
    /// instead of parking, so no fiber parks with any of them open. `Vm::gc_roots` roots them
    /// while they are open.
    pub(super) fn swap_task(&mut self, ctx: &mut FiberCtx) {
        assert!(
            self.gen_host_ctx.is_empty()
                && self.active_generators.is_empty()
                && self.box_chain.is_empty()
                && self.box_lifted.is_empty(),
            "a task switch with a generator or a box call open"
        );
        self.swap_ctx(ctx);
        std::mem::swap(&mut self.out, &mut ctx.out);
        std::mem::swap(&mut self.stderr, &mut ctx.stderr);
    }

    /// Swap the live per-execution `Vm` fields with `ctx` (B1). Used by the nursery scheduler to
    /// schedule a fiber in (its saved context becomes live) or out (the running context is parked
    /// back into the fiber), and by a box call. The fields [`FiberCtx`] holds, except the output
    /// stream ([`Vm::swap_task`]) and `start` — `pending_exit` stays global.
    pub(super) fn swap_ctx(&mut self, ctx: &mut FiberCtx) {
        // M1.7 — the handle's `get_mut` record names an object of the context that leaves: settle
        // it now. The context can resume on another shell (or a box on another caller's thread)
        // and write that object while this handle would read it.
        self.heap.settle_pending_mut();
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
        std::mem::swap(&mut self.nursery_spans, &mut ctx.nursery_spans);
        std::mem::swap(&mut self.eager_scheds, &mut ctx.eager_scheds);
        std::mem::swap(&mut self.fault_trace, &mut ctx.fault_trace);
        std::mem::swap(&mut self.fault_trace_depth, &mut ctx.fault_trace_depth);
        // stdin is NOT swapped: it is ONE source every task shares (Go/Python) — see `Stdin` and
        // `spawn_worker`, which hands the M:N worker the same shared handle.
        //
        // Task 1 — the fiber's module-globals view swaps for EVERY fiber: those `GcRef`s are the
        // fiber's own copies (in the run's one heap), so they travel WITH the fiber. The parent's
        // REAL modules move into `ctx.module_objs` on its swap-out. W6-2 — `module_snapshot` + `snapshot_memo` swap WITH them: a snapshot describes a
        // module VIEW, and one shell drains fibers holding DIFFERENT views, so each must fault in (and
        // re-snapshot) from its OWN. They carry no `GcRef` (heap-independent `SnapValue`), so GC
        // rooting is unaffected.
        std::mem::swap(&mut self.module_objs, &mut ctx.module_objs);
        std::mem::swap(&mut self.module_faulted, &mut ctx.module_faulted);
        std::mem::swap(&mut self.module_snapshot, &mut ctx.module_snapshot);
        std::mem::swap(&mut self.snapshot_memo, &mut ctx.snapshot_memo);
        // W7-4a — the snapshot rebuild map describes the SAME view, so it travels with it. Unlike the
        // two `Arc<ModuleSnapshot>`s above it holds `GcRef`s, exactly like `module_objs` just
        // above, and it moves atomically with its view.
        std::mem::swap(&mut self.snapshot_rebuild, &mut ctx.snapshot_rebuild);
        // W7-4c — the snapshot cell registry is heap-keyed too (its KEYS are `GcRef`s into the heap the
        // snapshot was built from), so it travels with the same view for the same reason.
        std::mem::swap(&mut self.snapshot_cells, &mut ctx.snapshot_cells);
        // TICKET-111 — the snapshot node registry is heap-keyed too, for the same reason.
        std::mem::swap(&mut self.snapshot_nodes, &mut ctx.snapshot_nodes);
        // TICKET-111 — pending adoption captures are heap-keyed too, for the same reason.
        std::mem::swap(&mut self.snapshot_adopt, &mut ctx.snapshot_adopt);
        // W7-4c — the counter travels WITH the registry it numbers; see `FiberCtx::snapshot_cells`.
        // Split them and a fiber resuming on a fresher shell re-mints ids its own registry already
        // uses, merging two unrelated bindings.
        std::mem::swap(&mut self.snapshot_next_id, &mut ctx.snapshot_next_id);
        // TICKET-213 / M1.7 — the view state describes the module view above, so it travels with
        // it: a context's snapshot stamp names its own view, never the heap (which every context of
        // the run shares). GC: a swapped-OUT `module_objs` needs no pin — `ctx` is registered with
        // the run's `RootRegistry`, which the collector walks.
        std::mem::swap(&mut self.heap.view, &mut ctx.view);
        // D2b — a context that owns its side state swaps it here: each field below belongs to ONE
        // fiber (or one box context), so a shell that runs many in turn must not leave it behind
        // for the next. A context that does not own side state aliases the shell's.
        if ctx.owns_side {
            // D2b — the fiber's `Executor` handles: the GC roots of the executors it made.
            std::mem::swap(&mut self.executors, &mut ctx.executors);
            // M19 Phase 3 — the fiber's intern cache. Its strings are this context's objects: a
            // cache left on the shell would hand the next fiber an object it may then write
            // (every crossing copies until M3, so no two fibers share one).
            std::mem::swap(&mut self.str_intern, &mut ctx.str_intern);
            // TICKET-215 — the fn-value memo; this context's objects, as above.
            std::mem::swap(&mut self.fn_values, &mut ctx.fn_values);
            // D6b — a mid-flight `connect` parked on writability swaps WITH its fiber (it owns the
            // connecting fd that the netpoller is watching; it must not be left on the shell where the
            // next fiber would inherit or drop it).
            std::mem::swap(&mut self.pending_connect, &mut ctx.pending_connect);
            // D6c — a socket timeout marker set by the poll thread (on the detached fiber's ctx) swaps
            // in here so the resumed socket op sees it at entry. M:N-only, like `pending_connect`.
            std::mem::swap(&mut self.poll_timed_out, ctx.poll_timed_out.get_mut());
            // B1 — the in-flight `read`'s latched deadline belongs to the parked fiber's op (which
            // re-executes on wake), so it swaps with the fiber exactly like `poll_timed_out`.
            std::mem::swap(&mut self.poll_deadline, &mut ctx.poll_deadline);
            // N3(a) — the taken-partial flag is set BEFORE the park and consulted at the re-entry, so
            // it must travel with the fiber exactly like `poll_deadline`.
            std::mem::swap(&mut self.poll_partial, &mut ctx.poll_partial);
            std::mem::swap(&mut self.poll_written, &mut ctx.poll_written);
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
            || self.join_suspend.is_some()
            || self.wait_suspend.is_some()
            || self.send_suspend.is_some()
            || self.yield_now
            || self.offload.is_some()
            || self.poll_park.is_some()
    }

    /// Run `f` with the native-reentry guard raised (B1). A blocking `recv` reached while the guard
    /// is up cannot park (its caller's loop/recursion state lives on the Rust stack, not in a
    /// [`Fiber`]), so it faults `deadlock` instead of suspending.
    ///
    /// `guarded` is the back-edge of a native LOOP: one call per element (list HOFs, the sort-key
    /// and merge-comparator loops, `Shared`/`RwShared` each/fold, the struct-iterator collect), so it
    /// runs [`Vm::guarded_checkpoint`] before [`Vm::reentered`]. A native that calls user code once
    /// uses `reentered` (TICKET-194).
    pub(super) fn guarded<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        self.guarded_checkpoint()?;
        self.reentered(f)
    }

    /// TICKET-194 — THE re-entry bracket: raises `native_reentry` around `f`, with no cancellation
    /// checkpoint. A native that calls user code ONCE (an operator or protocol hook, a generator
    /// resume, `update`/`read`/`write`) enters it here: that call does not wait, so it is not a
    /// cancellation point (owner decision 1, `docs/history/bug-hunts/w18.md`). The callee's own back-edges
    /// and waits still cut it. A run-wide halt still lands at the hook's frame push
    /// ([`Vm::call_halt`], TICKET-224).
    pub(super) fn reentered<T>(
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
            // A panic out of a bracketed wait left the permit released, and an FFI trampoline above
            // may resume Chezzi code; a no-op when ungated or already holding.
            #[rustfmt::skip]
            Err(payload) => { self.width_acquire(); std::panic::resume_unwind(payload) }
        }
    }

    /// [`Vm::reentered`], plus: run `f` with [`Vm::walk_base`] set to `base`, the structural depth the
    /// ENCLOSING native walk had already consumed. Every FAULTING `MAX_STRUCTURAL_DEPTH` guard tests
    /// `walk_base + depth` (the two DEGRADING ones — the map/set key-store pair — deliberately do
    /// not; see [`MAX_STRUCTURAL_DEPTH`]), so a chain of nested `eq`/`str` hooks shares ONE 10 000 allowance instead
    /// of each re-entry restarting at 0 — without this, hook-nesting depth × per-hook walk depth is
    /// unbounded and the process dies by host stack overflow (uncatchable, rc=134).
    ///
    /// The `catch_unwind` is load-bearing, not decoration: `reentered` catches the unwind, decrements
    /// `native_reentry`, and **resumes the unwind from inside itself**, so any `self.walk_base =
    /// saved` written AFTER the `reentered(...)` call would be skipped on a panic. Panics really do
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
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.reentered(f)));
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
    ///
    /// TICKET-155 — the 1-in-1024 sample is now SHARED with the owner rung (`MnSched::scope_fault`
    /// takes the sched lock, so that rung cannot run per element either). The caller ticks whenever
    /// `--timeout` is set OR a nursery is open on this fiber, so the `deadline.is_some()` test lives
    /// here now. Returns whether this call landed on the sample.
    #[cold]
    #[inline(never)]
    fn hof_sampled_tick(&mut self) -> Result<bool, RuntimeError> {
        self.back_edge_tick = self.back_edge_tick.wrapping_add(1);
        if !self.back_edge_tick.is_multiple_of(1024) {
            return Ok(false);
        }
        if self.deadline.is_some()
            && let Some(fr) = self.frames.last()
        {
            let span = fr.call_span;
            self.deadline_halt(span)?;
        }
        Ok(true)
    }

    /// The three halt rungs [`Vm::guarded`] runs before re-entering user code — `--timeout`, then
    /// cancel, then a run-wide `os.exit`, the same order `jump_checked` uses.
    ///
    /// Non-generic and out of line: `guarded` is generic over its callback, so its body is
    /// monomorphized at every call site and these rungs would be duplicated into all of them.
    #[inline(never)]
    fn guarded_checkpoint(&mut self) -> Result<(), RuntimeError> {
        // CANCELLATION CHECKPOINT — a native LOOP that re-enters user code per element drives it
        // from a RUST loop (`list.map`/`filter`/`fold`, `sort`'s comparator, an `Executor` handler:
        // `for e in .. { self.guarded(|vm| vm.invoke_value(f, ..))? }`, call.rs). A single-call
        // re-entry (an operator or protocol hook, a generator resume, `update`) uses `reentered`
        // and is not a checkpoint (TICKET-194). That Rust
        // loop emits no `Op::Jump`, so `jump_checked`'s back-edge never fires inside it and a
        // straight-line callback body has no back-edge of its own — a cancelled task would burn every
        // remaining element (with its prints / `Shared` writes / fs writes) to completion. The
        // per-element re-entry IS this loop's back-edge, so it is where the cancel is delivered; the
        // `?` on `guarded` aborts the native loop, exactly as the old every-instruction check did via
        // the callback's nested `run_until`. A DEFERRED call also runs through `guarded`
        // (`run_one_deferred`) — `halt_requested` is `None` while `deferring > 0`, so the defer body
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
        // live in the out-of-line [`Vm::hof_sampled_tick`] (see it — the placement is measured, not
        // stylistic) and are throttled 1/1024 on the shared `back_edge_tick`. The `deadline.is_some()`
        // gate is checked BEFORE the call, so a run with the cap off — the common case, and every
        // `chezzi run` — neither ticks nor reads the clock, same as `jump_checked`. `deadline_halt`
        // produces the `.timed_out()`-marked error the runner reports as `TIMED-OUT`, rather than
        // re-deriving the message here.
        //
        // TICKET-155: the tick also runs while a nursery is open on this fiber (`eager_scheds` is
        // non-empty), for the width yield at the bottom. (The owner-fault rung that first rode this
        // sample is now the `take_halt` below, TICKET-188.) A program with no `parallel:` and no cap
        // still neither ticks nor takes a lock.
        let sampled = if self.deadline.is_some() || self.owns_nursery() {
            self.hof_sampled_tick()?
        } else {
            false
        };
        // TICKET-188 — a cancel, or a child fault of a nursery this party owns: one predicate. The
        // owned half rides the 1-in-1024 sample (see `halt_read`); the cancel half runs every time.
        if let Some(fr) = self.frames.last() {
            let span = fr.call_span;
            if let Some(e) = self.take_halt_at(span, sampled) {
                return Err(e);
            }
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
        // [`Vm::exit_halt`], not `run_exit_err`: it sends a fiber that holds a cancel flag down the
        // `Cancelled` path. An exit is delivered inside a `defer` body too (TICKET-213); a job
        // fault is not.
        if self.quiesce.run_halt_hint()
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
            stamp: 0,
        };
        Value::obj(self.heap.alloc(Obj::Generator(Box::new(core))))
    }

    /// Resume a generator until its next `yield` (→ `Some(v)`) or until its body ends (→ `None`,
    /// state `Done`). Driven intrinsically by `.next()` (see `do_method_call`). The generator runs in
    /// its own private base-0 context swapped into the live `Vm`; the host context is parked in
    /// `gen_host_ctx` (GC-rooted) for the duration. Runs `reentered`, so a would-be blocking op inside a
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
        // TICKET-137 — a module-global generator the task's snapshot could not copy: fault with the
        // recorded cause. Before the state take below, whose `Done` short-circuit would read it as
        // exhausted.
        if let Obj::Generator(g) = self.heap.get(h)
            && let GenState::Unsendable(msg) = &g.state
        {
            let msg = msg.to_string();
            return Err(self.err(msg, span));
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
        // A resumed body frame reports THIS resume site as "called at", not the first `.next()` that
        // ever drove it (W14-35c; CPython names the faulting `next(g)` line). A suspended generator
        // holds exactly one frame — a `yield` cannot sit in a nested call.
        if !matches!(state, GenState::Pending(_))
            && let Some(f) = self.frames.first_mut()
        {
            f.call_span = span;
        }
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
        // W13-20 — give this resume its OWN fault latch. `fault_trace_depth` is a frame COUNT, and
        // the generator's private `frames` (just swapped in above) is a different stack than the
        // driver's, so comparing a generator-side count against the restored driver frame count
        // compares two different stacks. Park the host's latch and start the generator on an empty
        // one; on `Err` the generator's own capture becomes a PREFIX the driver-side capture
        // prepends, instead of a value the merge guard compares against the wrong stack.
        let host_trace = self.fault_trace.take();
        let host_trace_depth = std::mem::replace(&mut self.fault_trace_depth, 0);
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

        // Run to the next suspension / end (re-entered: no parking inside a generator).
        self.gen_yielding = false;
        let run = push_res.and_then(|()| self.reentered(|s| s.run_until(0)));
        let yielded = self.gen_yielding;
        self.gen_yielding = false;

        // Pull the generator's now-updated context back out and restore the host.
        let mut new_ctx = GenCtx::default();
        self.swap_gen_ctx(&mut new_ctx); // new_ctx = generator's current context; self.* now empty
        let mut host = self.gen_host_ctx.pop().expect("generator host context");
        self.swap_gen_ctx(&mut host); // self.* = host restored
        self.active_generators.pop();
        let _ = first_call;

        // Pull the generator's own capture back out and restore the host's latch. On `Err` the
        // generator's frames become the prefix the driver-side capture (in the dispatch loop's
        // error arm) prepends to.
        let gen_trace = self.fault_trace.take();
        self.fault_trace = host_trace;
        self.fault_trace_depth = host_trace_depth;
        if run.is_err() {
            self.gen_fault_prefix = gen_trace.unwrap_or_default();
        }

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
        let unhandled = *variant_id == crate::base::op::VID_ERR
            || *variant_id == crate::base::op::VID_NONE_VARIANT;
        if !unhandled {
            return None;
        }
        let detail = match payload.try_get(0) {
            Some(p) => self.display(p),
            None => self.display(v),
        };
        Some(self.err(format!("unhandled error: {detail}"), span))
    }

    // ----- top-level drivers -----

    /// Run every module in dependency order, then the entry's `main()`.
    pub(super) fn run(&mut self) -> Result<(), RuntimeError> {
        // TICKET-230 — the run's main thread is one of the N runners. The guard keeps a second
        // `run()` on this `Vm` from dropping the first permit and running with none.
        #[cfg(test)]
        width::set_probe(Some(Arc::clone(&self.quiesce.width_probe)));
        if self.run_permit.is_none() {
            self.run_permit = Some(self.gc_blocking(&[], |_| width::RunPermit::take()));
        }
        // M1.5 — this thread runs the program: it joins the run's heap until the `Vm` drops.
        self.gc_attach();
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
        self.run_invoke(proto, Vec::new())?;
        Ok(())
    }

    fn run_invoke(&mut self, proto: ProtoId, args: Vec<Value>) -> Result<Value, RuntimeError> {
        self.reset_for_invoke();
        let floor = self.nurseries.len();
        let home = self.entry_home();
        let r = self.run_proto(proto, home, None, args, true, false, Span::RUNTIME);
        let _ = self.drain_escaped_nursery(floor);
        r
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
        self.fault_trace = None;
        self.fault_trace_depth = 0;
        self.gen_fault_prefix.clear();
        // TICKET-195 — a delivered cut belongs to the invoke it unwound; a cancel stays (DEC-010).
        if matches!(self.cut, Some(Cut::Delivered { .. } | Cut::RunFault)) {
            self.cut = None;
        }
    }

    /// W13-21 — the declaration span of the entry callee, if it has one. `Some` for a callee backed
    /// by a real `FnDecl` (`Obj::Func`/`Obj::Closure`, both carry a `ProtoId`); `None` for anything
    /// else (`Obj::Native`, `Obj::Cffi`, or the callee not being found at all), which keeps the old
    /// `Span::RUNTIME` fallback — a closure or native has no declaration to borrow a coordinate from.
    fn entry_decl_span(&self, callee: Value) -> Option<Span> {
        let h = callee.as_obj()?;
        match self.heap.get(h) {
            Obj::Func { proto, .. } | Obj::Closure { proto, .. } => {
                Some(self.program.protos[*proto].decl_span)
            }
            Obj::Str(_)
            | Obj::Bytes(_)
            | Obj::ByteArray(_)
            | Obj::BigInt(_)
            | Obj::FloatBox(_)
            | Obj::Iter { .. }
            | Obj::List(_)
            | Obj::Tuple(_)
            | Obj::Map(_)
            | Obj::Set(_)
            | Obj::Struct { .. }
            | Obj::Enum { .. }
            | Obj::Cell(_)
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Channel(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => None,
        }
    }

    /// Bare `chezzi run` with a `module:function` manifest entrypoint — invoke a named top-level
    /// function of the entry module after `run()` has initialized all modules. Looks the name up in
    /// the entry module's namespace (so a re-exported import works too) and calls it with no args.
    /// A missing name (or a non-callable binding) is a clear runtime error rather than a silent no-op.
    pub fn invoke_entrypoint(&mut self, fn_name: &str) -> Result<(), RuntimeError> {
        let span = Span::RUNTIME;
        let home = self.entry_home();
        // Read the binding by name from the entry module's slot table (mirrors `module_define`).
        let idx = self.slot_index(home, fn_name).ok_or_else(|| {
            self.err(
                format!(
                    "entrypoint function `{fn_name}` not found in module `{}`",
                    self.module_name(home)
                ),
                span,
            )
        })?;
        let callee = self.read_slot(home, idx, span)?;
        // Guard with a clear message before `invoke_value`'s generic "not callable" fault.
        let callable = self.callable(callee).is_some();
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
        // W13-21 — the entry call's real coordinate, when the callee has a declaration to give one.
        // No file-id guard on the read: `invoke_entrypoint`'s one caller (`run_file_inner`) always
        // compiles a module GRAPH, and every module in a graph carries a distinct non-zero
        // `Span::file` (`resolver::tests::every_module_gets_a_distinct_nonzero_span_file_id`), so
        // `entry_decl_span` returns either a true lexed coordinate or `None` — never a coordinate
        // with an unresolvable file id. `render_span` already omits the path for a file id it can't
        // resolve, so this invents nothing (DEC-048).
        let entry_span = self.entry_decl_span(callee).unwrap_or(span);
        // Symmetric with the unhandled-top-level-Err rule: if the entry fn returns `Err(..)`/`None`,
        // surface it as "unhandled error: <detail>" (rc=1) rather than silently discarding it. This
        // lets a manifest entrypoint legitimately be `-> T!` and use `?`.
        let ret = self.invoke_value(callee, Vec::new(), entry_span)?;
        if let Some(e) = self.top_level_error(ret, entry_span) {
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
        self.run_invoke(proto, vec![recv])
    }

    /// `chezzi test` — construct a suite instance via its synthetic zero-arg `__new_<Suite>` thunk.
    pub fn build_suite_instance(&mut self, new_thunk: ProtoId) -> Result<Value, RuntimeError> {
        self.run_invoke(new_thunk, Vec::new())
    }

    /// `chezzi test` — initialize all modules (run top-levels once) so globals/functions exist before
    /// tests are invoked. A thin public wrapper over `run` for the runner.
    pub fn init_for_tests(&mut self) -> Result<(), RuntimeError> {
        self.run()
    }

    /// `chezzi test` — construct a fresh VM over a compiled program (the runner owns the lifecycle).
    /// The program reads the real process env, as under `chezzi run`; args stay empty and stdin is
    /// at EOF, so a test never blocks on the terminal.
    pub fn for_program(program: Arc<Program>) -> Self {
        let mut vm = Vm::new(program);
        vm.host.env = crate::native::HostConfig::process_env();
        vm
    }

    /// `chezzi test --max-heap=<N>` — set the per-test live-heap cap in bytes (`0` = OFF, the
    /// default). A test whose live heap exceeds `N` is hard-aborted (bypassing `recover:`) and
    /// bucketed `OverMemory`. Deterministic-in-VM (not OS RSS). M1.7: the cap is run-wide: every
    /// task allocates into the run's one heap, and any sweep measures it, so tasks whose live bytes
    /// only sum over the cap trip it too (`max_heap_caps_the_run_not_each_task`).
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
    /// that takes `run::run_file_bytes` (see `vm::RunOutputRaw`).
    pub fn take_out(&mut self) -> String {
        String::from_utf8_lossy(&self.take_out_bytes()).into_owned()
    }

    /// The byte-exact sibling of [`Vm::take_out`] — take + clear whatever a test printed to stdout,
    /// with no UTF-8 hop. `chezzi test --show-output` uses this so non-UTF-8 captured stdout
    /// (`b"\xff\xfe"`) reaches fd 1 unchanged, matching `chezzi run` (W6-9) and `go test`.
    pub fn take_out_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// The stderr twin of [`Vm::take_out_bytes`] — take + clear whatever a test printed via
    /// `io.eprint`. `chezzi test --show-output` writes this straight to fd 2, the way `go test`
    /// passes a test's stderr straight through.
    pub fn take_err_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.stderr)
    }

    /// `chezzi test` calls this right after a failed invoke and before the next one — take + clear
    /// the deepest-wins fault trace latch, so a passing test never inherits the previous test's
    /// frames and a shallower later fault still captures.
    pub fn take_fault_trace(&mut self) -> Vec<TraceFrame> {
        self.fault_trace_depth = 0;
        self.fault_trace.take().unwrap_or_default()
    }

    /// `chezzi test` — drain anything the program left running (e.g. an Executor a test forgot to
    /// shut down), as the ordinary run's end does ([`Vm::finish_run`]). An `Err` is a job fault
    /// that landed after the last test: the caller reports it (TICKET-208, C2).
    pub fn reap_after_tests(&mut self) -> Result<(), RuntimeError> {
        self.finish_run(Ok(()))
    }

    /// The current length of `exec_registry` — a mark of "every `Executor` created so far". Safe to
    /// take before a test's invoke and reuse after it: the registry is append-only (its one mutation
    /// is the `push` in `Vm::submit`'s ctor path), so an index taken here never shifts.
    pub fn exec_registry_mark(&self) -> usize {
        self.exec_registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// TICKET-200 — THE end-of-run ranking: which cause a run (or one test) reports. The order is
    /// exit, then an unjoined `Executor` job fault recorded before the run's own cause reached the
    /// top, then that cause (`--timeout`, `--max-heap`, an own fault, a deadlock). Go's first panic
    /// ends the process, so a job that faulted first is the report, with the job's frames; an own
    /// fault that happened first stays the report, because the job fault does not exist yet.
    /// "Earlier" means recorded when the cause reaches the top: a job fault recorded while the
    /// top's `defer`s unwind counts too (no timestamp is kept). Only executors created since `mark`
    /// count (a test's own; `0` for a whole run).
    ///
    /// Call it BEFORE any drain or reap: a drain takes the slots this reads. A replaced deadlock
    /// keeps its `.deadlock()` stamp (DEC-135), so `finish_run` still drains on it.
    pub fn rank_end(&mut self, r: Result<(), RuntimeError>) -> Result<(), RuntimeError> {
        // The ONLY taker of the job-fault cell, and it reports what it takes — for an `Ok` run
        // too: a job can fault while a passing run reads no halt.
        let job = self.quiesce.take_job_fault();
        // TICKET-223 — the ONLY taker of the deadlock report too, so a drain after the verdict finds
        // no report and its join waits for the victims (the verdict itself stays latched).
        let dl = self.quiesce.take_deadlock_report();
        if self.pending_exit.is_some() {
            return r;
        }
        if let Some((je, trace)) = job {
            self.adopt_child_fault(None, trace);
            return Err(if matches!(&r, Err(e) if e.is_deadlock) {
                je.deadlock()
            } else {
                je
            });
        }
        match (r, dl) {
            (Ok(()), Some((msg, site))) => Err(self.err(msg, site).deadlock()),
            (r, _) => r,
        }
    }

    /// The per-test A2 join: drain (join) only the executors created SINCE `mark`, i.e. by the test
    /// that took it. An executor built at module top level or in `before_all` sits before every
    /// mark and is left alive for the tests that follow — draining from `0` instead would mark it
    /// `shut` after the first test, and the next test's `submit` on it would fault.
    pub fn reap_executors_since(&mut self, mark: usize) -> Result<(), RuntimeError> {
        let r = self.drain_live_executors_from(mark);
        // A job fault that lands during the reap is taken here, by the test that reaped.
        self.rank_end(r)
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
            // TICKET-183: a let-created slot starts uninitialized (its line rides in the marker), so a
            // read before the let runs faults; a hoisted slot starts nil until its binding runs.
            slots: (0..m.global_slots.len())
                .map(|i| match m.let_lines.get(i).copied().unwrap_or(0) {
                    0 => Value::nil(),
                    line => Value::uninit(line),
                })
                .collect(),
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
                let nat = self.fn_value(FnKey::Native((*mname).into(), *func as usize), || {
                    Obj::Native {
                        name: (*mname).into(),
                        func: *func,
                        kind: *kind,
                    }
                });
                self.module_define(mod_obj, mname, nat);
            }
        }

        // Bind imports (dependencies already ran, so their namespaces are populated).
        for imp in &m.imports {
            self.bind_import(mod_obj, imp)?;
        }

        // Run the module body once. No module auto-runs `main` — it's an ordinary function the
        // program calls itself (scripting-language model). An unhandled `Err`/`None` reaching the
        // top level via a top-level `?` exits during this call; a bare dropped value does not.
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
        imp: &crate::base::module_id::ResolvedImport,
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
            // The resolver never resolves a variant import, so none reaches here.
            Import::Variants { .. } => {}
            Import::From { names, .. } => {
                for (member, alias) in names {
                    // Bind a member exactly when the importer holds a slot for it: its compiled slot
                    // list (`resolver::graph_global_binds`) holds a `from` member iff the home module
                    // holds one. A `from`-imported type carries no runtime value, so it has no slot
                    // here and is skipped, even when the importer's own `fn` of that name holds a
                    // slot (it shadows a prelude type). A member the home does not declare is a
                    // check error, never a run-time one.
                    if let Some(idx) = self.slot_index(target_obj, member) {
                        let value = self.read_slot(target_obj, idx, imp.span)?;
                        self.module_define(into, alias.as_ref().unwrap_or(member), value);
                    }
                }
            }
        }
        Ok(())
    }

    /// `Executor()` / `Executor(n)`: a fresh, registered Executor whose `limit` caps the jobs
    /// running at once (zero means no cap).
    pub(super) fn new_executor(&mut self, limit: usize, span: Span) -> Value {
        let core = Arc::new(ExecutorCore {
            limit,
            // W7-39, TICKET-208 — an executor inherits its CREATOR's whole cancel chain, as
            // every spawned task does, so an outer `cancel()` or scope cancel reaches
            // the jobs this executor will start. Captured HERE, at creation, not at `submit`:
            // the handle crosses the airlock by `Arc`, so the submitter can belong to an
            // unrelated executor.
            creator_cancel: self.scope_ancestors(),
            created_at: span,
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
        Value::obj(h)
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
