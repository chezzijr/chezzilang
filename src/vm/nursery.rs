//! VM: spawn and the nursery lifecycle: `do_spawn`, task entry, start and registration, join and
//! escape, M:N and eager nurseries.
//! Split out of `src/vm/sched.rs`. `super::*` is the parent module.

use super::frames::{Caught, Unwind};
use super::mnsched_verdict::{CancelTarget, cancel};
use super::sched::SpawnHead;
use super::sched::SpawnTarget;
use super::sched::spawn_worker_thread;
use super::*;

/// Test-only: the halted joins that ran their family's fibers on their own stack although the
/// joiner had handed its runner slot over ([`Vm::joiner_hands_over`]). Must stay 0.
#[cfg(test)]
pub(crate) static INLINE_AFTER_HANDOVER: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Test-only: the halted joins that waited for their replacement or a new runner to settle the
/// family instead ([`Vm::wait_family`]).
#[cfg(test)]
pub(crate) static HANDOVER_WAITS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// TICKET-299: where an open `parallel:` level starts in its frame, captured at `Op::EnterNursery`:
/// the frame's `defer` count (the level's body `defer`s sit above it) and its defer-scope marker
/// count (the scopes the level opened sit above it).
#[derive(Clone, Copy, Debug)]
pub(super) struct LevelFloor {
    pub(super) defers: usize,
    pub(super) markers: usize,
}

/// TICKET-299: what a non-normal exit goes on with once it left one `parallel:` level.
pub(super) enum LevelThen {
    /// `return` and function-scoped `?` ([`Vm::return_levels`]); `err`: the fault in hand.
    Return { err: Option<RuntimeError> },
    /// A fault or cancel unwind ([`Vm::unwind_on`]) leaves the top level of the frame it is at.
    Unwind(Box<Unwind>),
    /// A `recover:` that caught a fault leaves a level the fault left inside its recover block
    /// ([`Vm::reclaim_caught`]).
    Catch(Box<Caught>),
    /// A recover-scoped `?` ([`Vm::try_levels`]) leaves the levels opened inside its recover block;
    /// `err`: the fault in hand, `span`: the `?`.
    Try {
        span: Span,
        err: Option<RuntimeError>,
    },
    /// `break` and `continue` ([`Op::ReclaimNursery`]): one level per op, innermost first.
    Jump,
}

impl LevelThen {
    /// Whether the exit is a fault or cancel unwind, whose failure has already run the fault arm
    /// ([`Vm::on_step_fault`]).
    pub(super) fn unwinds(&self) -> bool {
        match self {
            LevelThen::Return { .. } | LevelThen::Try { .. } | LevelThen::Jump => false,
            LevelThen::Unwind(_) | LevelThen::Catch(_) => true,
        }
    }
}

impl Vm {
    /// A nursery's sched-wide deadlock fault at `span`, for a task whose own wait no site named.
    /// Every cut that knows its wait re-sites it ([`RuntimeError::at_site`]).
    pub(super) fn nursery_deadlock_err(&self, span: Span) -> RuntimeError {
        self.err(String::new(), span).at_site(span)
    }

    /// Launch a fiber's initial task in the (already swapped-in) child context: push the callee's
    /// frame and return; the scheduler then runs it. The callee always pushes a frame (TICKET-235,
    /// [`Vm::task_entry`]), so a call that waits inside the task has an `ip` to rewind.
    pub(super) fn start_task(&mut self, task: PendingCall) -> Result<(), RuntimeError> {
        match task {
            PendingCall::Call { callee, args, span } => {
                // Sample the `--max-heap` cap before dispatch (see `sample_mem_cap`). The callee and
                // args are plain Rust locals here, so they are NOT rooted — park them on the operand
                // stack (which `collect` traces) across the sample and take them back after. Under a
                // live cap only, so the uncapped path is byte-for-byte what it was.
                let (callee, args) = if self.heap.mem_cap() != 0 {
                    let argc = args.len();
                    self.push(callee);
                    for a in args {
                        self.push(a);
                    }
                    self.sample_mem_cap(span)?;
                    let args = self.stack.split_off(self.stack.len() - argc);
                    (self.pop(), args)
                } else {
                    (callee, args)
                };
                // TICKET-235: `Vm::task_entry` gave every callable head that pushes no frame an
                // entry thunk. A task queued any other way stops here, not at a park's rewind.
                // (A non-callable callee falls through to `invoke_value`'s own fault.)
                if self.callable(callee).is_some() && !self.enters_frame(callee) {
                    return Err(self.err(
                        "internal error: a task must start in a Chezzi frame".to_string(),
                        span,
                    ));
                }
                self.invoke_value(callee, args, span)?;
                Ok(())
            }
        }
    }

    /// `spawn f(args)` / `spawn recv.m(args)` — pop `argc(+1)` operands and register the task on the
    /// innermost nursery. The callee, the receiver and the args pass by handle (M3.4, Go `go f(x)`),
    /// like `defer`.
    pub(super) fn do_spawn(
        &mut self,
        kind: SpawnHead,
        argc: usize,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.sched_seed_point();
        let at = self.stack.len() - argc;
        let raw_args: Vec<Value> = self.stack.split_off(at);
        let head = self.pop();
        if self.eager_scheds.is_empty() {
            return Err(self.err("spawn must be inside a parallel: block".to_string(), span));
        }
        self.spawn_into(SpawnTarget::Nursery, kind, head, raw_args, span)
    }

    /// TICKET-235 — does calling `v` push a Chezzi frame that the task then runs in? True for a
    /// non-generator fn or closure. A native fn, a builtin, a ctor and a generator fn push none:
    /// a park inside one would have no `ip` to rewind ([`Vm::rewind_op`]).
    pub(super) fn enters_frame(&self, v: Value) -> bool {
        matches!(
            self.callable(v),
            Some(Callee::Func { proto, .. } | Callee::Closure { proto, .. })
                if !self.program.protos[proto].is_generator
        )
    }

    /// TICKET-235 — THE decision of how a spawned call becomes a task body; every task it yields
    /// starts in a frame. A head that pushes its own frame runs directly, unchanged. Any other
    /// head (a receiver, a native fn value, a builtin, a ctor, a generator fn) becomes argument 0
    /// of the site's entry thunk. A non-callable callee passes through, so `prepare_worker`
    /// reports it.
    fn task_entry(
        &mut self,
        kind: SpawnHead,
        head: Value,
        mut args: Vec<Value>,
        span: Span,
    ) -> Result<(Value, Vec<Value>), RuntimeError> {
        let proto = match kind {
            SpawnHead::Callee(_) if self.enters_frame(head) || self.callable(head).is_none() => {
                return Ok((head, args));
            }
            SpawnHead::Callee(None) => {
                return Err(self.err(
                    "internal error: a spawned call has no entry thunk".to_string(),
                    span,
                ));
            }
            SpawnHead::Callee(Some(p)) | SpawnHead::Receiver(p) => p,
        };
        // The thunk value is built on the PARENT side, the way `Op::MakeFunc` builds any fn value.
        let home = self.frames.last().expect("a spawn runs in a frame").home;
        let entry = self.fn_value(FnKey::Proto(proto), || Obj::Func { proto, home });
        args.insert(0, head);
        Ok((entry, args))
    }

    /// Start one task: the ONE function behind `spawn` and `Executor.submit` (TICKET-208). The
    /// task gets the callee and its arguments by handle (M3.4), and `target` gets the task.
    /// Everything a new party inherits from its
    /// starter is decided here or below, once, whichever door started it.
    pub(super) fn spawn_into(
        &mut self,
        target: SpawnTarget,
        kind: SpawnHead,
        head: Value,
        raw_args: Vec<Value>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let (head, args) = self.task_entry(kind, head, raw_args, span)?;
        let task = PendingCall::Call {
            callee: head,
            args,
            span,
        };
        match target {
            SpawnTarget::Nursery => self.register_task(task, span),
            SpawnTarget::Scope {
                sched,
                settle,
                executor,
            } => {
                let rw = self.prepare_worker(task)?;
                // A job's values are objects of the run's one heap, which `live_bytes` counts
                // already, so the starter is charged no worker heap of its own.
                let mut fiber = rw.into_fiber(0, 0);
                // Reserve the slot and claim the runners it needs in ONE core-lock hold. A scope
                // whose join already closed it takes no new task: that is what makes a submit
                // racing a `shutdown()` either rejected or counted by that join.
                let wids = {
                    let mut c = sched.lock();
                    if !c.scopes.get(0).is_some_and(|s| s.body_open) {
                        drop(c);
                        return self.refuse_job(settle, super::EXEC_SHUT_MSG);
                    }
                    sched.reserve_job_slot(&mut c, &mut fiber, settle);
                    sched.claim_runners(&mut c)
                };
                fiber.ctx.task = super::deadlock::TaskPath::job(&executor, fiber.task_index);
                let runner = self.start_runners(&sched, wids);
                if !runner {
                    // No thread can run it: fill the slot so no join waits for it.
                    {
                        let mut c = sched.lock();
                        c.running += 1;
                        c.exec_active += 1;
                    }
                    sched.finish(
                        fiber.task_index,
                        fiber.scope_id,
                        TaskOutcome::Cancelled {
                            out: Vec::new(),
                            stderr: Vec::new(),
                        },
                    );
                    return Err(self.err(super::EXEC_NO_RUNNER_MSG.to_string(), span));
                }
                sched.submit_job(fiber);
                // W7-26, the sampling half — charge what this Executor retains against the run's
                // GC pacing counter, so a live cap gets sampled.
                if self.heap.mem_cap() != 0 {
                    self.heap.charge_bytes(sched.take_charge());
                }
                Ok(())
            }
        }
    }

    /// `spawn:` block — snapshot the captured bindings from the current frame (like `MakeClosure`),
    /// build a zero-arg closure over the synthetic block proto, and register it as a `Call` task.
    /// A captured local is its frame's cell (uniform by-reference capture), so the task and the
    /// parent share one binding (M3.4, a Go closure in `go func() { ... }()`); a loop variable has
    /// a cell per iteration.
    pub(super) fn do_spawn_block(
        &mut self,
        proto: ProtoId,
        entries: &[CapEntry],
        span: Span,
    ) -> Result<(), RuntimeError> {
        let frame = self.frames.last().unwrap();
        let (base, home, enclosing) = (frame.base, frame.home, frame.closure);
        let mut captured = Vec::with_capacity(entries.len());
        for e in entries {
            let v = match e.src {
                CapSrc::Slot(i) => self.stack[base + i],
                CapSrc::Captured(parent_slot) => enclosing
                    .and_then(|h| match self.heap.get(h) {
                        Obj::Closure { captured, .. } => captured.try_get(parent_slot as usize),
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
                        | Obj::Func { .. }
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
                    })
                    .unwrap_or(Value::nil()),
            };
            captured.push(v);
        }
        // Positional (lever #3): slot order matches the synthetic block proto's `capture_names`.
        if self.eager_scheds.is_empty() {
            return Err(self.err("spawn must be inside a parallel: block".to_string(), span));
        }
        let h = self.heap.alloc(Obj::Closure {
            proto,
            captured: captured.into(),
            home,
        });
        self.register_task(
            PendingCall::Call {
                callee: Value::obj(h),
                args: Vec::new(),
                span,
            },
            span,
        )
    }

    /// Register a spawned task on the innermost nursery: build the handler into a live [`Fiber`]
    /// and [`MnSched::inject`] it straight into the running sched — it runs concurrently with the
    /// rest of the body. The `task_index` is the scope's monotonic `next_index` (spawn order), so
    /// Decision-F output stays deterministic. The checker guarantees a `parallel:` is open, but we
    /// guard here anyway and return a runtime error instead of panicking.
    pub(super) fn register_task(
        &mut self,
        task: PendingCall,
        span: Span,
    ) -> Result<(), RuntimeError> {
        // The innermost open nursery; every nursery
        // a worker shell enters has its scope (`Vm::op_enter_nursery`).
        let Some(Some(scope)) = self.eager_scheds.last() else {
            return Err(self.err("spawn must be inside a parallel: block".to_string(), span));
        };
        // Inject a live fiber. Clone the sched Arc, drop the borrow so `prepare_worker` can take
        // `&mut self`; `inject_or_extend` assigns the real slot index under its lock (the `0`
        // placeholder is overwritten), so no caller-side index bookkeeping is needed.
        let sched = Arc::clone(&scope.sched);
        // TICKET-103 — every eager scope may have a later scope registered on its sched by a
        // fiber (`activate_fiber_owned_nursery`), so a spawn goes to the nursery's TAIL scope,
        // or opens a continuation scope when the tail is no longer the sched's last.
        let tail = scope.more_scopes.last().copied().unwrap_or(scope.scope);
        let fiber = self.prepare_worker(task)?.into_fiber(0, tail);
        let (opened, wids) = sched.inject_or_extend(fiber, tail);
        if let Some(n) = opened
            && let Some(Some(scope)) = self.eager_scheds.last_mut()
        {
            scope.more_scopes.push(n);
        }
        self.start_runners(&sched, wids);
        Ok(())
    }

    /// `parallel:` dedent — run the nursery's spawned tasks as cooperative fibers (B1/B2). The
    /// joining (parent) fiber is parked while the children run; a child that blocks on an empty
    /// `recv` suspends and the scheduler switches to a runnable sibling, resuming it once a sibling
    /// `send`s. A child that never blocks runs to completion before the next starts — identical to
    /// the old FIFO run-to-completion drain, so non-blocking programs are byte-for-byte unchanged.
    /// The first child fault (or `std.os.exit`) aborts the remaining siblings and propagates; on that
    /// path the parent's restored `run_until` handles `recover:`/unwind in its own context.
    /// TASK B — cancel a `parallel:` body's tasks when it escapes its `JoinNursery` early (`?` /
    /// `return` / `break` / `continue`) or when a fault unwinds past it. Pop every nursery entry ABOVE
    /// `from_len` (the level the escaping construct should restore to), innermost-first: an eager
    /// nursery's tasks are LIVE fibers, so they are cancelled + drained + flushed; a
    /// lazy nursery's entries never started, so they are simply DROPPED (no fiber to cancel, no
    /// buffered output to flush). Depth returns to `from_len` — the old `truncate`'s no-leak behavior
    /// — at every reclaim site.
    ///
    /// §2c1 — this used to write ONE observable line per lazy nursery
    /// (`"{n} pending task(s) cancelled on early exit from parallel:"`). **That report is deleted.**
    /// A task now starts at its `spawn`, so on the M:N engine there are no unstarted tasks to count,
    /// and any residual count would be racy — a task injected into the run queue may or may not have
    /// been picked up before the escape. A confident wrong number on stdout is worse than no line
    /// (`docs/contributing/gaps.md` W7-12: an uncertain verdict must decline). trio and `asyncio.TaskGroup` print
    /// nothing here either. The eager path never reported, so this also removes a pre-existing
    /// eager-vs-lazy observable split rather than creating one.
    ///
    /// TICKET-147 (W14-12) — returns the FIRST fault the aborted levels' children ended with (`None`
    /// when none faulted, or on an `os.exit`, which keeps its `pending_exit` route). A caller whose
    /// escape carries its own fault keeps that as the root cause and drops this; a cancel funnel, whose
    /// escape is only the `cancelled` sentinel, replaces the sentinel with it.
    #[inline]
    pub(super) fn drain_escaped_nursery(&mut self, from_len: usize) -> Option<RuntimeError> {
        if self.eager_scheds.len() <= from_len {
            return None; // nothing escaped past the join (e.g. normal fall-through already popped it)
        }
        self.abort_escaped_nurseries(from_len)
    }

    /// [`Vm::drain_escaped_nursery`]'s body, out of line: every return calls the check above, and
    /// this body's large stack frame would cost each of them.
    #[inline(never)]
    fn abort_escaped_nurseries(&mut self, from_len: usize) -> Option<RuntimeError> {
        let mut child_fault: Option<RuntimeError> = None;
        // Every inline abort below waits as a nursery join: one wait, with the permit given back.
        let reg = self.wait_permit(WaitSpec::Nursery);
        while self.eager_scheds.len() > from_len {
            // All three stacks pop TOGETHER, unconditionally — nursery_defer_floors, nursery_spans
            // and eager_scheds are lockstep.
            self.nursery_defer_floors.pop();
            self.nursery_spans.pop();
            let eager = self.eager_scheds.pop().flatten(); // Some if this nursery is eager
            // Per-connection spawn / §2c1 — an eager nursery's tasks are already-started live fibers:
            // cancel + drain + flush them.
            if let Some(scope) = eager {
                let e = self.abort_fiber_owned_nursery(scope);
                child_fault = child_fault.or(e);
            }
        }
        self.wait_end(reg, &[]);
        child_fault
    }

    /// TICKET-132 — before `drain_escaped_nursery` pops anything, PARK the owner on the TOP levels
    /// (from `from_len`) instead of letting their abort wait inline. Declines (`false`, no pop) at
    /// the first level with no scope — that level and every level below it still drain inline,
    /// unchanged — or when `native_reentry > 0` (the Rust
    /// stack holds the native frame, so this fiber cannot park). On park, the rewound op re-runs once
    /// `MnSched::park_join` requeues the fiber; `drain_escaped_nursery`'s re-run then finds the
    /// family settled and pops normally.
    pub(super) fn park_escaped_abort(&mut self, from_len: usize) -> bool {
        let Some(origin) = self.park_abort_origin(from_len) else {
            return false;
        };
        self.rewind_op();
        self.join_suspend = Some(origin);
        true
    }

    /// TICKET-132/TICKET-287 — THE park test of an abort: cancel the fiber-owned levels above
    /// `from_len`, top-down, and return the origin scope of the first one whose family is not done.
    /// `None` (decline) under the same conditions [`Vm::park_escaped_abort`] lists. The caller sets
    /// `join_suspend` and stores what its re-entry needs: a rewind for an escape, a
    /// [`FaultPark`](super::frames::FaultPark) for a fault unwind.
    pub(super) fn park_abort_origin(&mut self, from_len: usize) -> Option<usize> {
        if !matches!(self.block_mode(WaitSpec::Nursery), BlockMode::Park) {
            return None;
        }
        for i in (from_len..self.eager_scheds.len()).rev() {
            let Some(Some(scope)) = self.eager_scheds.get(i) else {
                return None;
            };
            cancel(
                &self.sched_registry,
                CancelTarget::Scope(&scope.sched, scope.scope),
            );
            if !scope.sched.lock().family_done(scope.scope) {
                return Some(scope.scope);
            }
        }
        None
    }

    /// THE leave of the top `parallel:` level on every exit but fall-through (TICKET-299, owner rule
    /// F2): drop the defer scopes it opened, run its body `defer`s down to its floor, then
    /// ([`Vm::level_drained`]) park or cancel and wait for its children, then `then`. Enclosing
    /// defers therefore run after the cancelled children unwind (Go cancel + `wg.Wait()`, asyncio
    /// `TaskGroup`). Fall-through (`compile_parallel`) is the one exit with the other order.
    pub(super) fn leave_level(&mut self, then: LevelThen) -> Result<(), RuntimeError> {
        let top = self.eager_scheds.len() - 1;
        let floor = self.nursery_defer_floors[top];
        self.frames
            .last_mut()
            .expect("a level has its frame")
            .defer_markers
            .truncate(floor.markers);
        self.drain_then(floor.defers, DrainCont::Level { top, then })
    }

    /// [`Vm::leave_level`]'s drain on the Rust stack, for an unwind under a synchronous native
    /// caller or one with no record to run ([`Vm::unwind_on`]); the latest fault of the level's body
    /// `defer`s.
    pub(super) fn leave_level_now(&mut self, fi: usize) -> Option<RuntimeError> {
        let floor = self.nursery_defer_floors[self.eager_scheds.len() - 1];
        self.frames[fi].defer_markers.truncate(floor.markers);
        self.drain_deferred(fi, floor.defers)
    }

    /// [`Vm::leave_level`] after level `top`'s body `defer`s ran (`err`: their latest fault).
    pub(super) fn level_drained(
        &mut self,
        top: usize,
        then: LevelThen,
        err: Option<RuntimeError>,
    ) -> Result<(), RuntimeError> {
        match then {
            LevelThen::Return { err: prior } => {
                let mut err = err.or(prior);
                if self.escape_level_parks(top, &mut err) {
                    return Ok(());
                }
                self.return_levels(err)
            }
            LevelThen::Unwind(u) => self.unwind_level_drained(*u, err),
            LevelThen::Catch(c) => self.catch_level_drained(top, *c, err),
            LevelThen::Try { span, err: prior } => {
                // A hard halt in a body defer cuts the cleanup: it unwinds, which leaves the rest.
                let mut err = match err.or(prior) {
                    Some(e) => Some(self.cleanup_halt(e)?),
                    None => None,
                };
                if self.escape_level_parks(top, &mut err) {
                    return Ok(());
                }
                self.try_levels(span, err)
            }
            LevelThen::Jump => {
                // A body defer fault is a fault at the jump: the unwind leaves the level.
                if let Some(e) = err {
                    return Err(e);
                }
                if self.park_escaped_abort(top) {
                    return Ok(());
                }
                // TICKET-147 -- the aborted level's child fault is a real fault.
                match self.drain_escaped_nursery(top) {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            }
        }
    }

    /// An escape's (`return`, `?`, TICKET-132/TICKET-287) park or abort of level `top`: `true` when
    /// it parked (the op rewinds and re-runs once the family settles), the fault in hand going with
    /// the fiber ([`Vm::store_escape_park`]); else a child fault fills a gap in `err` (TICKET-147:
    /// a body defer fault stays the root cause).
    fn escape_level_parks(&mut self, top: usize, err: &mut Option<RuntimeError>) -> bool {
        if self.park_escaped_abort(top) {
            if let Some(e) = err.take() {
                self.store_escape_park(e);
            }
            return true;
        }
        let child = self.drain_escaped_nursery(top);
        *err = err.take().or(child);
        false
    }

    /// W14-4 -- a `return` escapes every `parallel:` level above the frame's own nurseries,
    /// innermost-first ([`Vm::leave_level`]); then the rest of the return ([`Vm::return_join`]).
    /// `err` is the fault in hand.
    pub(super) fn return_levels(&mut self, err: Option<RuntimeError>) -> Result<(), RuntimeError> {
        if self.eager_scheds.len() > self.level_floor(self.frames.len() - 1) {
            return self.leave_level(LevelThen::Return { err });
        }
        self.return_join(err)
    }

    pub(super) fn join_nursery(&mut self) -> Result<(), RuntimeError> {
        // TICKET-103 — a fiber-owned nursery's owner PARKS here while its family runs, and re-runs
        // this op once `MnSched::park_join`'s wake requeues it. Checked before the first pop below,
        // which the re-run must still find. Under `native_reentry > 0` the fiber cannot park (its
        // Rust stack holds the native frame), so `join_fiber_owned_nursery` runs the loop inline.
        // Both callers stop after a park: `Op::JoinNursery` has nothing after the call, and
        // `do_return` returns early on `join_suspend`.
        if matches!(self.block_mode(WaitSpec::Nursery), BlockMode::Park)
            && let Some(Some(scope)) = self.eager_scheds.last()
            && !scope.sched.lock().family_done(scope.scope)
        {
            self.join_suspend = Some(scope.scope);
            self.rewind_op();
            return Ok(());
        }
        // Consume this nursery's tasks (FIFO). Popping the entry now (as the old drain did at the
        // end) keeps the parent's `Handler::nursery_len` accounting correct on a later fault.
        self.nursery_defer_floors.pop(); // keep the parallel floor stack in lockstep with `eager_scheds`
        // TICKET-048 — lockstep with `eager_scheds`: the span of the parallel: construct this join is
        // about.
        self.nursery_spans.pop();
        // Per-connection spawn — pop the scope in lockstep. Its tasks were injected live; its join
        // drains the handlers it spawned.
        match self.eager_scheds.pop() {
            Some(Some(scope)) => self.join_fiber_owned_nursery(scope),
            Some(None) | None => Ok(()),
        }
    }

    /// TICKET-147 — reduce an aborted (escaped) nursery's slots into the fault its children ended
    /// with. An `os.exit` sets `pending_exit` inside the reduce and is not reported here: the catch
    /// sites honor it through `pending_exit`.
    /// TICKET-195 — it reduces through [`Vm::reduce_slots`], never [`Vm::reduce_task_slots`]: this
    /// runs inside the party's OWN fault's unwind, so the child's fault must not become a cut.
    fn escape_child_fault(
        &mut self,
        slots: impl IntoIterator<Item = Option<TaskOutcome>>,
    ) -> Option<RuntimeError> {
        self.reduce_slots(slots)
            .0
            .err()
            .filter(|_| self.pending_exit.is_none())
    }

    /// The `Executor` `core`'s private [`MnSched`], with one open scope (scope 0) whose cancel
    /// parents are `ancestors`, registered with the run (TICKET-208). The one builder of a sched
    /// that is not a scope of an enclosing one.
    pub(super) fn new_detached_sched(
        &mut self,
        span: Span,
        ancestors: Vec<Arc<AtomicBool>>,
        core: &Arc<ExecutorCore>,
    ) -> EagerScope {
        let cancel = Arc::clone(&core.cancel);
        let deadlock_err = self.nursery_deadlock_err(span);
        // An Executor claims wids 1..=n (TICKET-211). `MnSched::new` allocates the per-worker local queues up front (`locals: (0..nworkers)`), so the count must
        // be sized here even though most of those workers are farmed later; an unused local queue is
        // inert.
        let nworkers = worker_count().max(1) + 1;
        let mut inner = MnSched::new(
            0,
            nworkers,
            Arc::clone(&cancel),
            deadlock_err,
            self.heap.mem_cap(),
        );
        // TICKET-099 — this sched is PRIVATE (no link to the parent). A `send`/`close` inside its
        // body must be able to wake a receiver parked on ANY other live sched of this run — the parent
        // nursery, a sibling, or a descendant — not just an ancestor, so it shares the run's registry
        // and `wake_run_wide` walks it directly rather than through a hierarchical chain.
        inner.sched_registry = Arc::clone(&self.sched_registry);
        // gaps.md W7-56 — the deadlock predicate must see this run's outstanding eager `Executor`
        // jobs (uncounted senders).
        inner.exec_registry = Arc::clone(&self.exec_registry);
        // gaps.md W7-58 — so an idle worker of this sched can JUDGE the process-wide verdict on
        // behalf of a nursery owner, which never reaches `block_halts`.
        inner.join_run(&self.quiesce);
        inner.detached = Some(Arc::downgrade(core));
        let sched = Arc::new(inner);
        self.register_sched(&sched);
        // Structured concurrency — its jobs must observe the creator's scopes' cancel too
        // (`JoinScope::ancestors`).
        sched.lock().scopes[0].ancestors = ancestors;
        // TICKET-208 — an `Executor`'s scope: no body thread and no drainer. Its submitters are
        // other parties, which the process-wide verdict counts, so scope 0 is open (it takes
        // submits until a join closes it) and never "injecting". `submit` starts the runners.
        {
            let mut c = sched.lock();
            c.scopes[0].body_open = true;
            c.leaf_site = true;
            c.exec_limit = core.limit;
        }
        self.quiesce.add_guest(&sched);
        EagerScope {
            sched,
            cancel,
            scope: 0,
            more_scopes: Vec::new(),
        }
    }

    /// TICKET-103 (W12-1) — every nursery's activation: register its scope on the fiber's OWN sched
    /// (`self.mn`). Each `spawn` then injects a
    /// live fiber that runs on that sched's existing workers, so the task starts at the `spawn` as
    /// Go's `go` does, and no thread is added (W8-8, the pool bound). The queue-at-join fallback
    /// this replaces starved any owner body that blocks on its own child before the join.
    ///
    /// No `body_open`: the owner is a counted fiber (`running`/`parked`), so `body_open` would veto
    /// the predicate while it is parked and a genuine deadlock would hang. The scope names this
    /// nursery in its deadlock fault (DEC-095). `None` only on a `Vm` that is no worker shell.
    pub(super) fn activate_fiber_owned_nursery(
        &mut self,
        nursery_span: Span,
    ) -> Option<EagerScope> {
        let sched = self.mn.clone()?;
        let cancel = Arc::new(AtomicBool::new(false));
        let scope =
            sched.register_scope_seeded(Arc::clone(&cancel), self.scope_ancestors(), Vec::new());
        {
            let mut c = sched.lock();
            c.scopes[scope].deadlock_err = Some(self.nursery_deadlock_err(nursery_span));
            // TICKET-125 — this scope's fibers can only be fed once the OWNING fiber's own join
            // returns.
            c.scopes[scope].parent_scope = self.fiber_scope;
        }
        Some(EagerScope {
            sched,
            cancel,
            scope,
            more_scopes: Vec::new(),
        })
    }

    /// TICKET-211 — THE starter of runner threads: one runner per wid [`MnSched::claim_runners`]
    /// claimed, all `SENTINEL_SCOPE` (they drain the global queue until `terminate`). Only an
    /// Executor's sched starts any: new `chezzi-exec` threads, counted in `runner_starts` and never
    /// joined. The run's root sched starts none: its runners are fixed at build
    /// ([`MnSched::mark_all_runners_claimed`]), so a wid a refused worker thread gave back stays
    /// claimed. A failed spawn gives its wid back. Returns whether any runner remains, as
    /// [`MnSched::unclaim_runner`] reports.
    pub(super) fn start_runners(&self, sched: &Arc<MnSched>, wids: Vec<usize>) -> bool {
        if wids.is_empty() || sched.detached.is_none() {
            return true;
        }
        let Some(cancel) = sched.scope_cancel(0) else {
            for wid in wids {
                sched.unclaim_runner(wid);
            }
            return false;
        };
        // An Executor's jobs run on the host's workers (Go: one pool of Ms): no thread starts and
        // the claim is given back. The enqueue that follows wakes the host ([`MnSched::wake_host`]).
        if self.quiesce.host().is_some() {
            for wid in wids {
                sched.unclaim_runner(wid);
            }
            return true;
        }
        if let Some(core) = sched.detached.as_ref().and_then(|w| w.upgrade()) {
            core.runner_starts.fetch_add(wids.len(), Ordering::Relaxed);
        }
        let mut runner = true;
        for wid in wids {
            let shell = self.spawn_shell(sched, &cancel);
            if spawn_worker_thread(shell, sched, "chezzi-exec", wid, SENTINEL_SCOPE).is_err() {
                runner = sched.unclaim_runner(wid);
            }
        }
        runner
    }

    /// TICKET-103 — `JoinNursery` for a fiber-owned nursery (every nursery). Its scopes
    /// live on this fiber's own sched, so the owner's OS thread runs them inline, scope by scope over
    /// the family.
    ///
    /// [`Vm::joiner_shell`] decides whether this thread runs the family's fibers (TICKET-245).
    /// Under `native_reentry > 0` the owner hands its runner slot to its demote replacement and
    /// only waits; the inline loop remains there only for a refused OS thread.
    ///
    /// With `native_reentry == 0` the family is complete here, because `join_nursery` parked the
    /// fiber otherwise. Under `native_reentry > 0` the fiber cannot park (its Rust stack holds the
    /// native frame, the rule `park_recv`'s caller follows), so it waits here in place.
    fn join_fiber_owned_nursery(&mut self, scope: EagerScope) -> Result<(), RuntimeError> {
        let sids = scope.sids();
        if !self.wait_family(&scope, &sids) {
            return self.run_exit_err(Span::RUNTIME).map_or(Ok(()), Err);
        }
        let r = self.reduce_task_slots(scope.sched.drain_scopes(&sids));
        for &s in sids.iter().rev() {
            scope.sched.retire_scope(s);
        }
        r
    }

    /// The inline wait of a nursery join or abort for its family `sids`. `false`: a run halt ended
    /// it ([`RunHalt::ends_join`]). The caller then leaves the scope open: a queued child still
    /// names it, and the run's end reads the finished children's output from its slots.
    ///
    /// A job fault is not due inside a `defer`, which runs whole, and the run's workers may have
    /// left. So the family's cancelled fibers run to the end and the join completes: on this
    /// thread, or, when this joiner handed its slot over ([`Vm::joiner_hands_over`]), on its
    /// replacement or one new runner.
    fn wait_family(&mut self, scope: &EagerScope, sids: &[usize]) -> bool {
        if scope.sched.lock().family_done(scope.scope) {
            return true;
        }
        let wid = self.wid;
        let mut shell = self.joiner_shell(&scope.sched, &scope.cancel, wid);
        // M1.5 — the join waits detached; an inline loop attaches its own token.
        let done = self.wait_door(&[], |_| {
            sids.iter().all(|&sid| {
                if let Some(shell) = shell.as_mut() {
                    shell.mn_worker_loop(&scope.sched, wid, sid);
                }
                scope.sched.wait_for_scope(sid)
            })
        });
        if done || self.run_halt_due() {
            return done;
        }
        // A joiner that handed its slot over must not run fibers on its stack (a buried frame). Its
        // replacement settles the family, as a halt sets no `terminate`. A host worker
        // ([`Vm::spare_host`]) leaves when the host ends, so then one new runner does. If the OS
        // refuses that thread, the loop below runs inline.
        if self.joiner_hands_over() {
            let host_spare = self.owns_nested_sched(&scope.sched) && self.spare_host().is_some();
            if !host_spare || self.start_repl_runner(&scope.sched, &scope.cancel, wid) {
                #[cfg(test)]
                HANDOVER_WAITS.fetch_add(1, Ordering::Relaxed);
                self.wait_door(&[], |_| scope.sched.wait_for_family(scope.scope));
                return true;
            }
        }
        #[cfg(test)]
        if self.joiner_hands_over() {
            INLINE_AFTER_HANDOVER.fetch_add(1, Ordering::Relaxed);
        }
        let mut shell = self.spawn_shell(&scope.sched, &scope.cancel);
        self.wait_door(&[], |_| {
            for &sid in sids {
                shell.mn_worker_loop(&scope.sched, wid, sid);
            }
        });
        scope.sched.lock().family_done(scope.scope)
    }

    /// TICKET-103/TICKET-132 — reclaim a nursery whose body ESCAPED early (`?`/`return`/`break`/
    /// `continue` or a `recover:` catch jumped past its `JoinNursery`): trip the family's
    /// one cancel token, requeue its parked and socket-parked fibers, then settle them. It waits
    /// inline ONLY where [`Vm::park_abort_origin`] declines: `native_reentry > 0` (the Rust stack holds
    /// the native frame), a nested dispatch loop, or a lower level with no scope. A fault
    /// unwind parks through [`FaultPark`](super::frames::FaultPark) (TICKET-287). The body's escape error is what propagates, so a task fault here is swallowed.
    fn abort_fiber_owned_nursery(&mut self, scope: EagerScope) -> Option<RuntimeError> {
        let sids = scope.sids();
        cancel(
            &self.sched_registry,
            CancelTarget::Scope(&scope.sched, scope.scope),
        );
        if !self.wait_family(&scope, &sids) {
            return None;
        }
        let r = self.escape_child_fault(scope.sched.drain_scopes(&sids));
        for &s in sids.iter().rev() {
            scope.sched.retire_scope(s);
        }
        r
    }

    /// D2b — build a thin host **shell** `Vm` for the M:N engine: a worker `Vm` with the nursery
    /// scheduler and the cancel token wired in. It runs no code itself — fibers swap their own
    /// heap and module roots into it ([`Vm::swap_ctx`]); the shell only provides the dispatch
    /// engine and the `mn`/`cancel` flags the `recv`/`send`/back-edge paths read.
    pub(super) fn spawn_shell(&self, sched: &Arc<MnSched>, cancel: &Arc<AtomicBool>) -> Vm {
        let mut shell = self.spawn_worker();
        shell.mn = Some(Arc::clone(sched));
        // Cancel inheritance: if this shell serves a NESTED scope (its `cancel` is not the flag this VM
        // runs under), the enclosing scopes' flags go on the chain. `run_one_fiber` re-points both per
        // fiber swap-in; this only makes the shell coherent before the first swap-in.
        shell.cancel_outer = match &self.cancel {
            Some(mine) if !Arc::ptr_eq(mine, cancel) => self.scope_ancestors(),
            _ => self.cancel_outer.clone(),
        };
        shell.cancel = Some(Arc::clone(cancel));
        // TICKET-141 (W14-14) — the replacement worker, an inline join shell and a farmed helper of a
        // gated thread are gated too; each starts without a permit (see `src/vm/width.rs`).
        shell
    }
}

impl Vm {
    /// TICKET-188 — does this party own an open nursery? The gate of the owned halt read. Every
    /// `Vm` reader that asks "which nurseries does this task have open" reads this party's
    /// `eager_scheds` through [`Vm::owned_nurseries`] or this fn. The
    /// other uses of `eager_scheds` push, pop or index it in lockstep with `nursery_spans` (enter,
    /// join, abort, `spawn` into the innermost nursery). M3.3: a box closure runs in its caller's
    /// context, so a nursery it opens is one of these.
    #[inline]
    pub(super) fn owns_nursery(&self) -> bool {
        !self.eager_scheds.is_empty()
    }

    /// TICKET-188 — THE open nurseries of this party, outermost first.
    pub(super) fn owned_nurseries(&self) -> impl DoubleEndedIterator<Item = block::OwnedRef<'_>> {
        super::open_nurseries(&self.eager_scheds)
    }
}
