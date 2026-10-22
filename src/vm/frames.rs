//! VM: frames and traces, `run_until`, step faults, GC roots, jumps, halts and cancellation,
//! nurseries.
//! Split out of `src/vm/exec.rs`. `super::*` is the parent module.

use super::block::{self, Cut, Halt, OwnedScope, WakeSet};
use super::*;

/// TICKET-287 — a fault unwind parked at a fiber-owned nursery whose cancelled family is not done
/// (route (a) of w23 E1). The fiber's OS thread returns to its runner instead of running other
/// fibers on the faulting fiber's stack; `MnSched::park_join` requeues the fiber when the family
/// is done, and [`Vm::resume_fault_unwind`] runs the rest of the unwind. A fault has no op to
/// rewind, so the fault itself is what the fiber carries (`Fiber::fault_park`).
pub(super) struct FaultPark {
    at: ParkAt,
    rte: RuntimeError,
    /// The `defer` fault the frames above the park point raised, ranked at the end.
    defer_err: Option<RuntimeError>,
    cut: Option<Cut>,
    cancel_unwind_faulted: bool,
}

/// Where in [`Vm::on_step_fault`] the unwind parked.
enum ParkAt {
    /// In the frame unwind down to `target` (uncaught, or above the catching `recover:`), before
    /// the abort and the `defer`s of the frame it stopped at.
    Unwind {
        target: usize,
        std_site: Option<(Span, Span)>,
    },
    /// At the catching `recover:`'s own nursery reclaim, after its recover-block `defer`s.
    Catch { nursery_len: usize },
    /// In the cancel funnel's unwind ([`Vm::cancel_unwind`]); `escaped_err` is the first child
    /// fault of a nursery the frames above the park point aborted.
    Cancel { escaped_err: Option<RuntimeError> },
    /// A `return` or `?` escape that parked with a body `defer` fault (`rte`) in hand. Nothing
    /// resumes here: the rewound op re-runs and takes the fault back ([`Vm::take_escape_park`]).
    Escape,
}

impl Vm {
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

    /// Re-run the current op when this task resumes. Every running task has a frame:
    /// `Vm::task_entry` gives a task whose head pushes none an entry thunk, and `start_task`
    /// refuses any other (TICKET-235).
    #[inline]
    pub(super) fn rewind_op(&mut self) {
        self.frames
            .last_mut()
            .expect("a running task has a frame")
            .ip -= 1;
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
            if self.quiesce.run_halt_hint() {
                self.call_halt(span)?;
            }
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

    /// TICKET-224 — the run-wide halt at a function entry. A loop-free recursion emits no back-edge
    /// and no wait, so an `os.exit`, a fire-and-forget job fault or a latched deadlock verdict
    /// waited for the whole recursion (`fib(36)` printed 8.5 s after a 100 ms exit). Only the
    /// run-wide halt lands here, through [`Vm::exit_halt`] (DEC-208: one predicate, one funnel). A
    /// cancel and a nursery child's fault do not: cancellation stays at waits and loop back-edges
    /// (owner decision 2026-10-07, DEC-194), as Go's `context` and Python's `TaskGroup`.
    /// Cold and out of line: every call runs `frame_depth_guard`, and a run with no pending halt
    /// pays only the `run_halt_hint` load there.
    #[cold]
    #[inline(never)]
    fn call_halt(&mut self, span: Span) -> Result<(), RuntimeError> {
        match self.exit_halt(span) {
            Some(e) => Err(e),
            None => Ok(()),
        }
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
                function: crate::base::provider_name::display_fn_name(
                    &self.program.protos[f.proto].name,
                )
                .into_owned(),
                span: f.call_span,
            })
            .collect()
    }

    /// TICKET-148 — where a NATIVE fault raised inside std should point: the call site in user code
    /// that entered the innermost run of std frames (Rust's `#[track_caller]`). `None` when the
    /// innermost frame is user code or no frame's caller is a user file: decline, never guess.
    pub(super) fn std_entry_call_site(&self) -> Option<Span> {
        for f in self.frames.iter().rev() {
            let own = self.program.protos[f.proto]
                .lines
                .iter()
                .find(|s| s.file != 0);
            if !own.is_some_and(|s| self.program.file_is_std(s.file)) {
                return None;
            }
            if f.call_span.file != 0 && !self.program.file_is_std(f.call_span.file) {
                return Some(f.call_span);
            }
        }
        None
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
                // sweep just run, hard-abort it. Modeled on the `Cut::Cancelled` funnel below: unwind
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
                    let d = self.unwind_deferred(base_level, false);
                    return Err(self.unwind_result(over_rte, d, false).over_memory());
                }
            }
            // CANCELLATION POINTS (the every-instruction cancel check that used to sit here is GONE).
            // A cancel is observed only at CHECKPOINTS: loop back-edges (`jump_checked`, below) and
            // blocking/park ops (`chan_recv_step` / `op_wait_poll` / the blocking-native offload).
            // Consequence, intended (Trio-style structured concurrency): a STARTED task always runs
            // its straight-line prologue, so a `defer` it registers is ALWAYS registered before
            // anything can kill it — "does my cleanup run?" no longer depends on scheduler timing.
            // The cancel still unwinds like an uncaught fault that bypasses `recover:` — see the
            // post-step funnel below (`Cut::Cancelled` ⇒ `unwind_deferred(base_level, false)`).
            //
            // D3: reduction-counting preemption — gated on `self.mn` (an M:N worker shell running fibers
            // off the shared queue) or on `slice_in_place` (TICKET-205: an `Executor` job or a gated
            // nursery body, which has no sched of its own but is a party of `width::RUNNERS`).
            // Decrement the budget per dispatched op; at
            // exhaustion yield this worker so a queued sibling runs (round-robin fairness). A yielded
            // cancelled fiber observes the cancel at its next checkpoint. The
            // `native_reentry == 0` guard mirrors `recv`-park: a yield inside a native callback can't
            // save the caller's Rust-stack state, so the fiber cannot be swapped off. Instead the
            // THREAD hands its width permit to a sibling turn in place (TICKET-141,
            // `slice_end_in_place`). Outside a callback this reuses the suspend/rewind contract —
            // frames stay intact, resume re-enters `run_until(0)` — but carries no channel handle (a
            // voluntary park).
            if self.mn.is_some() || self.slice_in_place {
                if self.reds == 0 {
                    debug_assert!(
                        width::holds(),
                        "TICKET-230: a Chezzi slice ran without a runner permit"
                    );
                    if self.mn.is_some() && self.native_reentry == 0 {
                        self.yield_now = true;
                        return Ok(());
                    } else {
                        self.slice_end_in_place();
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
                    crate::base::op::BinKind::Add,
                    span,
                ),
                Op::Sub => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::base::op::BinKind::Sub,
                    span,
                ),
                Op::Mul => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::base::op::BinKind::Mul,
                    span,
                ),
                Op::Div => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::base::op::BinKind::Div,
                    span,
                ),
                Op::Mod => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::base::op::BinKind::Mod,
                    span,
                ),
                Op::Lt => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::base::op::BinKind::Lt,
                    span,
                ),
                Op::LtEq => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::base::op::BinKind::LtEq,
                    span,
                ),
                Op::Gt => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::base::op::BinKind::Gt,
                    span,
                ),
                Op::GtEq => self.q_arith(
                    self.quicken_base[pid] as usize + ip,
                    crate::base::op::BinKind::GtEq,
                    span,
                ),
                Op::Eq => self.q_eq(self.quicken_base[pid] as usize + ip, false, span),
                Op::NotEq => self.q_eq(self.quicken_base[pid] as usize + ip, true, span),
                other @ Op::ConstInt(_)
                | other @ Op::ConstFloat(_)
                | other @ Op::ConstStr(_)
                | other @ Op::ConstBytes(_)
                | other @ Op::True
                | other @ Op::False
                | other @ Op::Nil
                | other @ Op::Pop
                | other @ Op::StampGen(_)
                | other @ Op::Assert { .. }
                | other @ Op::LoadBuiltin(_)
                | other @ Op::GetGlobalSlot(_)
                | other @ Op::DefineGlobalSlot(_)
                | other @ Op::SetGlobalSlot(_)
                | other @ Op::GetCaptured(_)
                | other @ Op::Neg
                | other @ Op::Not
                | other @ Op::BitAnd
                | other @ Op::BitOr
                | other @ Op::BitXor
                | other @ Op::Shl
                | other @ Op::Shr
                | other @ Op::Contains
                | other @ Op::AsBool
                | other @ Op::AsInt
                | other @ Op::JumpIfFalseKeep(_)
                | other @ Op::JumpIfTrueKeep(_)
                | other @ Op::PushHandler(_)
                | other @ Op::PopHandler
                | other @ Op::CallMethod { .. }
                | other @ Op::CallHook { .. }
                | other @ Op::CallStatic { .. }
                | other @ Op::CallStaticDyn { .. }
                | other @ Op::CallBuiltin(..)
                | other @ Op::CallPrint(_)
                | other @ Op::CallPrintSep { .. }
                | other @ Op::Yield
                | other @ Op::DeferCall(_)
                | other @ Op::DeferMethod(..)
                | other @ Op::EnterDeferScope
                | other @ Op::LeaveDeferScope
                | other @ Op::DrainHandlerDefers
                | other @ Op::Try
                | other @ Op::JsonDecode(_)
                | other @ Op::JsonToValue
                | other @ Op::NewList(_)
                | other @ Op::NewTuple(_)
                | other @ Op::NewMap(_)
                | other @ Op::NewSet(_)
                | other @ Op::NewStruct(..)
                | other @ Op::NewCell
                | other @ Op::CellLoad
                | other @ Op::CellStore
                | other @ Op::NewEnum { .. }
                | other @ Op::MakeFunc(_)
                | other @ Op::MakeFuncIn(_)
                | other @ Op::MakeMethodFunc { .. }
                | other @ Op::JumpIfProvided(..)
                | other @ Op::MakeCffi(_)
                | other @ Op::MakeClosure(..)
                | other @ Op::GetField { .. }
                | other @ Op::SliceRange
                | other @ Op::SetField { .. }
                | other @ Op::Dup
                | other @ Op::Dup2
                | other @ Op::ToStr
                | other @ Op::ToStrFmt(_)
                | other @ Op::BuildStr(_)
                | other @ Op::ListClone
                | other @ Op::ArrLen
                | other @ Op::HasNextHook
                | other @ Op::IsGenerator
                | other @ Op::IsCursor
                | other @ Op::IterableToCursor
                | other @ Op::IsChannel
                | other @ Op::ChanRecvOrClosed
                | other @ Op::EnsureEnum(_)
                | other @ Op::MatchArm { .. }
                | other @ Op::MatchNoArm(_)
                | other @ Op::EnterNursery
                | other @ Op::JoinNursery
                | other @ Op::ReclaimNursery
                | other @ Op::SpawnCall(..)
                | other @ Op::SpawnRecv(..)
                | other @ Op::SpawnBlock(..)
                | other @ Op::WaitPoll(_)
                | other @ Op::NewChannel(_)
                | other @ Op::NewShared
                | other @ Op::NewRwShared
                | other @ Op::NewAtomic
                | other @ Op::NewAtomicInt
                | other @ Op::NewExecutor
                | other @ Op::NewExecutorN => self.step(other, span),
            };
            if let Err(rte) = step_result {
                self.on_step_fault(base_level, rte)?;
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

    /// `run_until`'s post-step fault arm, out of line: a caught fault resumes at its `recover:`
    /// boundary (`Ok`), anything else propagates (`Err`). TICKET-188: `run_one_fiber`'s offload
    /// resume delivers a halt through this same arm, so a `recover:` outside the faulting nursery
    /// still catches a child fault that arrives there (DEC-096).
    #[cold]
    #[inline(never)]
    pub(super) fn on_step_fault(
        &mut self,
        base_level: usize,
        rte: RuntimeError,
    ) -> Result<(), RuntimeError> {
        // `std.os.exit(code)` is a hard halt: unwind past every `recover:` to the top.
        if self.pending_exit.is_some() {
            return Err(rte);
        }
        // B3.4: a cancel observed deeper in this step (a blocking `recv` that woke on the
        // nursery cancel flag set `Cut::Cancelled` and returned the sentinel) unwinds the
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
        // W7-3 CARVE-OUT — the (a) `Cut::Cancelled` marker ONLY. `Cut::Cancelled` is a
        // task-wide LATCH that stays set while the cancelled task's `defer`s run, but "a
        // `defer` is never itself cancelled" (docs/reference/concurrency/) and `cancel_suppressed`
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
        // W13-20 — take the parked generator prefix UNCONDITIONALLY, above the bypass block
        // and the capture below. The prefix must be consumed exactly once per fault: a fault
        // this level CATCHES, or that leaves via the cancel / `--max-heap` / `--timeout`
        // bypass just below, drops it here instead of surviving to decorate an unrelated
        // later fault.
        let gen_prefix = std::mem::take(&mut self.gen_fault_prefix);
        // TICKET-135 (D1): a deadlock verdict is fatal like Go's `all goroutines are asleep`.
        // `recover:` is transparent to it: the fault takes the uncaught path (defers and
        // escaped-nursery reports unchanged) and the marker is re-stamped so a faulting
        // `defer` cannot strip it.
        let fatal = rte.is_deadlock;
        let caught_here =
            !fatal && matches!(self.handlers.last().copied(), Some(h) if h.frame_len > base_level);
        // TICKET-208 — a delivered job fault (`Cut::RunFault`) bypasses `recover:` as a cancel
        // does: it ends the run, and this party's `defer`s still run.
        let cancel_bypass = (self.is_cancelled() || matches!(self.cut, Some(Cut::RunFault)))
            && !(self.deferring > 0 && caught_here);
        // TICKET-096 — a nursery OWNER's cut is `Delivered { floor: Some(n) }` while a child fault
        // recorded at `nurseries` index `n` is unwinding it. A handler with `Handler::nursery_len
        // > n` was installed INSIDE nursery `n`'s body — in the cancelled scope — and must not
        // catch (measured: `recover:` wrapping a `parallel:` whose child panics keeps printing
        // `outer r=Err('boom')` at rc=0, since that handler sits at `nursery_len <= n`, outside
        // the nursery). The `n < self.nurseries.len()` term is a liveness guard: a floor left by a
        // fault that was neither caught nor propagated cannot bypass an unrelated later handler.
        // This makes the delivery happen ONCE — the owner aborts at the first delivery instead of
        // catching and re-reading the recorded fault at every later checkpoint.
        // `n` is task-wide (`Vm::owned_nurseries`): inside a box call a nursery of a parked caller
        // sits below `base`, so every handler of this context is inside it.
        let base = self.nursery_base();
        let owner_bypass = matches!(
            (self.cut, self.handlers.last()),
            (Some(Cut::Delivered { floor: Some(n) }), Some(h))
                if caught_here && n < base + self.nurseries.len() && base + h.nursery_len > n
        );
        if cancel_bypass || owner_bypass || rte.is_over_memory || rte.is_timed_out {
            let over_mem = rte.is_over_memory;
            let timed = rte.is_timed_out;
            // TICKET-135 (W14-39): a cancelled task aborts and joins every nursery it unwinds
            // past, like a faulting one. With `false` its nested nursery's parked children
            // were orphaned, and at CHEZZI_THREADS=1 the join hung. The hard halts
            // (`--max-heap`, `--timeout`) keep `false`.
            // TICKET-287 — an owner bypass aborts its nurseries too: with `false` the owner left
            // its cancelled family running, and a sleeping child's wake found no runner. At a
            // fiber's outermost loop both funnels park where the fault unwind parks. A hard halt
            // still waits inline.
            let report = cancel_bypass || owner_bypass;
            if report && base_level == 0 && !over_mem && !timed {
                return self.cancel_unwind(rte, None, None);
            }
            let rte = self.unwind_cancelled(base_level, report, rte);
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
            let mut trace = gen_prefix;
            trace.extend(self.capture_trace());
            self.fault_trace = Some(trace);
            self.fault_trace_depth = self.frames.len();
        }
        // The nearest `recover:` boundary owned by THIS dispatch loop catches the fault; a
        // handler at/below `base_level` belongs to an outer loop, so we unwind to
        // `base_level` and propagate. Either way, every frame discarded on the way runs its
        // deferred calls first (Go: defers run as the panic unwinds, before recover regains
        // control). A fault inside a deferred call supersedes the original.
        // TICKET-148 — computed while the frames are intact; applied below (at the catch's
        // stamp, and to the uncaught headline) only if the error is still this one (a
        // faulting `defer` may supersede it).
        let std_site = if !fatal && !rte.is_panic && self.program.file_is_std(rte.span.file) {
            self.std_entry_call_site().map(|site| (rte.span, site))
        } else {
            None
        };
        let target = match self.handlers.last().copied() {
            Some(h) if !fatal && h.frame_len > base_level => h.frame_len,
            _ => base_level,
        };
        // A genuine fault (not a B3.4 cancel / `std.os.exit`, both handled above) cancels-and-
        // reports each unwound frame's escaped nurseries — emitted PER FRAME, BEFORE that
        // frame's `defer`s, matching the interp oracle (whose `exec_parallel` /
        // `leave_implicit_nursery` report as the body unwinds, then `finish_frame` runs the
        // defers). `unwind_deferred` does the interleaving; this covers BOTH the uncaught arm
        // (no handler) and the frames discarded above a catching `recover:`.
        // TICKET-152 (W14-37): a FATAL deadlock drops its frames without running their
        // `defer`s, as Go's all-goroutines-asleep abort does (parked siblings already run
        // none, DEC-092). Per-call, never a VM latch: `chezzi test` keeps running.
        // TICKET-287 — at a fiber's outermost loop the unwind parks at a fiber-owned nursery that
        // is not done, instead of running other fibers on this stack while it waits.
        let rte = if fatal {
            self.unwind_no_defer(target);
            rte
        } else {
            let (d, _) = self.unwind_deferred_escaped(target, true, true, base_level == 0);
            if self.join_suspend.is_some() {
                self.store_fault_park(ParkAt::Unwind { target, std_site }, rte, d);
                return Ok(());
            }
            self.unwind_result(rte, d, false)
        };
        self.finish_fault(base_level, rte, fatal, std_site)
    }

    /// TICKET-287 — store the fault unwind this fiber parks in; [`Vm::resume_fault_unwind`] runs
    /// it on. The cut and its latch travel with it: `run_one_fiber` clears both on every resume.
    fn store_fault_park(&mut self, at: ParkAt, rte: RuntimeError, defer_err: Option<RuntimeError>) {
        self.fault_park = Some(FaultPark {
            at,
            rte,
            defer_err,
            cut: self.cut,
            cancel_unwind_faulted: self.cancel_unwind_faulted,
        });
    }

    /// TICKET-287 — a `return` or recover-scoped `?` parks with a body `defer` fault in hand; the
    /// rewound op takes it back on its re-run ([`Vm::take_escape_park`]), so the park loses nothing.
    pub(super) fn store_escape_park(&mut self, defer_err: RuntimeError) {
        self.store_fault_park(ParkAt::Escape, defer_err, None);
    }

    /// TICKET-287 — the body `defer` fault an escape parked with ([`Vm::store_escape_park`]).
    pub(super) fn take_escape_park(&mut self) -> Option<RuntimeError> {
        self.fault_park
            .take_if(|p| matches!(p.at, ParkAt::Escape))
            .map(|p| p.rte)
    }

    /// TICKET-287 — the cancel funnel of a fiber's outermost loop: [`Vm::unwind_cancelled`] down to
    /// frame 0 that parks at a fiber-owned nursery that is not done. `defer_err` and `escaped_err`
    /// are what the frames above an earlier park point raised.
    fn cancel_unwind(
        &mut self,
        rte: RuntimeError,
        defer_err: Option<RuntimeError>,
        escaped_err: Option<RuntimeError>,
    ) -> Result<(), RuntimeError> {
        let (d, e) = self.unwind_deferred_escaped(0, true, true, true);
        // A lower frame's `defer` runs later and supersedes; the first escaped child fault stays first.
        let d = d.or(defer_err);
        let escaped_err = escaped_err.or(e);
        if self.join_suspend.is_some() {
            self.store_fault_park(ParkAt::Cancel { escaped_err }, rte, d);
            return Ok(());
        }
        Err(self.unwind_result(rte, d.or(escaped_err), true))
    }

    /// TICKET-287 — resume a fault unwind that parked ([`Vm::store_fault_park`]) once the family it
    /// waited for is done. The abort it parked at now finds the family done and waits for nothing.
    /// A lower level that is not done parks it again.
    pub(super) fn resume_fault_unwind(&mut self, p: FaultPark) -> Result<(), RuntimeError> {
        if matches!(p.at, ParkAt::Escape) {
            // The rewound op re-runs next and takes its fault back (`take_escape_park`).
            self.fault_park = Some(p);
            return Ok(());
        }
        self.cut = p.cut;
        self.cancel_unwind_faulted = p.cancel_unwind_faulted;
        match p.at {
            ParkAt::Escape => Ok(()),
            ParkAt::Cancel { escaped_err } => self.cancel_unwind(p.rte, p.defer_err, escaped_err),
            ParkAt::Unwind { target, std_site } => {
                let (d, _) = self.unwind_deferred_escaped(target, true, true, true);
                // A lower frame's `defer` runs later, so its fault supersedes an earlier one.
                let d = d.or(p.defer_err);
                if self.join_suspend.is_some() {
                    self.store_fault_park(ParkAt::Unwind { target, std_site }, p.rte, d);
                    return Ok(());
                }
                let rte = self.unwind_result(p.rte, d, false);
                self.finish_fault(0, rte, false, std_site)
            }
            ParkAt::Catch { nursery_len } => self.reclaim_caught(0, nursery_len, p.rte),
        }
    }

    /// The tail of [`Vm::on_step_fault`] after the frame unwind: relocate a std fault, then catch
    /// at the nearest `recover:` of this loop or propagate.
    fn finish_fault(
        &mut self,
        base_level: usize,
        rte: RuntimeError,
        fatal: bool,
        std_site: Option<(Span, Span)>,
    ) -> Result<(), RuntimeError> {
        let rte = if fatal { rte.deadlock() } else { rte };
        // TICKET-148 — relocate a NATIVE std fault to the user's call into std, for the
        // caught stamp below AND the uncaught headline (the trace keeps its frames). A
        // superseding `defer` fault (a different span) and a `panic` stay where they are.
        let rte = match std_site {
            Some((raised, site)) if raised == rte.span && !rte.is_panic => {
                RuntimeError { span: site, ..rte }
            }
            _ => rte,
        };
        // A deferred `std.os.exit`, or a cleanup that met the latched verdict, turns the unwind
        // into a hard halt.
        let rte = self.cleanup_met_halt(base_level, rte)?;
        match self.handlers.last().copied() {
            Some(h) if !fatal && h.frame_len > base_level => {
                self.handlers.pop();
                // This `recover:` caught the fault — discard any trace captured deeper in (it
                // belongs to a fault that is now handled), so a later uncaught fault re-captures.
                self.fault_trace = None;
                self.fault_trace_depth = 0;
                self.cancel_unwind_faulted = false;
                // `unwind_deferred` already dropped frames down to `h.frame_len`; restore the
                // operand stack / call-depth / ip to the boundary's snapshot.
                self.stack.truncate(h.stack_len);
                self.call_depth = h.call_depth;
                self.cur_base = self.frames.last().map(|f| f.base).unwrap_or(0);
                self.frames[h.frame_len - 1].ip = h.ip;
                // `unwind_deferred` ran the defers of frames ABOVE the boundary, but the
                // boundary frame's own (recover-block) defers remain — drain them now, before
                // binding the result. A fault in one supersedes the original.
                let d = self.drain_frame_to(h.defer_len);
                let rte = self.unwind_result(rte, d, false);
                // TICKET-096 — this handler is outside the faulting nursery, so the fault is
                // handled; the floor must not survive it and bypass an unrelated later handler.
                // TICKET-195 — cleared only AFTER the recover-block drain just above, which still
                // ranks its `defer` fault below the delivered one.
                if matches!(self.cut, Some(Cut::Delivered { .. })) {
                    self.cut = None;
                }
                // W13-20 — clear the prefix HERE, after every drain this catch runs
                // (`unwind_deferred` above AND this recover-block drain), not before either
                // one. A deferred `.next()` — whether outside the recover block (drained by
                // `unwind_deferred`) or DIRECTLY INSIDE it (drained by `drain_frame_to` just
                // above) — can fault and park its own frames here without ever passing back
                // through this loop's error arm to be taken (that only happens for a fault
                // THIS dispatch loop raises directly). Clearing before either drain leaves the
                // second drain's fault to survive uncleared and decorate a later, unrelated
                // uncaught fault — the review finding this fixes.
                self.gen_fault_prefix.clear();
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
                // TICKET-287 — a fiber-owned nursery that is not done parks this fiber here,
                // after the recover-block defers, as the frame unwind parks.
                self.reclaim_caught(base_level, h.nursery_len, rte)
            }
            // Uncaught: `unwind_deferred(target, true)` above already cancelled-and-reported
            // every unwound frame's escaped nurseries (the toplevel module nursery preserved).
            _ => Err(rte),
        }
    }

    /// TICKET-287/TICKET-294 — the catching `recover:`'s nursery reclaim. At a fiber's outermost
    /// loop it parks at each fiber-owned level that is not done ([`Vm::park_abort_origin`]), on the
    /// first run and on every resume, so no level waits inline on this fiber's stack.
    fn reclaim_caught(
        &mut self,
        base_level: usize,
        nursery_len: usize,
        rte: RuntimeError,
    ) -> Result<(), RuntimeError> {
        if base_level == 0
            && let Some(origin) = self.park_abort_origin(nursery_len)
        {
            self.join_suspend = Some(origin);
            self.store_fault_park(ParkAt::Catch { nursery_len }, rte, None);
            return Ok(());
        }
        self.finish_catch(base_level, nursery_len, rte)
    }

    /// The end of a `recover:` catch: reclaim the nurseries above `nursery_len`, then push the
    /// caught fault as the boundary's `Err` value.
    fn finish_catch(
        &mut self,
        base_level: usize,
        nursery_len: usize,
        rte: RuntimeError,
    ) -> Result<(), RuntimeError> {
        // TICKET-147 — the caught fault is the root cause; a child's fault is dropped.
        let _ = self.drain_escaped_nursery(nursery_len);
        let rte = self.cleanup_met_halt(base_level, rte)?;
        // Convert the fault message (a `str`, i.e. an `Error`) into `Err(msg)`; the
        // boundary's `done` label receives a ready `Result`.
        let sp = rte.span;
        let msg = self.alloc_str(rte.message);
        // W8-22 — stamp the fault's origin on the message handle so `e.line()` etc.
        // can read it back later. `msg` is always a fresh `Obj::Str`
        // (`alloc_str` allocates one per call), so this can never alias an
        // unrelated string's span. A stdlib-raised span is not stamped
        // (TICKET-045), except a NATIVE fault, which is relocated above to the
        // user's call into std (TICKET-148).
        self.stamp_err_span(msg, sp);
        let err = self.alloc_enum("Result", "Err", vec![msg]);
        self.push(err);
        Ok(())
    }

    /// Mark-sweep collection. Roots: see [`Vm::gc_roots`]. Everything else is garbage.
    pub(super) fn collect(&mut self) {
        let mut work = self.gc_roots();
        while let Some(h) = work.pop() {
            if self.heap.mark(h) {
                work.extend(self.heap.children(h));
            }
        }
        self.heap.sweep();
    }

    /// The GC roots: the whole operand stack (which contains every frame's local slots *and* any
    /// in-flight expression temporaries), each frame's home module + backing closure, and the module
    /// namespace cache.
    pub(super) fn gc_roots(&self) -> Vec<GcRef> {
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
        work.extend(self.fn_values.values().copied());
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
        // TICKET-111 — the snapshot node registry's KEYS, load-bearing for the same reason as
        // `snapshot_cells` above: an unrooted key could be swept and its slot recycled, and adoption
        // would then identify the wrong node with a live global's id.
        work.extend(self.snapshot_nodes.keys().copied());
        // TICKET-111 — the pending-adoption captures' VALUES (the capture's own rebuilt handle, not a
        // module-global key): load-bearing the same way, so a capture waiting to be adopted by a
        // module the task has not read yet is never swept out from under it.
        work.extend(self.snapshot_adopt.values().copied());
        // TICKET-246 — the slot-door cache of the box call now running indexes this (box) heap.
        // The frames below it index parked heaps and are not roots here.
        if let Some(frame) = self.box_chain.last() {
            work.extend(frame.roots());
        }
        work
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
    /// `!self.is_cancelled()` latches on the first observation: a `defer` containing a loop must not
    /// re-fire the check and skip the remaining defers while the cancel unwind is already in flight.
    /// The `Err` is funnelled by `run_until`'s post-step handler into `unwind_deferred(base_level,
    /// false)` — defers run, `recover:` inside the task is bypassed (a cancelled task must die).
    /// A run-wide halt also lands at a function entry ([`Vm::call_halt`], TICKET-224); a cancel does not.
    pub(super) fn jump_checked(&mut self, target: usize, span: Span) -> Result<(), RuntimeError> {
        let ip = self.frames.last().unwrap().ip;
        if target < ip {
            // `chezzi test --timeout` — observe the wall-clock deadline at THE loop back-edge, the
            // hottest engine-independent checkpoint. It covers both the top-level test body
            // (`invoke_test → run_proto → run_until`, which runs OUTSIDE the fiber scheduler) AND a
            // `spawn`ed fiber (its loop routes through here too), so a single check catches both.
            // Zero clock reads when the cap is OFF: the `Some` guard short-circuits before any
            // `Instant::now()`. Throttled to one read per 1024 back-edges (the read is the cost). The
            // `is_timed_out` marker alone drives the recover-bypass — no `Cut::Cancelled` latch.
            // The 1/1024 sample is shared by both wall-clock rungs below. Hoisted OUT of the `deadline`
            // guard (W7-57) because the exit rung needs it even when `--timeout` is off; the clock read
            // itself stays behind `Some(dl)`, so an uncapped run still reads no clock.
            self.back_edge_tick = self.back_edge_tick.wrapping_add(1);
            let sampled = self.back_edge_tick.is_multiple_of(1024);
            if sampled
                && let Some(dl) = self.deadline
                && std::time::Instant::now() >= dl
            {
                return Err(super::exec::timed_out_err(self.timeout_ms, span));
            }
            // TICKET-188 — a cancel, or a child fault of a nursery this party owns: one predicate.
            // The owned half rides the 1-in-1024 sample (see `halt_read`).
            if let Some(e) = self.take_halt_at(span, sampled) {
                return Err(e);
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

    /// TICKET-188 — THE halt predicate: must this party stop now, and why? Every checkpoint (`jump_checked`'s
    /// loop back-edge, `guarded`'s native-HOF re-entry, the would-wait path of every blocking op ([`Vm::wait_halt`]),
    /// every demote loop, [`Vm::join_executor`]) asks exactly this, through
    /// [`Vm::take_halt`]. It is [`block::halt_of`] over this party's cancel flags and open nurseries: a
    /// cancel, or a recorded fault of a child of a nursery it owns (unless a run-wide exit is pending).
    /// `None` while [`Vm::cancel_suppressed`]: two suppressions, both load-bearing:
    ///
    /// * `Cut::Cancelled` — latch: once the cancel unwind is in flight, a checkpoint inside it must
    ///   not re-fire and skip the remaining `defer`s.
    /// * `self.deferring > 0` — a deferred call is the cleanup the cancel exists to run. Defers drain
    ///   on the normal-return / own-fault paths too, where `cancelled` is still false while the scope
    ///   flag is already tripped by a faulted sibling; without this, the first checkpoint inside the
    ///   first deferred call returns `cancelled` and the defer body never executes.
    pub(super) fn halt_requested(&self) -> Option<Halt> {
        self.halt_read(true)
    }

    /// [`Vm::halt_requested`], with the owned half read only when `owned`. The two CPU checkpoints
    /// ([`Vm::jump_checked`], [`Vm::guarded_checkpoint`]) pass their 1-in-1024 sample: walking the
    /// open nurseries on every element cost +11.6 % on `benches/chz/hof_nursery.chz` (TICKET-188,
    /// `docs/contributing/benchmarks.md`). The cancel half still runs every time; it is the same `halt_of`.
    fn halt_read(&self, owned: bool) -> Option<Halt> {
        if self.cancel_suppressed() {
            return None;
        }
        if !owned || !self.owns_nursery() {
            return block::halt_of(self.cancel_flags().map(|a| &**a), std::iter::empty(), false);
        }
        self.halt_requested_owned()
    }

    /// [`Vm::halt_requested`] for a party with an open nursery. Out of line so a nursery-free loop
    /// pays one `is_empty()`.
    #[cold]
    #[inline(never)]
    fn halt_requested_owned(&self) -> Option<Halt> {
        block::halt_of(
            self.cancel_flags().map(|a| &**a),
            self.owned_nurseries(),
            self.quiesce.run_halt_hint(),
        )
    }

    /// [`Vm::halt_requested`], delivered: a cancel latches `cancelled` and returns the `cancelled`
    /// sentinel; a child fault records `Cut::Delivered { floor }` (DEC-096: only a `recover:` INSIDE the
    /// faulting nursery is bypassed) and returns the child's error.
    pub(super) fn take_halt(&mut self, span: Span) -> Option<RuntimeError> {
        self.take_halt_at(span, true)
    }

    /// [`Vm::take_halt`] over [`Vm::halt_read`]: the owned half only when `owned`.
    pub(super) fn take_halt_at(&mut self, span: Span, owned: bool) -> Option<RuntimeError> {
        let h = self.halt_read(owned)?;
        Some(self.deliver_halt(h, span))
    }

    pub(super) fn read_halts(&mut self, read: Option<Halt>, span: Span) -> Option<RuntimeError> {
        if let Some(h) = read {
            return Some(self.deliver_halt(h, span));
        }
        self.run_exit_err(span)
    }

    pub(super) fn blocked_halt(&mut self, read: Option<Halt>, span: Span) -> Option<RuntimeError> {
        if let Err(e) = self.deadline_halt(span) {
            return Some(e);
        }
        self.read_halts(read, span)
    }

    /// Deliver a [`Halt`] read by [`Vm::halt_requested`]. A wait that settles under its sched's core
    /// lock reads the halt BEFORE taking that lock (a child fault's `scope_fault` takes it too, and a
    /// fiber-owned nursery lives on the same sched) and delivers it here, below its settle. Out of
    /// line: it runs once per halt, and inlined it bloats the per-element checkpoints.
    #[cold]
    #[inline(never)]
    fn deliver_halt(&mut self, h: Halt, span: Span) -> RuntimeError {
        match h {
            Halt::Cancelled => {
                self.cut = Some(Cut::Cancelled);
                self.err("cancelled".to_string(), span)
            }
            Halt::ChildFault { floor, err, trace } => {
                self.adopt_child_fault(Some(floor), trace);
                err
            }
        }
    }

    /// TICKET-195 — THE one writer of [`Cut::Delivered`]: another party's fault (a child's, or a
    /// job's) is about to unwind this party. Installs the faulting party's own trace with
    /// `fault_trace_depth = usize::MAX`, so the capture in [`Vm::on_step_fault`] never replaces it
    /// with this party's frames. A cancel already latched keeps precedence (`halt_of`: cancel first).
    /// A party running a `defer` is never cut (as in [`Vm::cancel_suppressed`]): a fault a join
    /// inside the cleanup reduces is the cleanup's own, and supersedes the fault being unwound.
    pub(super) fn adopt_child_fault(&mut self, floor: Option<usize>, trace: Vec<TraceFrame>) {
        if self.is_cancelled() || self.deferring > 0 {
            return;
        }
        self.cut = Some(Cut::Delivered { floor });
        self.fault_trace = Some(trace);
        self.fault_trace_depth = usize::MAX;
    }

    /// TICKET-208 — this party is cut by the run-wide job fault: no `recover:` of its own catches
    /// it ([`Cut::RunFault`]), and the report carries the faulting job's frames.
    pub(super) fn adopt_run_fault(&mut self, trace: Vec<TraceFrame>) {
        self.cut = Some(Cut::RunFault);
        self.fault_trace = Some(trace);
        self.fault_trace_depth = usize::MAX;
    }

    /// TICKET-223 — `on_step_fault`'s read of `Vm::cleanup_halt`: on a verdict it also drops the
    /// frames left above `base_level` without their `defer`s (DEC-152).
    fn cleanup_met_halt(
        &mut self,
        base_level: usize,
        rte: RuntimeError,
    ) -> Result<RuntimeError, RuntimeError> {
        self.cleanup_halt(rte).inspect_err(|e| {
            if e.is_deadlock {
                self.unwind_no_defer(base_level);
            }
        })
    }

    /// TICKET-195 — is this party unwinding a cancel ([`Cut::Cancelled`])?
    pub(super) fn is_cancelled(&self) -> bool {
        matches!(self.cut, Some(Cut::Cancelled))
    }

    /// gaps.md W7-57 — how the three CPU-side checkpoints ([`Vm::jump_checked`]'s loop back-edge,
    /// [`Vm::guarded`]'s native-HOF re-entry, [`Vm::frame_depth_guard`]'s function entry via
    /// [`Vm::call_halt`], TICKET-224) deliver a run-wide `os.exit`. These three are the only
    /// halts a spinning party ever reaches, and they need one thing the blocking rungs' bare
    /// [`Vm::run_exit_err`] does not give them:
    ///
    /// **A fiber that HOLDS a cancel flag unwinds as `Cancelled`, not as an exit.** Its unwind runs
    /// no `defer`: `Vm::next_deferred` reads the published exit before each pop. Ordering the publication (`request_exit` → `halt_all_scheds` → flag) does NOT
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
    /// Nothing suppresses this rung (TICKET-213, owner decision 5): an exit cuts a running `defer`
    /// body, and it cuts an already-cancelled fiber too, which runs user code only inside a `defer`
    /// of its own unwind (keeping an `is_cancelled()` exemption hangs the exit on a spinning
    /// `defer`, measured). A job fault still waits for a `defer` to end: `run_halt_due` holds that.
    /// `pending()` — the `Mutex` cell, the authority — is confirmed before either arm, because the
    /// atomic is only a lock-free HINT and a `chezzi test` reset clears the cell.
    pub(super) fn exit_halt(&mut self, span: Span) -> Option<RuntimeError> {
        if !self.quiesce.run_halt_hint() || !self.run_halt_due() {
            return None;
        }
        if self.quiesce.pending().is_some() && self.cancel_flags().next().is_some() {
            self.cut = Some(Cut::Cancelled);
            return Some(self.err("cancelled".to_string(), span));
        }
        self.run_exit_err(span)
    }

    /// The cancel flags [`Vm::halt_requested`] reads: this fiber's own scope flag plus every ENCLOSING scope's
    /// (structured concurrency — an outer cancel cancels this one too; a nested nursery keeps its own
    /// flag for its own faults, and `cancel_outer` is normally empty).
    ///
    /// SINGLE SOURCE — [`Vm::wake_set`] (the N4 watch set) MUST stay exactly the flags the resume
    /// path re-reads, so both go through this + [`Vm::cancel_suppressed`] rather than hand-copying the
    /// set. (A watch that misses a flag ⇒ a cancel-wakeable demoted fiber is called a deadlock and its
    /// cleanup is truncated; a watch that misses a suppression ⇒ the veto never lifts and a genuine
    /// deadlock hangs silently.)
    fn cancel_flags(&self) -> impl Iterator<Item = &Arc<AtomicBool>> {
        self.cancel.iter().chain(self.cancel_outer.iter())
    }

    /// The two suppressions of the cancel predicate — a tripped flag does NOT cancel this fiber while
    /// either holds, and neither can change while it is blocked in place.
    pub(super) fn cancel_suppressed(&self) -> bool {
        self.is_cancelled() || self.deferring > 0
    }

    /// TICKET-188 — THE wake set of this party: the cancel flags [`Vm::halt_requested`] reads plus
    /// the flags of every nursery it owns, i.e. every flag whose trip may make it halt. EMPTY when
    /// nothing could cut it at all (`cancel_suppressed`: already unwinding, or inside a `defer`) — a
    /// party nothing can wake is exactly the one that IS a genuine deadlock. Every wait registration
    /// reads it: a demoted `Waiter` ([`Vm::block_enter`]) and a fiber's timer sleep (`TimerSleep`).
    pub(super) fn wake_set(&self) -> WakeSet {
        if self.cancel_suppressed() {
            return WakeSet::default();
        }
        WakeSet {
            cancel: self.cancel_flags().cloned().collect(),
            owned: self.owned_nurseries().map(|o| OwnedScope::of(&o)).collect(),
        }
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
    pub(super) fn op_enter_nursery(&mut self, span: Span) {
        self.nurseries.push(Vec::new());
        self.mn_scopes.push(None); // lockstep — set Some(scope_id) only if early-enlisted
        // TASK B — capture this parallel body's defer floor so a recover-scoped `?` can run
        // the body's defers before the nursery reclaim (see `nursery_defer_floors`).
        let floor = self.frames.last().map(|f| f.deferred.len()).unwrap_or(0);
        self.nursery_defer_floors.push(floor);
        self.nursery_spans.push(span);
        // §2c1 — EVERY nursery on the M:N engine activates an EAGER sched NOW, so a `spawn`
        // in the body injects a LIVE fiber that runs concurrently with the rest of the body.
        // That is Go's `go f()`: the task starts at the `spawn`, and the join keeps its own
        // (orthogonal) job of guaranteeing COMPLETION by the barrier (`docs/design/roadmap.md` §2b/§2c1).
        //
        // A nursery entered inside a spawned task (`mn.is_some()`) never builds a private sched:
        // at EVERY worker count it registers a scope on the fiber's OWN sched
        // (`activate_fiber_owned_nursery`), its tasks start at the `spawn` on that sched's
        // existing workers, adding no thread, and the owner PARKS at its join (`Disp::JoinPark`,
        // `MnSched::park_join`), releasing its worker until the family completes (TICKET-103).
        // That is Go's `gopark` releasing the P. TICKET-131 removed the worker-count clause that
        // gave such a nursery a private sched with a BLOCKING join at T>=2: that join held the
        // outer sched's only runner while a sibling stayed queued, so two recoverers fanning in
        // to an open body hung (W13-6). The clause could only go once an escape out of a
        // fiber-owned nursery parked instead of waiting inline (TICKET-132), because the inline
        // wait popped its own ancestor. Do not restore the clause, and do not add a replacement
        // worker for a joiner that can park (TICKET-125 and TICKET-127 false-faulted `i6n2` at
        // T=4); a joiner inside a native callback cannot park and hands its slot over in
        // `Vm::joiner_shell` (TICKET-245). Cost: while the OUTER body is open, these tasks share the outer sched's
        // runners (`docs/contributing/gaps.md` W13-27).
        //
        // A top-level nursery has no outer worker to starve and creates exactly ONE drainer per
        // thread, so it stays eager; if that drainer thread fails, `activate_fiber_owned_nursery`
        // returns `None` (there is no `self.mn`) and the nursery queues its tasks to the join.
        let eager = self.mn.is_none();
        let scope = eager
            .then(|| self.activate_eager_nursery(span))
            .flatten()
            .or_else(|| self.activate_fiber_owned_nursery(span));
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
        // …and the flags of every nursery this party has open, outermost first. §2c1: on `main` there
        // is no fiber, so `cancel`/`cancel_outer` are empty and a nested scope would observe no
        // ancestor at all. TICKET-188: a child fault in an enclosing nursery of the SAME owner cancels
        // the inner nursery's tasks, so an owner parked at an inner join is freed (CPython
        // `TaskGroup` cancels the body, which cancels the inner group). An `Executor` created outside
        // an eager job inherits nothing (`creator_cancel` in `Op::NewExecutor`); an owner in its
        // `shutdown()` is cut by its own halt read in `join_executor`. This used to be a
        // second walk, `nursery_ancestors`, read by only the eager-nursery seams.
        a.extend(self.owned_nurseries().map(|o| Arc::clone(o.flag)));
        a
    }

    pub(super) fn push(&mut self, v: Value) {
        self.stack.push(v);
    }

    pub(super) fn pop(&mut self) -> Value {
        self.stack.pop().expect("operand stack underflow")
    }
}
