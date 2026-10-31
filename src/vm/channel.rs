//! VM: channel methods, send and recv steps, sender and receiver wakes and parks.
//! Split out of `src/vm/netio.rs`. `super::*` is the parent module.

use super::core::{ChanState, Pending, PendingOp, RecvReady, SendOutcome, Settled};
use super::netio::CLOSED_RECV;
use super::netio::CLOSED_SEND;
use super::netio::EMPTY_RECV_DEADLOCK;
use super::netio::FULL_SEND_DEADLOCK;
use super::netio::RENDEZVOUS_SEND_DEADLOCK;
use super::netio::deadline_gap_wake;
use super::netio::send_deadlock_msg;
use super::*;
use crate::base::builtin_method::ChannelMethod;

impl Vm {
    /// The wake fan-out of a `close()`, a `trip()` and a seal (TICKET-219): wake EVERY receiver of
    /// channel `h`, parked or demoted, so each re-runs and observes the new state. Same routing as
    /// `send_commit`: an inline outermost-`parallel:` builder VM (`self.mn == None`) wakes
    /// enlisted, parked receivers via the held `mn_enlist_sched`, not just the local condvar
    /// (cross-nursery flat scheduler #2).
    pub(super) fn wake_receivers_all(&mut self, h: GcRef, core: &Arc<ChannelCore>) {
        if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
            let key = self.channel_core_ptr(h);
            sched.close_wake(key, core);
        } else {
            // Wake any demoted OS thread blocked on this core's condvar (in-callback recv).
            core.wake_all();
            // Cooperative engine: re-add every sibling fiber parked on this channel's `recv`.
            self.wake_on_send(h);
        }
    }

    /// `Channel[T]` methods (C2/C4): `send` (the value itself: one word, M3.2), `recv` (FIFO;
    /// empty = wait or a deadlock fault), `len`.
    pub(super) fn channel_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let Some(which) = ChannelMethod::from_name(method) else {
            return Err(self.err(format!("type Channel has no method '{method}'"), span));
        };
        match which {
            ChannelMethod::Send => {
                self.arity_err("send", args, 1, span)?;
                // TICKET-185 — `closed` is judged ONCE, inside `chan_send_step`'s commit (a fresh
                // send) or by the offer's own `Pending` (a parked one). Unbounded: enqueue
                // immediately. Bounded or rendezvous with no taker: publish an offer and block —
                // park the fiber (`SendStep::Parked` — the receiver+value were re-rooted), block in
                // place, demote, or, in a context that cannot block, fault. On park,
                // `do_method_call` skips the result push.
                match self.chan_send_step(h, args[0], span)? {
                    SendStep::Sent | SendStep::Parked => Ok(Value::nil()),
                }
            }
            // `try_send` is the non-blocking partner of `send`: it never parks. It returns `false` when
            // the send can't proceed — the channel is CLOSED, a BOUNDED channel is FULL (queue at
            // capacity), or a rendezvous channel has no parked receiver — and `true` once the value
            // moved. (An unbounded channel is never full.) The decision is `send`'s own
            // (`Vm::send_commit`), with no offer: a `try_send` never waits for a taker.
            // NOTE: the full-vs-not decision on a bounded channel under multi-sender contention is the
            // SAME nondeterminism class as `try_recv` returning `None`-vs-`Some` under contention.
            ChannelMethod::TrySend => {
                self.arity_err("try_send", args, 1, span)?;
                let core = self.channel_core(h);
                let sent = matches!(self.send_commit(h, &core, args[0], None), SendOutcome::Sent);
                Ok(Value::bool(sent))
            }
            ChannelMethod::Recv => {
                self.arity_err("recv", args, 0, span)?;
                // D5 owe #3 (Path C) — a `recv` reached INSIDE a native callback on the M:N engine
                // can't snapshot-park, so `recv_step_or_demote` DEMOTES the worker thread: block in
                // place on the channel condvar + spin a replacement, resuming on a sibling `send`
                // (Go's `handoffp`). `demote_recv_block` is itself closed-aware (a `close` faults the
                // demoted recv).
                match self.recv_step_or_demote(h, span)? {
                    // `recv_step_or_demote` woke a sender for `Got`; a filled slot committed none.
                    RecvStep::Got(v) | RecvStep::Filled(v) => Ok(v),
                    // `chan_recv_step` already re-rooted the receiver + set `suspend`; the sentinel is
                    // never observed (`do_method_call` gates the result-push on `suspend`).
                    RecvStep::Parked => Ok(Value::nil()),
                    // Closed-and-drained: a distinct fault (not the deadlock fault) — no producer left.
                    RecvStep::ClosedEmpty => Err(self.err(CLOSED_RECV.to_string(), span)),
                }
            }
            ChannelMethod::TryRecv => {
                // A1: non-blocking poll. Unlike `recv` it never touches the scheduler /
                // `native_reentry` / `suspend` / `ip` — it always returns immediately with an
                // `Option`: `Some(v)` if queued, `None` if empty.
                self.arity_err("try_recv", args, 0, span)?;
                let core = self.channel_core(h);
                // A `timer(ms)` channel reports ready (`Some(true)`) once its deadline has passed, even
                // with nothing queued — the level-triggered, non-blocking poll (used by `wait`'s
                // source-order scan and the `else` arm). `--parallel` may also have a real `true`
                // queued by the background send; either way `Some(true)`.
                // A tripped latch (`trip()`) reports ready forever, like a passed timer deadline.
                let ready = self.recv_ready(&core, &mut core.q.lock().unwrap());
                let got = match ready {
                    RecvReady::Value(v) => {
                        self.wake_senders(h); // a real pop freed a slot — wake a parked bounded sender
                        Some(v)
                    }
                    RecvReady::Fired => Some(Value::bool(true)),
                    RecvReady::Closed | RecvReady::Wait => None,
                };
                Ok(match got {
                    Some(v) => self.alloc_enum("Option", "Some", &[v]),
                    None => self.alloc_enum("Option", "None", &[]),
                })
            }
            // `close()` marks the channel closed (idempotent) and wakes every parked / demoted
            // receiver so each re-runs and observes the close: a `for v in ch:` ends, a bare `recv`
            // faults. Mirrors `send`'s wake fan-out but delivers no value.
            ChannelMethod::Close => {
                self.arity_err("close", args, 0, span)?;
                let core = self.channel_core(h);
                // TICKET-185 — close every parked sender's offer in the same hold that sets
                // `closed` (Go: a parked sender's value is NOT delivered once the channel is closed;
                // the woken sender settles `Closed` and faults `send on a closed channel`). A value
                // already committed to a receiver's slot stays delivered.
                core.q.lock().unwrap().close();
                self.wake_receivers_all(h, &core);
                Ok(Value::nil())
            }
            // `trip()` flips the manual level-trigger latch (the primitive behind `std.cancel`'s
            // `done()`): the channel is then permanently ready (`recv`/`try_recv`/`wait` yield `true`).
            // Idempotent. Reuses `close()`'s exact wake fan-out so a parked `recv`/`wait` re-runs and
            // observes the latch — but does NOT set `closed` (a closed+empty `wait` arm is *skipped*;
            // we need it *ready*).
            ChannelMethod::Trip => {
                self.arity_err("trip", args, 0, span)?;
                let core = self.channel_core(h);
                // W7-13r(b) — the store happens UNDER `core.q`, the same lock a blocked waiter
                // re-checks its readiness predicate under, and that is what makes the wake reliable.
                // `close()` has always set `closed` under this lock; `trip()` used a bare atomic, so a
                // waiter could evaluate "not tripped" while holding `q` and be notified before it had
                // atomically released `q` and enqueued on `cv` — a lost wakeup costing a full
                // `DEMOTE_POLL_BACKOFF`, the exact shape W7-13 fixed for values and closes. Holding
                // `q` across the store makes the two orderings the only possibilities: the waiter sees
                // the latch in its predicate, or it is already on the condvar when `notify_all` runs.
                // The guard is dropped before the wake fan-out below, which takes the sched lock —
                // `q` is never held across that.
                {
                    let _g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                    core.done_latch.store(true, Ordering::Relaxed);
                }
                self.wake_receivers_all(h, &core);
                Ok(Value::nil())
            }
            ChannelMethod::Len => {
                self.arity_err("len", args, 0, span)?;
                // The buffer only (TICKET-185): a parked sender's offer is not buffered — Go
                // measures `len: 0` on an unbuffered channel with a parked sender.
                let n = self.channel_core(h).q.lock().unwrap().len();
                Ok(Value::int(n as i64))
            }
            // `cap()` reports the channel's capacity: `-1` for unbounded `Channel[T]()`, `0` for
            // rendezvous `Channel[T](0)`, `n` for bounded `Channel[T](n)`.
            // A capacity above 2^62 boxes as `Obj::BigInt` (`Value::int` would wrap).
            ChannelMethod::Cap => {
                self.arity_err("cap", args, 0, span)?;
                let cap = self.channel_core(h).cap.map_or(-1i64, |c| c as i64);
                Ok(self.make_int(cap))
            }
        }
    }

    /// One `send` step for [`Vm::channel_method`]'s `send` (TICKET-185: one transfer protocol).
    /// [`Vm::send_commit`] decides in one `core.q` hold: closed faults, a free buffer place or a
    /// parked receiver's slot takes the value, and otherwise the value becomes this send's OFFER.
    /// An offered send then BLOCKS until a receiver takes the offer (`Sent`) or `close()` closes it
    /// (`CLOSED_SEND`): it parks the fiber, blocks in place (a party that owns its OS thread),
    /// demotes (inside a native callback), or — in a context that cannot block at all (the inline
    /// outermost-`parallel:` builder) — publishes no offer and faults with the shared deadlock
    /// message. A parked send re-runs this fn on wake and settles its offer first.
    pub(super) fn chan_send_step(
        &mut self,
        h: GcRef,
        v: Value,
        span: Span,
    ) -> Result<SendStep, RuntimeError> {
        let core = self.channel_core(h);
        // A re-run after a park: settle the offer this `send` published before it parked, BEFORE
        // the cancel checkpoint — a taken offer means the `send` happened.
        if let Some(op) = self.pending.take() {
            if op.p.is_queued()
                && self.deadline_halt(span).is_ok()
                && !(self.native_reentry == 0 && self.halt_requested().is_some())
            {
                // A stray wake: still queued — re-park on the same offer.
                self.pending = Some(op);
                self.park_send(h, v, core.cap, span);
                return Ok(SendStep::Parked);
            }
            if let Some(r) = self.send_settled(op, span) {
                return r;
            }
            self.deadline_halt(span)?;
        }
        if core.cap.is_none() {
            // Unbounded — never blocks.
            return match self.send_commit(h, &core, v, None) {
                SendOutcome::Closed => Err(self.err(CLOSED_SEND.to_string(), span)),
                SendOutcome::Sent | SendOutcome::Offered | SendOutcome::Full => Ok(SendStep::Sent),
            };
        }
        // The table decides how this send blocks; a `Refuse` context publishes no offer.
        let mode = self.block_mode(WaitSpec::Send);
        let p = (mode != BlockMode::Refuse).then(Pending::new);
        match self.send_commit(h, &core, v, p.as_ref().map(|p| (p, 0))) {
            SendOutcome::Sent => return Ok(SendStep::Sent),
            SendOutcome::Closed => return Err(self.err(CLOSED_SEND.to_string(), span)),
            SendOutcome::Full => {
                self.wait_halt(span)?;
                return Err(self.err(send_deadlock_msg(core.cap).to_string(), span));
            }
            SendOutcome::Offered => {}
        }
        let op = PendingOp::new(p.unwrap(), vec![(Arc::clone(&core), 0, true)]);
        // A send with room or a waiting receiver committed above and is never cut. A halt here
        // withdraws the offer, unless a receiver already took it (DEC-185).
        if let Err(e) = self.wait_halt(span) {
            return self.send_settled(op, span).unwrap_or(Err(e));
        }
        // An `Offered` send wakes no parked fiber: every parked cap-0 receiver holds a live slot,
        // published in the same `core.q` hold as its "not ready" check, so the failed `give` proves
        // no parked fiber can take this offer. Only a party blocking in place (or demoted) re-checks
        // a predicate, and the channel's condvar reaches it.
        core.wake_all();
        if mode == BlockMode::Park {
            // A real M:N WORKER snapshot-parks: the worker loop drives `send_suspend` →
            // `Disp::SendPark`.
            self.pending = Some(op);
            self.park_send(h, v, core.cap, span);
            return Ok(SendStep::Parked);
        }
        if mode == BlockMode::Demote {
            return self.demote_send_block(h, core, op, span);
        }
        self.block_send(h, &core, op, span)
    }

    /// A party that owns its OS thread — an eager `Executor` job, or the top-level `main` thread —
    /// blocks until its offer settles, the mirror of the empty-`recv` case in `chan_recv_step`
    /// ([`Vm::block_wait_tick`]). The party registration is scoped to the WAIT only — see the same
    /// rule spelled out in [`Vm::block_recv`]. M3.2: `h` is pinned across the wait, so the
    /// collector reads the offer's value through it.
    fn block_send(
        &mut self,
        h: GcRef,
        core: &Arc<ChannelCore>,
        op: PendingOp,
        span: Span,
    ) -> Result<SendStep, RuntimeError> {
        let msg = if core.cap == Some(0) {
            RENDEZVOUS_SEND_DEADLOCK
        } else {
            FULL_SEND_DEADLOCK
        };
        let p = Arc::clone(&op.p);
        let mut r = Ok(());
        while p.is_queued() {
            let on = Blocked::at(quiesce::PartyWait::Send(Arc::clone(&p)), msg, span);
            r = self.block_wait_tick(WaitSpec::Send, on, h, core, span, |_| !p.is_queued());
            if r.is_err() {
                break;
            }
        }
        self.send_settled(op, span)
            .unwrap_or_else(|| r.map(|()| SendStep::Sent))
    }

    /// TICKET-194 — THE mapping from a send's settled offer to its result. A taken offer means the
    /// `send` happened; a closed one faults `CLOSED_SEND` (Go: `send on closed channel`). `None`:
    /// the offer was withdrawn untaken, so the caller's own outcome (a halt, a re-poll) stands.
    pub(super) fn send_settled(
        &self,
        op: PendingOp,
        span: Span,
    ) -> Option<Result<SendStep, RuntimeError>> {
        match op.settle() {
            Settled::Sent(_) => Some(Ok(SendStep::Sent)),
            Settled::Closed => Some(Err(self.err(CLOSED_SEND.to_string(), span))),
            Settled::Cancelled | Settled::Got(..) => None,
        }
    }

    /// TICKET-185 — the ONE `send` decision for every sender: [`ChanState::send`] (closed / buffer /
    /// give to a slot / offer / full), made atomically. M:N / inline-enlist: under the sched lock
    /// ([`MnSched::send_commit`], which also wakes the receivers when the value moved). No scheduler
    /// in scope: under `core.q`, then the condvar + [`Vm::wake_on_send`]. The INLINE outermost-
    /// `parallel:` builder VM runs with `self.mn == None` but holds the global sched in
    /// `self.mn_enlist_sched`; its send must still wake an enlisted, parked receiver (the
    /// cross-nursery wake), so it routes through that sched.
    pub(super) fn send_commit(
        &mut self,
        h: GcRef,
        core: &Arc<ChannelCore>,
        v: Value,
        offer: Option<(&Arc<Pending>, u32)>,
    ) -> SendOutcome {
        if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
            let key = self.channel_core_ptr(h);
            // TICKET-185 — a give hands its receiver to this worker's `runnext`. A builder VM
            // (`mn == None`, enlisted) owns no worker slot, so it passes an out-of-range `wid`
            // and the hand-off falls back to the broadcast.
            let wid = if self.mn.is_some() {
                self.wid
            } else {
                usize::MAX
            };
            return sched.send_commit(key, core, v, offer, wid);
        }
        let out = core
            .q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(core.cap, v, offer);
        if matches!(out, SendOutcome::Sent) {
            core.wake_all();
            self.wake_on_send(h); // wake a receiver parked on this channel's `recv`
        }
        out
    }

    /// Park the running fiber on a full bounded `send`: re-root the receiver AND the value argument on
    /// the operand stack (send is 1-arg, unlike recv's 0-arg — both must be re-pushed or the rewound
    /// `CallMethod(send)` mis-reads the stack), rewind `ip` so the send re-executes on resume, and set
    /// the `send_suspend` sentinel. The scheduler / worker loop files the fiber into the channel's wait
    /// set; a sibling `recv` freeing a slot ([`Vm::wake_senders`]) wakes it. The re-run settles the
    /// offer the value already sits in.
    pub(super) fn park_send(&mut self, h: GcRef, value: Value, cap: Option<usize>, span: Span) {
        self.push(Value::obj(h)); // receiver (deeper on the stack)
        self.push(value); // its one arg, back on top
        self.rewind_op();
        self.send_suspend = Some(h);
        self.park_site = Some((send_deadlock_msg(cap), span));
    }

    /// Bounded-channel backpressure — after a `recv` frees a slot on a BOUNDED channel, wake any fiber
    /// parked on a full `send` to it. No-op for an unbounded channel (no sender ever parks there — the
    /// common `recv` path pays only a `cap.is_none()` check). Routes exactly like `chan_send_step`'s
    /// wake: an active sched (`mn` / `mn_enlist_sched`) → [`MnSched::recv_wake`]; else, with no
    /// scheduler in scope, [`Vm::wake_on_send`] wakes any other live sched's bucket for this channel.
    pub(super) fn wake_senders(&mut self, h: GcRef) {
        let core = self.channel_core(h);
        self.wake_senders_core(&core);
    }

    /// Same as [`Vm::wake_senders`], keyed on an already-held `core` rather than a heap handle — the
    /// entry point for a caller that only has the `Arc<ChannelCore>` (TICKET-028's receiver-wait
    /// sites, which never allocated a `GcRef` for their key).
    pub(super) fn wake_senders_core(&mut self, core: &Arc<ChannelCore>) {
        if core.cap.is_none() {
            return;
        }
        let key = Arc::as_ptr(core) as usize;
        if let Some(sched) = self.mn.clone() {
            // TICKET-128 (W13-25) — this waker (the receiver) keeps running after the handoff, so
            // `recruit: true`: it may block its own thread (e.g. `io.input`) before anyone else
            // reaches the handed-off sender, and nobody else would steal it before `HANDOFF_GRACE`.
            // TICKET-185 — `Settled` on every cap: only a party whose `Pending` left QUEUED (the
            // sender whose offer this receive took, or moved into the buffer) can proceed.
            sched.handoff_wake(key, core, WakeKind::Settled, self.wid, true);
        } else if let Some(sched) = self.mn_enlist_sched.clone() {
            sched.recv_wake(key, core);
        } else {
            core.wake_all();
            self.wake_on_send_key_kind(Arc::as_ptr(core) as usize, WakeKind::Settled);
        }
    }

    /// The ONE entry for a `recv` that may run inside a native re-entry (TICKET-136, W14-16): the
    /// `recv` method and `for v in ch:`. A native re-entry — a callback, a `defer`, a generator
    /// resume — has a host-stack loop frame that cannot be snapshot-parked, so an M:N fiber DEMOTES
    /// (block in place, spin a replacement worker) instead of faulting. A `timer(ms)` channel is
    /// excluded: it has no sibling sender, and [`Vm::chan_recv_step`] synthesises its value at any
    /// re-entry. A new site that calls `chan_recv_step` directly brings W14-16 back.
    ///
    /// A `Got` value freed a slot: this fn wakes a parked bounded sender. M1.8 — it wakes through
    /// the core it read before the wait, never through `h`: the receiver may be a popped
    /// temporary, which is no GC root while the wait is detached (`Vm::gc_blocking`).
    pub(super) fn recv_step_or_demote(
        &mut self,
        h: GcRef,
        span: Span,
    ) -> Result<RecvStep, RuntimeError> {
        let core = self.channel_core(h);
        let spec = if core.timer.is_some() {
            WaitSpec::Timer
        } else {
            WaitSpec::Recv
        };
        // A timer's Demote cell is served inside `chan_recv_step`: its value is synthesised at the
        // deadline, never queued, so `demote_recv_block`'s pop loop cannot serve it.
        let step = if spec == WaitSpec::Recv && self.block_mode(spec) == BlockMode::Demote {
            self.demote_recv_block(h, span)
        } else {
            self.chan_recv_step(h, span)
        };
        if let Ok(RecvStep::Got(_)) = step {
            self.wake_senders_core(&core);
        }
        step
    }

    /// One blocking-`recv` step on the snapshot-park / block-in-place / fault paths (NOT the
    /// in-callback demote path, which [`Vm::recv_step_or_demote`] takes first). Pops a value if one is waiting,
    /// signals `ClosedEmpty` on a closed-and-drained channel, or parks the running fiber (re-rooting
    /// the receiver + rewinding `ip` so the calling op re-runs on resume, setting `suspend`). Shared
    /// by `recv` (`CallMethod`) and the `ChanRecvOrClosed` op (`for v in ch:`).
    pub(super) fn chan_recv_step(
        &mut self,
        h: GcRef,
        span: Span,
    ) -> Result<RecvStep, RuntimeError> {
        // TICKET-185 — a re-run after a park settles this receiver's slot FIRST, before the deadline
        // and cancel checkpoints: a filled slot means a sender's `send` already returned, so the
        // value is this receiver's.
        if let Some(op) = self.pending.take()
            && let Settled::Got(_, v) = op.settle()
        {
            return Ok(RecvStep::Filled(v));
        }
        // W7-17 — `--timeout` ABOVE the cancellation checkpoint, because the deadline outranks a cancel
        // and because ending a timer park early TRIPS this fiber's cancel to close the park gap
        // ([`deadline_gap_wake`]): read in the other order, that fiber would report `cancelled` instead
        // of the honest hard halt. Suppressed inside a `defer` by the SAME `deferring > 0` term
        // `halt_requested` uses — a cleanup body's `ch.recv()` on an already-queued value must still
        // complete (measured: ungated, it silently truncated the defer at that `recv`, which is exactly
        // the "stopped promptly ≠ cleaned up" failure W7-16 was caught on). A defer that would PARK is
        // still aborted, at the park checkpoint below — that one is a hang, not cleanup.
        if self.deferring == 0 {
            self.deadline_halt(span)?;
        }
        // TICKET-194 — a receive with a value, a tripped latch, a fired timer or a close ready does
        // not wait, so it is not a cancellation point (owner decision 1): `recv_ready` decides what
        // it gets, and only a receive that would wait reaches `wait_halt`.
        let core = self.channel_core(h);
        let ready = self.recv_ready(&core, &mut core.q.lock().unwrap());
        match ready {
            RecvReady::Value(v) => return Ok(RecvStep::Got(v)),
            RecvReady::Fired => return Ok(RecvStep::Got(Value::bool(true))),
            RecvReady::Closed => return Ok(RecvStep::ClosedEmpty),
            RecvReady::Wait => {}
        }
        self.wait_halt(span)?;
        // A `timer(ms)` channel delivers `true` once its deadline passes. Handled here (uniformly,
        // before the ordinary park logic) so it works regardless of the engine the receiver runs in
        // and where the timer was created. Delivery is scheduled at RECV time, in the recv's own
        // scheduler — not at construction (a timer made at the top level can be recv'd in a child).
        {
            if let Some(deadline) = core.timer {
                if self.block_mode(WaitSpec::Timer) == BlockMode::Park {
                    // W7-17 — the `--timeout` checkpoint sits HERE, at the park, not at the top of this
                    // fn: everything above settles without blocking (a queued value, a tripped latch, an
                    // already-fired timer), and a hard abort has no business preempting a `recv` that
                    // completes. Putting it at the top truncated a `defer`'s cleanup `ch.recv()` on an
                    // ALREADY-QUEUED value — measured, and the exact "stopped promptly ≠ cleaned up"
                    // failure W7-16 was caught on. What must not happen post-deadline is a PARK, which
                    // reaches no loop back-edge and no `block_halt_check` at all.
                    self.deadline_halt(span)?;
                    // --parallel, top level: schedule a one-shot background `send(true)` at the deadline
                    // (in THIS scheduler) and park. The pending timer is accounted `inflight` so it
                    // vetoes the deadlock predicate while the lone fiber waits; the job un-accounts it.
                    // (Cancel was checked by `wait_halt` above — the engine-agnostic checkpoint.)
                    let sched = self.mn.clone().unwrap();
                    let key = self.channel_core_ptr(h);
                    let core_job = Arc::clone(&core);
                    let sched_job = Arc::clone(&sched);
                    sched.inflight.fetch_add(1, Ordering::Relaxed);
                    // W7-17 — fire at the SOONER of our deadline and the run's `--timeout`, so the
                    // wake that ends this park exists on both. Off (`self.deadline == None`) this is
                    // byte-identical to the plain `submit_at(deadline, …)` it replaces: one job, one
                    // `inflight` add/sub, no re-arming.
                    let fire_at = self.deadline.map_or(deadline, |rd| deadline.min(rd));
                    let gap_cancel = self.cancel.clone();
                    timer::submit_at(
                        fire_at,
                        Box::new(move || {
                            if std::time::Instant::now() >= deadline {
                                sched_job.send_wake(key, &core_job, Value::bool(true));
                            } else {
                                deadline_gap_wake(&sched_job, key, &core_job, &gap_cancel);
                            }
                            sched_job.leave_inflight();
                        }),
                    );
                    self.park_recv(h, span);
                    return Ok(RecvStep::Parked);
                }
                // TICKET-181 changed cell (e) — an M:N fiber inside a native callback DEMOTES for
                // the timeout, like `sleep_ms`: it used to inline-sleep and pin its worker, so at
                // `CHEZZI_THREADS=1` a sibling it then sent to had never run (`send on a rendezvous
                // channel: deadlock`; Go prints the value). The value is synthesised at the
                // deadline, never queued, so `demote_recv_block`'s pop loop cannot serve it — it
                // takes the sleep bracket, accounted `inflight`.
                if self.block_mode(WaitSpec::Timer) == BlockMode::Demote {
                    self.demote_block_until(deadline, WaitSpec::Timer, "timer.recv()", span)?;
                    return Ok(RecvStep::Got(Value::bool(true)));
                }
                // Every other context (`BlockMode::InlineSleep`: the top-level VM, the inline
                // outermost-`parallel:` builder VM, an eager `Executor` job's `Vm`): inline-sleep to
                // the deadline, synthesise.
                //
                // W7-16 — the wait is CHUNKED, not one `thread::sleep`: this deadline is ours, so it
                // stays a cancellation + `--timeout` checkpoint for its whole duration. Pre-fix an
                // eager `Executor` job here ran the full 3 s through a `cancel()` at 50 ms (and
                // through `--timeout`), while the same `timer(ms).recv()` in a nursery — which parks,
                // above — was cancelled at 55 ms. Same primitive, two answers.
                self.block_until_deadline(WaitSpec::Timer, deadline, span)?;
                return Ok(RecvStep::Got(Value::bool(true)));
            }
        }
        // M:N snapshot-park path (empty-open parks the fiber; the worker loop files it into the wait
        // set). A fiber woken only to be cancelled must not re-park — `wait_halt` above already
        // returned in that case. `MnSched::park` re-checks the queue, close, latch and cancel under
        // the sched lock, so no pre-park pop is needed here; an in-place waiter re-reads
        // `recv_ready` at `block_recv`'s loop head.
        let recv_mode = self.block_mode(WaitSpec::Recv);
        if recv_mode == BlockMode::Park {
            // TICKET-286 — the ungated park checkpoint the W7-17 comment above names: a park past the
            // deadline (inside a `defer` too) would requeue at its gap re-check and spin.
            self.deadline_halt(span)?;
            self.park_recv(h, span);
            return Ok(RecvStep::Parked);
        }
        // An eagerly-dispatched `Executor` job has no scheduler, and neither does the top-level `main`
        // thread — but "no scheduler" has not meant "nobody can send" since eager execution put
        // running jobs outside every scheduler. Both BLOCK here and let the process-wide verdict
        // decide (`future.md` §2d step 0): it raises this same fault only once every counted party is
        // blocked with no satisfiable wait between them.
        //
        // For `main` this is what makes `ex.submit(fn(): ch.send(42))` then `ch.recv()` print `42`, as
        // Go and CPython both do; it faulted here before, which was a wrong answer about a live
        // program. When nothing can in fact send — a top-level `recv` on a channel with no producer at
        // all — the verdict is reached on the FIRST halt check, before any wait, so that program still
        // faults with no added latency.
        if recv_mode == BlockMode::InPlace {
            return self.block_recv(h, &core, span);
        }
        // A native callback with no thread of its own to block on: the host stack cannot be unwound
        // to park either. Fault, as before.
        Err(self.err(EMPTY_RECV_DEADLOCK.to_string(), span))
    }

    /// TICKET-194 — THE ready decision of a channel receive, made in the caller's `core.q` hold
    /// `q`: a value > a tripped latch > a fired timer > closed. M3.2: a Task outcome sealed as text
    /// ([`ChanState::seal_err`]) is built into its `Err` value here, once, so every reader shares
    /// that one object. The build allocates under `q`, which no collection can wait for: an
    /// allocation reaches no safepoint.
    pub(super) fn recv_ready(&mut self, core: &ChannelCore, q: &mut ChanState) -> RecvReady {
        if let Some(msg) = q.unbuilt_outcome() {
            let msg = self.alloc_str(msg.to_string());
            let err = self.alloc_enum("Result", "Err", &[msg]);
            q.build_outcome(err);
        }
        if let Some(v) = q.pop() {
            return RecvReady::Value(v);
        }
        if core.done_latch.load(Ordering::Relaxed)
            || core.timer.is_some_and(|d| std::time::Instant::now() >= d)
        {
            return RecvReady::Fired;
        }
        if q.closed {
            return RecvReady::Closed;
        }
        RecvReady::Wait
    }

    /// Park the running fiber on an empty `recv`: re-root the receiver on the operand stack, rewind
    /// `ip` so the current op (`CallMethod(recv)` or `ChanRecvOrClosed`) re-executes on resume, and
    /// set the `suspend` sentinel. The scheduler / worker loop files the fiber into the channel's
    /// wait set; a sibling `send`/`close` wakes it.
    pub(super) fn park_recv(&mut self, h: GcRef, span: Span) {
        self.push(Value::obj(h));
        self.rewind_op();
        self.suspend = Some(h);
        self.park_site = Some((EMPTY_RECV_DEADLOCK, span));
    }
}
