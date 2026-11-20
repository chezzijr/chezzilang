//! VM: blocking waits: deadlines and halts, `wait` polling and arms.
//! Split out of `src/vm/netio.rs`. `super::*` is the parent module.

use super::core::{Pending, PendingOp, SendOutcome, Settled};
use super::deadlock::Wait;
use super::netio::CLOSED_SEND;
use super::netio::deadline_gap_wake;
use super::quiesce::RunHalt;
use super::*;

impl Vm {
    /// The `chezzi test --timeout` wall-clock halt, on its own so the ops that PARK a fiber can observe
    /// it too ([`Vm::chan_recv_step`], [`Vm::op_wait_poll`]) — a parked fiber reaches neither
    /// `jump_checked`'s loop back-edge nor [`Vm::block_halts`], so without a checkpoint at the op
    /// itself the deadline has no path to it at all (W7-17).
    ///
    /// Deliberately NOT gated on `native_reentry` (unlike the cancellation checkpoint it sits beside at
    /// those two call sites): a `--timeout` is a HARD abort that must always win, this only ever returns
    /// `Err` — it never unwinds VM state — and the demoted waits' halt read (`block_halts`) already
    /// returns this same error under a host frame. Free when the cap is off: `Instant::now()` is
    /// read only when `self.deadline` is `Some`.
    pub(super) fn deadline_halt(&self, span: Span) -> Result<(), RuntimeError> {
        if let Some(dl) = self.deadline
            && std::time::Instant::now() >= dl
        {
            return Err(super::exec::timed_out_err(self.timeout_ms, span));
        }
        Ok(())
    }

    /// W7-47 — a run-wide `os.exit` issued by SOMEBODY ELSE (typically an eager `Executor` job), for
    /// the blocking loops whose party would otherwise never learn of it. Produces verbatim the shape
    /// `reduce_task_slots` already produces for a joined child's exit (`sched.rs`): `pending_exit` set
    /// plus the `"exit"` sentinel `Err`, which unwinds past every `recover:` to the driver.
    ///
    /// The one funnel for all three causes of a run halt, in this order: an exit, a job fault
    /// (TICKET-208), then the deadlock verdict that `QuiesceState::decide` latched (TICKET-223).
    ///
    /// Deliberately does NOT set `Cut::Cancelled` — that would SWALLOW the outcome (`run_outcome`),
    /// which is the opposite of what an exit needs.
    ///
    /// Returns the error rather than a `Result` because most call sites are demote loops that must run
    /// their un-accounting (`running += 1`, `unregister_waiter`, …) BETWEEN learning
    /// of the exit and returning it, exactly as their cancel arms do.
    pub(super) fn run_exit_err(&mut self, span: Span) -> Option<RuntimeError> {
        // TICKET-213 (owner decision 5) — an exit cuts a `defer` body; a job fault does not, a
        // cleanup still runs whole for a fault. `run_halt_due` holds that split (it keeps
        // `!in_defer()` on the job-fault arm alone), so this funnel returns the exit whenever the
        // exit cell is set, inside a `defer` too. (W7-57 suppressed the exit here while a `defer`
        // ran; Go `os.Exit` and Python `os._exit` run no cleanup.)
        if !self.run_halt_due() {
            return None;
        }
        if let Some(code) = self.quiesce.pending() {
            self.pending_exit = Some(code);
            return Some(self.err("exit".to_string(), span));
        }
        // TICKET-208 — an `Executor` job hit its `--max-heap` limit: the run ends with THAT abort, and
        // no `recover:` of this party catches it (`Cut::RunFault`).
        if let Some((err, trace)) = self.quiesce.job_fault() {
            self.adopt_run_fault(trace);
            return Some(err);
        }
        // A latched deadlock verdict ends every task at its wait itself
        // ([`super::MnSched::on_deadlock`]): nothing to deliver here.
        None
    }

    /// TICKET-208 — THE predicate "a run-wide halt is pending for this party": an `os.exit` from
    /// another party, an `Executor` job's `--max-heap` abort, or a latched deadlock verdict
    /// (TICKET-223). A CPU-side or in-place checkpoint reads this (behind the lock-free
    /// `run_halt_hint`); it never reads the exit, job-fault or deadlock cell by itself. An exit and
    /// a deadlock are due inside a `defer` too (TICKET-213, TICKET-152); a job fault waits for the
    /// `defer` to end. Which kinds cut a `defer` is `RunHalt::cuts_cleanup`; a latched verdict is
    /// due inside a `defer` even while a job fault waits.
    pub(super) fn run_halt_due(&self) -> bool {
        let halt = self.quiesce.run_halt();
        halt.cuts_cleanup() || (halt == RunHalt::Fault && !self.in_defer())
    }

    /// The halts of a party that comes back from a wait it could not poll (a nursery join, an
    /// in-place blocking native): a cancel or a child fault, then the run-wide halt. No
    /// `--timeout` rung: at a return that would turn a post-hoc timeout into an abort.
    pub(super) fn resume_halts(&mut self, span: Span) -> Result<(), RuntimeError> {
        self.halts_after(span, false)
    }

    /// [`Vm::resume_halts`] at the return of a call that held its thread (`Vm::wait_call`): the
    /// run-wide rung is read behind `run_halt_hint`, as a function entry and a loop back-edge read
    /// it. Every writer of a run-wide halt latches the hint right after its cell (`Vm::halt_run`
    /// after an exit or a job abort, before the scheds are halted; `QuiesceState::decide` with a
    /// verdict), so a halt this read misses lands at the next function entry. A short C call cannot
    /// afford the three cell locks.
    pub(super) fn return_halts(&mut self, span: Span) -> Result<(), RuntimeError> {
        self.halts_after(span, true)
    }

    fn halts_after(&mut self, span: Span, hinted: bool) -> Result<(), RuntimeError> {
        let read = self.halt_requested();
        if hinted && read.is_none() && !self.quiesce.run_halt_hint() {
            return Ok(());
        }
        self.read_halts(read, span).map_or(Ok(()), Err)
    }

    /// TICKET-200 — the one list of halts a party blocked in place observes, read with no lock held:
    /// every in-place wait loop calls it each tick, and [`Vm::guard_free_then_take`] reads it when
    /// the guard comes free. A new wait loop calls it; it never copies a rung.
    ///
    /// TICKET-223 — it has no deadlock text of its own: a positive verdict is latched by
    /// `QuiesceState::decide` and delivered by [`Vm::run_exit_err`] as one of its three causes (an
    /// exit, a job fault, the verdict), with the report of the party site the latch recorded. An
    /// in-place wait is awake and judges nothing: the run's awake count decides a deadlock
    /// ([`MnSched::on_deadlock`]).
    pub(super) fn block_halts(&mut self, span: Span) -> Result<(), RuntimeError> {
        // Checked HERE because a blocked job never reaches `jump_checked`'s loop back-edge, which is
        // where every other path observes the deadline. Without it `chezzi test --timeout` could not
        // kill a job blocked forever on a channel — exactly the hang eager execution makes easier to
        // write. No `back_edge_tick` throttle: this runs once per `DEMOTE_POLL_BACKOFF`, not per op.
        // `cancel`'s cooperative stop (D4), an enclosing scope's cancel and (TICKET-188) a child
        // fault of a nursery this party owns arrive first; then W7-47, the run-wide halt (an
        // `os.exit` or a job fault from another party). BELOW cancel, so a party that already holds
        // a cancel flag keeps unwinding as `Cancelled` (only a party with no cancel flag at all —
        // precisely top-level `main` — reaches that rung). ABOVE the deadlock verdict, because an
        // `Exit` outranks a synthesized `Deadlocked` — the precedence `reduce_task_slots` encodes.
        let read = self.halt_requested();
        if let Some(e) = self.blocked_halt(read, span) {
            return Err(e);
        }
        // TICKET-062 (W10-16) / TICKET-096 — a child fault recorded while the exit rung ran (DEC-134:
        // read again below the exit, and again after a positive verdict).
        if let Some(e) = self.take_halt(span) {
            return Err(e);
        }
        Ok(())
    }

    /// `wait:` runtime (§6d) — execute [`Op::WaitPoll`]. The `n` arm channel handles are on the
    /// operand stack (`stack[base..base+n]`, source order). Poll source order: the first channel with
    /// a queued value (or a fired timer) wins → drop the handles, push `?value`, jump to that arm's
    /// body. A closed+empty recv arm is ready too and pushes `!RecvError` (D2). Nothing ready → run
    /// `else` (jump), else block: a snapshot-park, or a demote under a host frame. A live timer arm
    /// is just another arm on both.
    ///
    /// TICKET-185 — a `wait:` that blocks publishes ONE [`Pending`] for all its arms: an OFFER per
    /// send arm and (rendezvous only) a SLOT per recv arm. The CAS that takes an offer or fills a
    /// slot also decides the arm, so no `wait:` ever fires two arms. Every re-run of this op settles
    /// that `Pending` FIRST — before the deadline and cancel checkpoints — and only then polls.
    pub(super) fn op_wait_poll(&mut self, meta: &WaitMeta, span: Span) -> Result<(), RuntimeError> {
        let n = meta.n;
        // Per-arm stack width: a recv arm holds ONE slot (the channel handle), a SEND arm holds TWO
        // (channel THEN value). Walk a running `off` cursor from `base` so send arms are read correctly.
        let slot_width = |is_send: bool| if is_send { 2 } else { 1 };
        let total: usize = meta.is_send.iter().map(|&s| slot_width(s)).sum();
        let base = self.stack.len() - total;
        if let Some(op) = self.pending.take() {
            let settled = op.settle();
            if let Some(r) = self.wait_settled(settled, base, meta, span) {
                return r;
            }
        }
        // W7-17 — `--timeout` above the cancellation checkpoint and suppressed inside a `defer`, for
        // exactly the reasons `chan_recv_step` documents at the same seam: the deadline outranks a
        // cancel (and ending a timer arm early trips this fiber's cancel to close the park gap), while
        // a cleanup body's already-satisfiable `wait:` must still complete.
        if !self.in_defer() {
            self.deadline_halt(span)?;
        }
        let mut soonest: Option<(usize, std::time::Instant)> = None;
        let mut any_on = false;
        let mut off = 0usize;
        for i in 0..n {
            let slot = base + off;
            off += slot_width(meta.is_send[i]);
            // A non-object slot is the off sentinel `compile_wait` pushes for a false guard.
            let Some(h) = self.stack[slot].as_obj() else {
                continue;
            };
            any_on = true;
            let core = self.channel_core(h);
            if meta.is_send[i] {
                // SEND arm — `send`'s own decision (`Vm::send_commit`, no offer): the value moves
                // (buffer room, a parked receiver's slot, unbounded) → take the arm; a closed channel
                // is READY-and-FAULTS (Go's panic-on-send-to-closed; the exact bare-`send` message),
                // SELECTED not skipped — first-ready wins in source order; otherwise not ready.
                let val = self.stack[slot + 1];
                match self.send_commit(h, &core, val, None) {
                    SendOutcome::Sent => {
                        self.take_wait_send_arm(base, meta.arm_targets[i]);
                        return Ok(());
                    }
                    SendOutcome::Closed => return Err(self.err(CLOSED_SEND.to_string(), span)),
                    SendOutcome::Full | SendOutcome::Offered => continue,
                }
            }
            let ready = self.recv_ready(&core, &mut core.q.lock().unwrap());
            match ready {
                core::RecvReady::Value(v) => {
                    self.wake_senders(h); // a `wait:` arm freed a slot — wake a parked bounded sender
                    self.take_wait_arm(base, Some(v), meta.arm_targets[i]);
                    return Ok(());
                }
                core::RecvReady::Fired => {
                    self.take_wait_arm(base, Some(Value::bool(true)), meta.arm_targets[i]);
                    return Ok(());
                }
                // D2 — closed and drained: the arm is ready and binds `!RecvError`, as a bare `recv`
                // returns (Go's `select` picks `<-closed`; crossbeam gives `Err(RecvError)`).
                core::RecvReady::Closed => {
                    self.take_wait_arm(base, None, meta.arm_targets[i]);
                    return Ok(());
                }
                core::RecvReady::Wait => {
                    if let Some(deadline) = core.timer
                        && soonest.is_none_or(|(_, d)| deadline < d)
                    {
                        soonest = Some((i, deadline));
                    }
                }
            }
        }
        // Nothing ready → the non-blocking `else` fallback.
        if let Some(t) = meta.else_target {
            self.stack.truncate(base);
            self.frames.last_mut().unwrap().ip = t;
            return Ok(());
        }
        // TICKET-194 — a ready arm or an `else` never waits, so neither is a cancellation point.
        self.wait_halt(span)?;
        // D5 — every arm off and no `else`: guards are checked once, so nothing can ever make this
        // `wait:` ready. Fault before any park.
        if !any_on {
            return Err(self.err(
                "wait: every arm is off (its guard is false) and there is no else".to_string(),
                span,
            ));
        }
        // Block on all live arms. The arm operands are on the stack (they root the channels + re-supply
        // the poll on resume). `keys` = (SOURCE arm index, stack slot, channel handle, is_send) per
        // ENABLED arm, in arm order: an off arm is left out, so an entry's position is not its arm
        // index — every offer, slot and `Settled` index is the source one (`meta.arm_targets[i]`).
        let keys: Vec<(u32, usize, GcRef, bool)> = {
            let mut v = Vec::with_capacity(n);
            let mut off = 0usize;
            for (i, &is_send) in meta.is_send.iter().enumerate() {
                let slot = base + off;
                off += slot_width(is_send);
                if let Some(h) = self.stack[slot].as_obj() {
                    v.push((i as u32, slot, h, is_send));
                }
            }
            v
        };
        let has_send = keys.iter().any(|&(.., is_send)| is_send);
        let wait_spec = WaitSpec::Wait {
            deadline: soonest.is_some(),
            has_send,
        };
        let wait_mode = self.block_mode(wait_spec);
        if wait_mode == BlockMode::Park {
            // W7-17 — the ungated park checkpoint (see `chan_recv_step`'s): everything above settled
            // without blocking, so a hard abort had no business preempting it, but a PARK past the
            // deadline reaches no back-edge and no `block_halts` and would hang — including inside
            // a `defer`, where the top-of-fn check is suppressed.
            self.deadline_halt(span)?;
        }
        // Publish, under ONE `Pending`: an offer per send arm, and — for every mode but `Park`,
        // whose slots `MnSched::park_wait` publishes under the sched lock — a slot per rendezvous recv
        // arm. An arm that became ready since the poll stops the publishing: settle and poll again.
        let p = Pending::new();
        let mut op = PendingOp::new(Arc::clone(&p), Vec::with_capacity(n));
        let mut offered: Vec<GcRef> = Vec::new();
        let mut ready = false;
        for &(i, slot, h, is_send) in &keys {
            let core = self.channel_core(h);
            if is_send {
                let val = self.stack[slot + 1];
                let mut g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                if g.closed || g.send_ready_for(core.cap, Some(&p)) {
                    ready = true;
                    break;
                }
                g.offer(&p, i, val);
                drop(g);
                offered.push(h);
            } else if wait_mode != BlockMode::Park {
                let mut g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                if g.closed || g.recv_ready_for(Some(&p)) || core.done_latch.load(Ordering::Relaxed)
                {
                    ready = true;
                    break;
                }
                if core.cap == Some(0) {
                    g.slot(&p, i);
                }
            }
            op.at.push((core, i, is_send));
        }
        if ready {
            let settled = op.settle();
            return self
                .wait_settled(settled, base, meta, span)
                .unwrap_or_else(|| {
                    self.rewind_op(); // re-run this WaitPoll: it polls again
                    Ok(())
                });
        }
        // A published offer wakes no parked fiber (see `chan_send_step`): a parked cap-0 receiver
        // holds a live slot, so the failed `give` proves none can take it. Only a party blocking in
        // place re-checks a predicate, and the channel's condvar reaches it.
        for h in offered {
            self.channel_core(h).wake_all();
        }
        // M:N (`--parallel`) snapshot-park, top level: rewind to re-run `WaitPoll` on wake and set
        // `wait_suspend`; the worker loop captures each arm's (key, core) WHILE the fiber heap is live
        // (`Disp::WaitPark`) and `MnSched::park_wait` files ONE shared token in every arm bucket. A
        // `send`/`close` to any arm claims the fiber once and sweeps the rest (lost-wakeup-safe via the
        // park-gap re-check). Mirrors the single-`recv` `park_recv`/`Disp::Park` path, generalized to N.
        if wait_mode == BlockMode::Park {
            // WAIT-1 fix — a live timer arm is NOT taken by an inline-sleep (which would pin the worker
            // and strand a sibling `send` that lands mid-window). Instead, for the soonest timer arm
            // submit ONE background `send_wake(true)` at its deadline (in THIS scheduler) and fall
            // through to the snapshot-park, so the timer channel parks as an ordinary arm bucket. On
            // wake the re-poll pops a sibling's value (timer NOT taken) OR finds `now >= deadline` and
            // takes the timer arm. The existing `WaitPark` claimed-CAS sweep guarantees exactly one of
            // {a sibling send/close, the timer's own deadline send} wins (WAIT-2 late-alarm = CAS
            // already claimed = no-op; WAIT-3 same-instant = single claimed CAS = one winner).
            if let Some((i, deadline)) = soonest {
                // A fiber about to be cancelled must not arm a stray timer — the top-of-fn
                // cancellation checkpoint already returned in that case.
                let sched = self.mn.clone().unwrap();
                // `i` is the timer's SOURCE arm index; an off arm is never `soonest`.
                let &(_, _, h, _) = keys
                    .iter()
                    .find(|k| k.0 as usize == i)
                    .expect("the timer arm is enabled");
                let key = self.channel_core_ptr(h);
                let core_job = self.channel_core(h);
                let sched_job = Arc::clone(&sched);
                // The timer is a task of the run's count while this fiber waits on it: on every
                // park, a re-park too, before the job below can fire.
                self.timer_wait = Some(core_job.timer_wait(&sched.quiesce.awake));
                // Arm ONCE per timer channel: a re-park of this same wait (woken with no consumable
                // value — e.g. a sibling `close` on another arm) re-runs WaitPoll and re-enters this
                // block, but the CAS fails the second time so we do NOT submit a redundant job. The
                // first job survives the re-park (it captures the stable `key`+`core` and wakes
                // whatever token is in this bucket at the deadline). Fresh `timer(ms)` ⇒ fresh core
                // ⇒ `armed=false`, so no reset is needed.
                if core_job
                    .timer_armed
                    .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
                {
                    // Account the pending timer `inflight` (gated STRICTLY on `soonest.is_some()`) so it
                    // vetoes the deadlock predicate while a lone fiber waits; the job un-accounts it.
                    sched.inflight.fetch_add(1, Ordering::Relaxed);
                    // W7-17 — same clamp as `chan_recv_step`'s timer park: fire at the SOONER of this
                    // arm's deadline and the run's `--timeout`, and deliver `true` ONLY if the arm's own
                    // deadline really passed. An early fire requeues the `WaitPark` token with nothing
                    // consumable — exactly the "woken with no consumable value" case the arm-once CAS
                    // above already tolerates — and the re-poll faults at the top-of-fn checkpoint. It
                    // is `deadline_gap_wake`, not a bare wake, because this job can fire BEFORE the
                    // fiber is parked and the CAS then forbids a second arm; see its doc. With
                    // `--timeout` off this is the plain `submit_at(deadline, …)`.
                    let fire_at = self.deadline.map_or(deadline, |rd| deadline.min(rd));
                    let gap_cancel = self.cancel.clone();
                    timer::submit_at(
                        fire_at,
                        Box::new(move || {
                            // Only the timer's own deadline fires it. An early wake (the run's
                            // `--timeout` came first) leaves it pending: the woken task's resume
                            // ends its wait, and a later wait on this timer counts it again.
                            let fired = std::time::Instant::now() >= deadline;
                            if fired {
                                sched_job.send_wake(key, &core_job, Value::bool(true));
                            } else {
                                deadline_gap_wake(&sched_job, key, &core_job, &gap_cancel);
                            }
                            sched_job.leave_inflight();
                            if fired && core_job.timer_fired(&sched_job.quiesce.awake) {
                                sched_job.on_deadlock();
                            }
                        }),
                    );
                }
            }
            self.pending = Some(op);
            self.rewind_op(); // re-run this WaitPoll on resume
            self.park_site = Some((Wait::select(has_send).phrase(), span));
            self.wait_suspend = Some(keys.iter().map(|&(i, _, h, s)| (i, h, s)).collect());
            return Ok(());
        }
        let arms: Vec<(Arc<ChannelCore>, bool)> = keys
            .iter()
            .map(|&(_, _, h, is_send)| (self.channel_core(h), is_send))
            .collect();
        // Under a host frame (`BlockMode::Demote`, the only other `Wait` cell): a host-stack loop
        // frame sits between the worker loop and here, so we cannot snapshot-park. Demote: block
        // this worker in place until an arm may be ready (its offers and slots stay published),
        // then REWIND so this op settles and re-polls. A live timer arm (`soonest`) bounds the wait
        // (so a real send still beats the timer on the re-poll). Lower throughput but sound — the
        // documented v1 limit (§6d).
        let latch = self.halt_latch();
        if let Err(e) = self.demote_wait_block(arms, &p, soonest, span) {
            let settled = op.settle();
            let settled = self.wait_settled(settled, base, meta, span);
            return self.settled_over_halt(latch, settled, Err(e));
        }
        self.pending = Some(op);
        self.rewind_op();
        Ok(())
    }

    /// TICKET-185 — act on a `wait:`'s settled [`Pending`]: `Some` when it decided the `wait:` (an
    /// offer was taken, a slot was filled, or a closed send arm faults), `None` when it was
    /// cancelled and the arms must be polled again.
    fn wait_settled(
        &mut self,
        settled: Settled,
        base: usize,
        meta: &WaitMeta,
        span: Span,
    ) -> Option<Result<(), RuntimeError>> {
        match settled {
            Settled::Sent(i) => {
                self.take_wait_send_arm(base, meta.arm_targets[i as usize]);
                Some(Ok(()))
            }
            Settled::Got(i, v) => {
                self.take_wait_arm(base, Some(v), meta.arm_targets[i as usize]);
                Some(Ok(()))
            }
            Settled::Closed => Some(Err(self.err(CLOSED_SEND.to_string(), span))),
            Settled::Cancelled => None,
        }
    }

    /// Commit a chosen `wait` arm: drop the `n` channel handles (`stack[base..]`), push what the
    /// receive gives (`T!RecvError`, the same value a bare `recv` returns: `got` is `None` for a
    /// closed, drained arm), and jump to the arm body's target ip (the bind/assign/discard prologue).
    pub(super) fn take_wait_arm(&mut self, base: usize, got: Option<Value>, target: usize) {
        let value = self.recv_result(got);
        self.stack.truncate(base);
        self.push(value);
        self.frames.last_mut().unwrap().ip = target;
    }

    /// Commit a chosen SEND `wait` arm: drop all arm operands (`stack[base..]`, including this arm's
    /// value, already enqueued by the poll) and jump to the arm body — which binds NOTHING, so unlike
    /// [`Vm::take_wait_arm`] this pushes no value.
    pub(super) fn take_wait_send_arm(&mut self, base: usize, target: usize) {
        self.stack.truncate(base);
        self.frames.last_mut().unwrap().ip = target;
    }
}
