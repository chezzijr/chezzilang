//! VM: demoting a blocked fiber in place: recv, send, `wait`, timers and sockets.
//! Split out of `src/vm/sched.rs`. `super::*` is the parent module.

use super::*;

impl Vm {
    /// D5 owe #3 (Path C) — a blocking `recv` reached inside a native callback (the host-stack loop
    /// frame of `xs.map(f)` / a sort comparator / `Shared.update(f)`, so `native_reentry > 0`) cannot
    /// snapshot-park. Instead of faulting `deadlock`, this worker thread **demotes**: it blocks in place
    /// on the channel's own condvar and resumes in place once a sibling `send`s — Go's `handoffp`. A
    /// fresh replacement worker is spun up ONCE (covering this thread's `wid`) so the live runnable-worker
    /// count stays at N; after this fiber settles, [`Vm::mn_worker_loop`] sees `self.demoted` and exits,
    /// so steady-state live workers = N + (fibers currently blocked in a callback) — Go's exact cost.
    ///
    /// Returns [`RecvStep::Got`] (the native callback continues on this thread with the value),
    /// [`RecvStep::ClosedEmpty`] (the channel was `close()`d while demoted — the caller faults
    /// "receive on a closed channel"), or a `cancelled` / `deadlock` fault (which unwinds the callback
    /// → the fiber faults). Never [`RecvStep::Parked`] — a demoted recv blocks in place, it never
    /// snapshot-parks. Only ever called on the M:N engine inside a native callback (the recv site gates
    /// on `mn.is_some() && native_reentry > 0`).
    ///
    /// TICKET-141 — releases this thread's width permit for the whole wait and re-takes it after.
    pub(super) fn demote_recv_block(
        &mut self,
        h: GcRef,
        span: Span,
    ) -> Result<RecvStep, RuntimeError> {
        self.demote_recv_block_in_place(h, span)
    }

    fn demote_recv_block_in_place(
        &mut self,
        h: GcRef,
        span: Span,
    ) -> Result<RecvStep, RuntimeError> {
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_recv_block called with no active M:N scheduler (self.mn is None)"),
        );
        let core = self.channel_core(h);
        let _gated = self.gated_register(&[&core]);
        // TICKET-185 — a rendezvous receiver publishes its SLOT before it registers (H3: a sender that
        // looks for a receiver while this one is registering must find the slot). Published only when
        // no value is waiting, in the same hold; the loop below takes a waiting value itself.
        let p = crate::vm::core::Pending::new();
        {
            let mut g = core.q.lock().unwrap_or_else(|e| e.into_inner());
            if core.cap == Some(0) && !g.recv_ready_for(None) && !g.closed {
                g.slot(&p, 0);
            }
        }
        let op =
            crate::vm::core::PendingOp::new(Arc::clone(&p), vec![(Arc::clone(&core), 0, false)]);
        // A filled slot is a delivered value: it outranks every other way out of this block.
        let finish =
            |op: crate::vm::core::PendingOp, r: Result<RecvStep, RuntimeError>| match op.settle() {
                crate::vm::core::Settled::Got(_, w) => Ok(RecvStep::Filled(w)),
                crate::vm::core::Settled::Cancelled
                | crate::vm::core::Settled::Closed
                | crate::vm::core::Settled::Sent(_) => r,
            };
        // 1. Account running → a registered waiter AND register the channel (#1 fix), under core lock A, then
        //    notify so an idle puller sitting in an untimed `take_runnable` `cv.wait` re-evaluates the
        //    deadlock predicate now that this fiber left `running` (without this notify a genuine
        //    all-blocked quiesce would never be detected — a hang). The registration lets
        //    `is_deadlocked` peek this fiber's queue so a value a sibling races in isn't misread as a
        //    deadlock (the #1 false-positive against an innocent parked sibling).
        let reg = match self.block_enter(
            WaitSpec::Recv,
            Some(quiesce::PartyWait::Recv(
                Arc::clone(&core),
                Some(Arc::clone(&p)),
            )),
            "recv",
            span,
        ) {
            Ok(reg) => reg,
            Err(e) => return finish(op, Err(e)),
        };
        // 2. Spin up a replacement worker ONCE per demoted thread (covers this `wid` while we block).
        //    Subsequent re-entries of this loop on the SAME thread (a callback that recvs repeatedly)
        //    reuse the already-spawned coverage — one spawn + one eventual exit per demoted thread.
        //    If the OS refuses the thread (a real mode for this raw-thread-per-demotion design under
        //    `RLIMIT_NPROC`/ENOMEM with many fibers blocked-in-callback), DON'T panic mid-accounting:
        //    un-roll step 1 (account + registry) and fault this fiber cleanly so the join still completes.
        // 3. Block in place. The take + un-account (unregister_waiter/running++) + un-register are
        //    ATOMIC under core lock A (A-then-q — the order `send_wake` uses → no ABBA), so the deadlock
        //    checker never observes an emptied-but-still-counted/registered demoted fiber (the #1
        //    window). A filled slot, then the QUEUE, are checked FIRST so a genuinely-sent value always
        //    wins over a spuriously-set `terminate`. Each exit path un-accounts under A; lost condvar
        //    wakeups are bounded by `DEMOTE_POLL_BACKOFF` (≤ latency, never a hang).
        loop {
            // TICKET-188 — read before core lock A: a child fault's `scope_fault` takes that lock.
            let halt = self.halt_requested();
            // --- settle under core lock A: a delivered value wins over cancel / terminate / deadlock ---
            let exit: Result<RecvStep, RuntimeError> = {
                let mut c = sched.lock();
                let mut qg = core.q.lock().unwrap_or_else(|e| e.into_inner());
                if !p.is_queued() {
                    // A sender filled the slot: `finish` takes the value.
                    reg.release(&sched, &mut c);
                    Ok(RecvStep::ClosedEmpty)
                } else if let Some(w) = qg.pop() {
                    // Withdraw the slot in the same hold, so no second sender fills it.
                    let _ = p.try_cancel();
                    qg.withdraw(&p);
                    reg.release(&sched, &mut c);
                    Ok(RecvStep::Got(w))
                } else if core.done_latch.load(Ordering::Relaxed) {
                    // A tripped latch (`trip()`) delivers `true` like a passed timer — ranks below a
                    // real value, above closed/terminate/deadlock (a `done().recv()` on a cancelled
                    // token reached inside a native callback must not false-deadlock).
                    reg.release(&sched, &mut c);
                    Ok(RecvStep::Got(WireValue::Bool(true)))
                } else {
                    // Closed-and-drained: nothing is waiting here and the channel is closed, so no value
                    // will ever arrive — signal `ClosedEmpty` (the caller faults "receive on a closed
                    // channel"). Ranks below a delivered value, above terminate/deadlock.
                    let closed = qg.closed;
                    drop(qg);
                    // Cancel (a sibling faulted): set `cancelled` BEFORE returning the Err so the
                    // outcome is SWALLOWED (a cancelled task is dropped, not reported). MUST go through
                    // `halt_requested()`, never a raw `self.cancel` load: a `defer` body runs under
                    // `guarded` (native_reentry > 0), and the predicate's `deferring == 0` term is what
                    // keeps cleanup atomic; it also folds in `cancel_outer` and (TICKET-188) a child
                    // fault of a nursery this party owns.
                    if let Some(e) = self.blocked_halt(halt, span) {
                        reg.release(&sched, &mut c);
                        Err(e)
                    } else if closed {
                        reg.release(&sched, &mut c);
                        Ok(RecvStep::ClosedEmpty)
                    } else if c.cut_in_place() {
                        // Terminate without a delivered value (genuine deadlock / nursery torn down).
                        reg.release(&sched, &mut c);
                        Err(sched.deadlock_err.clone())
                    } else if sched.is_deadlocked(&c) {
                        // Path C self-sufficient deadlock detection: evaluate the predicate HERE rather
                        // than depending on a separate idle puller being alive to fire it.
                        // TICKET-232 — through the one entry; the next pass returns through
                        // `run_exit_err` or `terminate`, which releases `reg`.
                        drop(c);
                        sched.fault_own_deadlock();
                        continue;
                    } else {
                        drop(c);
                        // --- wait on the channel's OWN condvar (q-only; core lock A released) ---
                        width::released(|| {
                            let q = core.q.lock().unwrap_or_else(|e| e.into_inner());
                            let _ = core.cv.wait_timeout_while(q, DEMOTE_POLL_BACKOFF, |g| {
                                p.is_queued() && !g.recv_ready_for(Some(&p)) && !width::reserved()
                            });
                        });
                        continue;
                    }
                }
            };
            return finish(op, exit);
        }
    }

    /// TICKET-185 — a blocking `send` reached INSIDE a native callback, whose offer is already
    /// published: the send-side twin of [`Vm::demote_recv_block`]. The worker thread demotes and
    /// blocks in place until a receiver takes the offer (`Sent`) or `close()` closes it
    /// (`CLOSED_SEND`). Cancel / exit / terminate / self-detected deadlock fault in place, unless
    /// the offer was already taken — then the `send` happened.
    ///
    /// Releases this thread's width permit for the whole wait and re-takes it after (TICKET-141).
    pub(super) fn demote_send_block(
        &mut self,
        core: Arc<ChannelCore>,
        op: crate::vm::core::PendingOp,
        span: Span,
    ) -> Result<SendStep, RuntimeError> {
        let r = self.demote_send_block_in_place(&core, &op.p, span);
        self.send_settled(op, span)
            .unwrap_or_else(|| r.map(|()| SendStep::Sent))
    }

    fn demote_send_block_in_place(
        &mut self,
        core: &Arc<ChannelCore>,
        p: &Arc<crate::vm::core::Pending>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_send_block called with no active M:N scheduler (self.mn is None)"),
        );
        let _gated = self.gated_register(&[core]);
        let reg = self.block_enter(
            WaitSpec::Send,
            Some(quiesce::PartyWait::Send(Arc::clone(p))),
            "send",
            span,
        )?;
        loop {
            // TICKET-188 — read before core lock A: a child fault's `scope_fault` takes that lock.
            let halt = self.halt_requested();
            {
                let mut c = sched.lock();
                // The offer's commit happens under `core.q`; read it in that hold, and release the
                // registration in the same hold (DEC-181).
                let settled = {
                    let _q = core.q.lock().unwrap_or_else(|e| e.into_inner());
                    !p.is_queued()
                };
                if settled {
                    reg.release(&sched, &mut c);
                    return Ok(());
                }
                if let Some(e) = self.blocked_halt(halt, span) {
                    reg.release(&sched, &mut c);
                    return Err(e);
                }
                if c.cut_in_place() {
                    reg.release(&sched, &mut c);
                    return Err(sched.deadlock_err.clone());
                }
                if sched.is_deadlocked(&c) {
                    // TICKET-232 — the next pass returns through `run_exit_err` or `terminate`.
                    drop(c);
                    sched.fault_own_deadlock();
                    continue;
                }
            }
            width::released(|| {
                let q = core.q.lock().unwrap_or_else(|e| e.into_inner());
                let _ = core.cv.wait_timeout_while(q, DEMOTE_POLL_BACKOFF, |_| {
                    p.is_queued() && !width::reserved()
                });
            });
        }
    }

    /// §6d M:N — a blocking multi-channel `wait` reached INSIDE a native callback (`native_reentry > 0`).
    /// A host-stack loop frame sits between the worker loop and the `wait`, so the fiber CANNOT
    /// snapshot-park (`park_wait`); it demotes — blocks this worker in place until an arm MAY be
    /// ready, then returns so `op_wait_poll` settles and re-polls in source order. The N-arm analogue
    /// of [`Vm::demote_recv_block`]: account `running → a registered waiter`, register EVERY arm (so
    /// `is_deadlocked` peeks them all and a value racing onto any arm vetoes a false fire), then loop.
    ///
    /// TICKET-185 — the `wait:`'s offers (send arms) and slots (rendezvous recv arms) are published
    /// under `p` before this is called, so a send arm blocks here like a recv arm (Go blocks it
    /// too). Ready == `p` settled, a recv arm has a value to take (a buffered value or another
    /// party's live offer) or a `trip()` latch, the timer arm's deadline passed, or every arm is a
    /// closed recv arm (the re-poll then faults `wait: all channels closed`). Cancel / exit /
    /// terminate / self-detected deadlock fault in place. Never parks.
    ///
    /// TICKET-141 — releases this thread's width permit for the whole wait and re-takes it after.
    pub(super) fn demote_wait_block(
        &mut self,
        arms: Vec<(Arc<ChannelCore>, bool)>,
        p: &Arc<crate::vm::core::Pending>,
        timer: Option<(usize, std::time::Instant)>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.demote_wait_block_in_place(arms, p, timer, span)
    }

    fn demote_wait_block_in_place(
        &mut self,
        arms: Vec<(Arc<ChannelCore>, bool)>,
        p: &Arc<crate::vm::core::Pending>,
        timer: Option<(usize, std::time::Instant)>,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let sched = Arc::clone(
            self.mn
                .as_ref()
                .expect("demote_wait_block called with no active M:N scheduler (self.mn is None)"),
        );
        let _gated = {
            let refs: Vec<&Arc<ChannelCore>> = arms.iter().map(|(c, _)| c).collect();
            self.gated_register(&refs)
        };
        // TICKET-181 — a timed `wait:` returns at its deadline whatever anyone does, so it is
        // `inflight` (changed cell (a), C1); an untimed one registers its arms as a waiter.
        let spec = WaitSpec::Wait {
            deadline: timer.is_some(),
            has_send: arms.iter().any(|(_, is_send)| *is_send),
        };
        let wait = (!spec.will_return())
            .then(|| quiesce::PartyWait::Wait(arms.clone(), Some(Arc::clone(p))));
        let reg = self.block_enter(spec, wait, "wait", span)?;
        let recv_ready = |core: &Arc<ChannelCore>, g: &crate::vm::core::ChanState| {
            g.recv_ready_for(Some(p)) || core.done_latch.load(Ordering::Relaxed)
        };
        loop {
            // TICKET-188 — read before core lock A: a child fault's `scope_fault` takes that lock.
            let halt = self.halt_requested();
            {
                let mut c = sched.lock();
                let mut ready = !p.is_queued();
                let mut all_closed = true;
                for (core, is_send) in &arms {
                    if *is_send {
                        all_closed = false;
                        continue;
                    }
                    let g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                    ready |= recv_ready(core, &g);
                    all_closed &= g.closed;
                }
                // WAIT-1 (demote path) — a live timer arm is ready once `now >= deadline`; the
                // re-poll scans the channel arms first, so a real `send` still beats the timer.
                ready |= timer.is_some_and(|(_, d)| std::time::Instant::now() >= d);
                if ready || all_closed {
                    reg.release(&sched, &mut c);
                    return Ok(());
                }
                // Cancel (a sibling faulted): swallow the outcome (mirror the snapshot-park cancel arm).
                if let Some(e) = self.blocked_halt(halt, span) {
                    reg.release(&sched, &mut c);
                    drop(c);
                    return Err(e);
                }
                if c.cut_in_place() {
                    reg.release(&sched, &mut c);
                    drop(c);
                    return Err(sched.deadlock_err.clone());
                }
                // A pending timer guarantees future progress (its deadline send), so it vetoes the
                // self-detected deadlock just like an `inflight` job does on the snapshot-park path.
                if sched.is_deadlocked(&c) {
                    // TICKET-232 — the next pass returns through `run_exit_err` or `terminate`.
                    drop(c);
                    sched.fault_own_deadlock();
                    continue;
                }
            }
            // No single condvar to wait on (N arms, N condvars) → bounded backoff poll. Sleep on the
            // FIRST arm's condvar with a timeout so a `send`/`close` to arm 0 wakes promptly, and any
            // other arm is observed within `DEMOTE_POLL_BACKOFF` (the documented lower-throughput path).
            let (first, is_send0) = &arms[0];
            width::released(|| {
                let q = first.q.lock().unwrap_or_else(|e| e.into_inner());
                // Clamp the backoff to the timer deadline so the loop re-polls and fires the timer arm
                // by its deadline (saturating, so a deadline that already passed yields ~zero wait).
                let backoff = match timer {
                    Some((_, d)) => DEMOTE_POLL_BACKOFF
                        .min(d.saturating_duration_since(std::time::Instant::now())),
                    None => DEMOTE_POLL_BACKOFF,
                };
                // W7-13r(b) — arm 0 is ready on what the loop above SETTLES on: `p` settled, or (recv arm)
                // a value to take or a `trip()` latch. `closed` is deliberately NOT a ready condition: the
                // re-poll SKIPS a closed+empty recv arm, so reporting ready would return instantly,
                // re-poll, skip, and spin — the parity-perf-0 live-lock.
                let _ = first.cv.wait_timeout_while(q, backoff, |g| {
                    !(!p.is_queued() || (!is_send0 && recv_ready(first, g))) && !width::reserved()
                });
            });
        }
    }

    /// D5 owe #3 Path C (#3) — a `sleep_ms(ms>0)` reached INSIDE a native callback (`native_reentry > 0`)
    /// on the M:N engine. The offload gate (`running → timer thread`) requires `native_reentry == 0`
    /// (the callback's `for`-loop state lives on the un-snapshottable Rust host stack), so without this
    /// the sleep runs INLINE and pins the worker for `ms`. Instead DEMOTE like a recv-in-callback: spin a
    /// replacement worker + sleep in place + resume, freeing the worker for `ms` (Go's `handoffp`).
    /// Accounted as `inflight` (NOT a registered waiter): a sleeper returns unconditionally, so it must VETO
    /// the deadlock predicate (like an offloaded blocking native — `is_deadlocked` already treats
    /// `inflight>0` as "external progress guaranteed"). A demoted waiter fiber is the opposite (it
    /// returns only via a sibling `send`). Returns `Ok(Nil)` (`sleep_ms` yields nothing). Residual: the
    /// `thread::sleep` is uninterruptible, so a cancel during the sleep is observed only after it returns
    /// (no worse than the inline pin it replaces — the worker is now freed).
    ///
    /// TICKET-141 — releases this thread's width permit for the whole wait and re-takes it after.
    pub(super) fn demote_block_until(
        &mut self,
        deadline: std::time::Instant,
        spec: WaitSpec,
        what: &str,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.width_release();
        let r = self.demote_block_until_in_place(deadline, spec, what, span);
        self.width_acquire();
        r
    }

    fn demote_block_until_in_place(
        &mut self,
        deadline: std::time::Instant,
        spec: WaitSpec,
        what: &str,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let reg = self.block_enter(spec, None, what, span)?;
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            std::thread::sleep(DEMOTE_POLL_BACKOFF.min(deadline - now));
            // The one predicate for a run-wide halt (`run_halt_due`): it answers `false` wherever
            // `run_exit_err` below will not fire, so the sleep is never cut short for nothing.
            let read = self.halt_requested();
            if let Some(e) = self.blocked_halt(read, span) {
                self.block_exit(reg);
                return Err(e);
            }
        }
        // 4. Un-account inflight → running (the `+1` is essential — the fiber's next dispatch does
        //    `running -= 1`, which would underflow without this restore).
        self.block_exit(reg);
        // The halts a blocked party observes ([`Vm::block_halts`]), read once after the sleep: a
        // cancel or child fault aborts the native callback loop, so no further elements sleep; a
        // run-wide `os.exit` (W7-47) since W7-57 also aborts the sleep itself within one
        // `DEMOTE_POLL_BACKOFF` (measured 3012 ms → 63 ms).
        self.block_halts(span)?;
        Ok(Value::nil())
    }

    /// **Block a would-block socket op IN PLACE when the fiber cannot park.** [`Vm::park_on_fd`] parks on
    /// the netpoller only when this Vm is an M:N worker shell AND `native_reentry == 0`. Callers do NOT
    /// route every other context here — the precondition is exactly the `Demote`/`InPlace` cells of the
    /// `WaitSpec::Socket` row of `block::mode`, i.e. the two contexts whose thread runs nothing else:
    ///
    /// - **In a native callback on an M:N worker** (`native_reentry > 0` — the callback's `for`-loop state
    ///   lives on the un-snapshottable Rust host stack). This is the original D5 owe #3 Path C case:
    ///   DEMOTE like [`Vm::demote_block_until`] — spin a replacement worker once so the pool keeps its
    ///   width, then backoff-poll in place.
    /// - **Top-level `main` on the DEFAULT engine** (`parallel`, `mn == None`, no `the job's sched`, no
    ///   scheduler, `native_reentry == 0`). There is no pool to demote FROM, so the scheduler bookkeeping
    ///   is skipped entirely — hence the `Option` sched — and only the wait loop runs. Until 2026-08-10
    ///   the four callers returned `Err("… requires the --parallel engine")` here too, on the DEFAULT
    ///   engine, because `mn.is_some()` means "worker shell", not "parallel is on" (`Vm::parallel`) —
    ///   which made the hello-world TCP server unwritable. `connect` had had a fallback all along — its
    ///   own private sleep-spin, deleted by `W7-59`, which now routes it here like the other four; they
    ///   were simply left behind (the repo's recurring fixed-some-arms-of-an-N-way-set finding, W7-22).
    ///
    /// **Not, deliberately: an eager `Executor` job.** TICKET-052 gave a blocked eager job's pool
    /// thread to a replacement worker, so the STARVATION argument above no longer holds — but the
    /// carve-out survives regardless, because widening `accept`/`read`/`write` to block in place would
    /// change what those ops RETURN on a would-block fd (a hang instead of the loud `Err` this arm
    /// keeps), which is a separate behaviour change TICKET-052 does not make. (The since-removed
    /// cooperative engine was excluded for the original reason: it ran every fiber on one thread.)
    ///
    /// **`connect` is deliberately admitted even though it is a wait a non-thread-owning party can
    /// reach** (`W7-59`): the starvation argument above is about waiting on a CHEZZI peer fiber,
    /// which only `accept`/`read`/`write` do. A `connect` handshake is completed by the KERNEL, so no
    /// chezzi party is starved by waiting for it — and both ancestors block (measured: CPython
    /// `socket.connect` 0.1 ms, Go `net.Dial` 314 µs, each from the sole/main thread). `connect`
    /// therefore has its own `WaitSpec::Connect` row in `block::mode`, not the `Socket` row.
    ///
    /// **Consequence, deliberate: an fd that never becomes ready is now a HANG, not an immediate `Err`.**
    /// That is Go-identical (`ln.Accept()` on the main goroutine with nobody dialing blocks forever) and
    /// it is the ancestor's contract, not an oversight. Three escapes are checked at the top of every
    /// loop iteration — the op's own `timeout_ms` (`deadline`, below), the run's `--timeout` via
    /// [`Vm::deadline_halt`], and cancellation — but **which of them EXIST depends on the context**, and
    /// for top-level `main` under `chezzi run` two of the three do not:
    ///
    /// - `--timeout` is a **`chezzi test` flag only**; `chezzi run --timeout=500 f.chz` is rejected by
    ///   the CLI with `chezzi run: unknown flag '--timeout=500'` (`src/main.rs:243`), so `self.deadline`
    ///   is `None` and [`Vm::deadline_halt`] is a no-op on this path.
    /// - `main` has no scope — no `cancel`, no `cancel_outer` — so `halt_requested()` reads an EMPTY
    ///   flag set and nothing can ever trip it.
    ///
    /// So an `accept`/`read`/`write` reached on `main` with **no explicit `timeout_ms`** has no escape at
    /// all short of SIGINT. Pass a `timeout_ms` if you need one. (The in-callback M:N demote — the other
    /// context routed here — does have all three: it runs under `chezzi test` where `--timeout` is legal,
    /// and its fiber has a scope cancel.)
    ///
    /// **Second-order, also deliberate: a socket-blocked `main` is NOT a quiescence party.** It cannot
    /// register as one — there is no scheduler here to register with — which matches the precedent on
    /// both sides: `time.sleep_ms` deliberately stays out of the predicate as "a false-deadlock
    /// generator" (`netio.rs`), and the M:N socket demote is accounted `inflight` specifically to VETO
    /// the predicate (Go-identical). It therefore never FABRICATES a verdict — the safe direction
    /// (`parked-is-not-stuck`: a wrong verdict is worse than a late one).
    ///
    /// It does, however, **SUPPRESS** the process-wide verdict for as long as the op is outstanding —
    /// which for an fd that never becomes ready is FOREVER, not merely "delayed" (an earlier version of
    /// this note claimed the latter; measured, it is wrong). Repro: `main` blocks in `accept()` while an
    /// eager `Executor` job blocks on a `Channel` nothing can ever send. Chezzi prints `listening` and
    /// hangs (rc=124 under `timeout 12`).
    ///
    /// **That is the ancestor's answer — do not "fix" it in code.** Go behaves the same way: an open
    /// socket is a runnable party, so `all goroutines are asleep` does not fire (measured — `main` in
    /// `ln.Accept()` plus a goroutine on an empty `chan int`: prints `listening`, hangs, rc=124). The
    /// since-removed cooperative engine printed `Err('accept would block: …')` and then the full
    /// `recv on an empty channel: deadlock`, exit 1 — but only because it REFUSED the socket op
    /// outright, which is what left its channel party alone in the predicate. It was the engine that
    /// did NOT match the ancestor.
    ///
    /// On an M:N worker it is accounted `inflight` (NOT a registered waiter): a socket op is woken by external OS readiness, so
    /// it must VETO the deadlock predicate — exactly the netpoller-park accounting (a lone in-callback
    /// `accept` with no client correctly never self-terminates, Go-identical), hence **no** `is_deadlocked`
    /// self-fire here. The flip side: this op exits the wait ONLY via fd readiness, `cancel`, or another
    /// worker setting `terminate` — so a nursery where *every* remaining fiber is an in-callback socket
    /// demote on an fd that never becomes ready, with no faulting sibling, hangs silently (no `deadlock`
    /// fault). That is the same Go-identical "all goroutines waiting on a never-ready socket" case the
    /// netpoller park already has; the rule is unchanged (don't await an fd that nothing will signal).
    ///
    /// Between attempts the worker kernel-BLOCKS on the fd via [`wait_fd_ready`] (woken immediately on
    /// readiness, no busy-poll, no wasted syscalls — close to the epoll path it can't use here) with a
    /// `DEMOTE_POLL_BACKOFF` timeout so a sibling-fault `cancel` is still observed within that window.
    /// `cancel`/`terminate` are re-checked at the TOP of every iteration (before re-attempting), so a
    /// cancelled task stops issuing socket work promptly and its outcome is SWALLOWED (mirrors the
    /// recv/sleep demote). `attempt` re-runs one non-blocking op (it owns a cloned `Arc<…Core>`) and
    /// returns `SockPoll::Ready` with the op's `Result`-shaped `Value`, or `SockPoll::WouldBlock`.
    ///
    /// `deadline` is the caller's `timeout_ms` (`None` = untimed): it bounds the WHOLE op here exactly
    /// as it does on the netpoller-park path, and expiring yields `Err("timeout")` — or, for a str `read`
    /// whose demote closure took a partial codepoint off the wire (`poll_partial` latched, N3a), the
    /// `Err("incomplete utf-8: …")` classification instead. It is the ONLY escape from the "never-ready
    /// fd" case above — an in-callback socket op is `inflight`, so it can never self-fire the deadlock
    /// predicate.
    ///
    /// TICKET-141 — releases this thread's width permit for the whole wait and re-takes it after.
    pub(super) fn demote_block_socket(
        &mut self,
        fd: std::os::fd::RawFd,
        interest: poller::Interest,
        deadline: Option<std::time::Instant>,
        span: Span,
        attempt: impl FnMut(&mut Vm) -> SockPoll,
    ) -> Result<Value, RuntimeError> {
        self.width_release();
        let r = self.demote_block_socket_in_place(fd, interest, deadline, span, attempt);
        self.width_acquire();
        r
    }

    fn demote_block_socket_in_place(
        &mut self,
        fd: std::os::fd::RawFd,
        interest: poller::Interest,
        deadline: Option<std::time::Instant>,
        span: Span,
        mut attempt: impl FnMut(&mut Vm) -> SockPoll,
    ) -> Result<Value, RuntimeError> {
        // `None` when there is no worker pool at all (top-level `main`, an eager `Executor` job): the
        // calling thread is its own, so there is nothing to demote FROM and the scheduler bookkeeping
        // below is skipped. The wait loop itself does not care either way.
        let sched = self.mn.as_ref().map(Arc::clone);
        let reg = if self.block_mode(WaitSpec::Socket) == BlockMode::Demote {
            Some(self.block_enter(WaitSpec::Socket, None, "a socket op", span)?)
        } else {
            None
        };
        let out = loop {
            // The halts a blocked party observes ([`Vm::block_halts`]: `--timeout`, cancel, child
            // fault, `os.exit`), BEFORE doing more work each iteration. W7-18 — an in-callback socket
            // op is accounted `inflight`, so it VETOES the deadlock predicate: without the deadline
            // read here an untimed `accept` hangs exactly like the netpoller-park shape. `break`, NOT
            // `?` — this loop is bracketed by `block_enter`/`block_exit`, and returning past the exit
            // would leak `running -= 1` / `inflight += 1` for the rest of the process (which is why
            // every other exit below is a `break` too).
            if let Err(e) = self.block_halts(span) {
                break Err(e);
            }
            // Nursery torn down (deadlock elsewhere / `os.exit`): fault in place. An `inflight` socket op
            // never *self*-fires the predicate (it vetoes it), so a genuine quiesce is surfaced by another
            // worker setting `terminate`, observed here within the backoff.
            if let Some(s) = &sched
                && s.lock().cut_in_place()
            {
                break Err(s.deadlock_err.clone());
            }
            match attempt(self) {
                SockPoll::Ready(r) => break r,
                SockPoll::WouldBlock => {
                    // The caller's `timeout_ms` bounds the WHOLE op here too, exactly as it does on the
                    // netpoller-park path — the demote loop used to wait on fd readiness with no
                    // deadline at all, and an in-callback op is accounted `inflight` (it VETOES the
                    // deadlock predicate), so a peer that never sends hung the program with no fault and
                    // no `Err("timeout")`. Cap the kernel wait by the remaining budget so the deadline is
                    // observed within it, not one full backoff late.
                    let left = match deadline {
                        None => DEMOTE_POLL_BACKOFF,
                        Some(dl) => {
                            let left = dl.saturating_duration_since(std::time::Instant::now());
                            if left.is_zero() {
                                // N3(a) — a str `read` that took a partial codepoint off the wire (its
                                // demote closure latched `poll_partial`) reports the incomplete-utf-8
                                // classification, not `timeout` (nothing arrived). `read_bytes`/`write`/
                                // `accept` never latch it, so their demote timeout stays `timeout`.
                                break Ok(match self.poll_partial {
                                    Some(owed) => self.sock_incomplete_err(owed),
                                    None => self.sock_err("timeout"),
                                });
                            }
                            left.min(DEMOTE_POLL_BACKOFF)
                        }
                    };
                    wait_fd_ready(fd, interest, left);
                }
            }
        };
        if let Some(reg) = reg {
            self.block_exit(reg);
        }
        out
    }
}
