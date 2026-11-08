// vm::netio — split out of vm/mod.rs. `super::*` == the `vm` module.
// Channels, Shared/RwShared/Atomic, sockets/listeners, netpoller parks.

use super::*;

/// Generate a `pub(super) fn <name>(&self, GcRef) -> Arc<CoreType>` that clones out the shared
/// `Arc` behind a handle of the given `Obj` variant (refcount bump). See [`Vm::channel_core`] for
/// the rationale (the `Arc` is held only for the calling method, so it does not borrow the heap);
/// `channel_core`/`socket_core` stay hand-written to carry that doc.
macro_rules! core_accessor {
    ($name:ident, $variant:ident, $core:ty) => {
        pub(super) fn $name(&self, h: GcRef) -> Arc<$core> {
            match self.heap.get(h) {
                Obj::$variant(core) => Arc::clone(core),
                _ => unreachable!(concat!(stringify!($name), " on non-", stringify!($variant))),
            }
        }
    };
}

/// A parking guard wait first retries its take this many times, [`GUARD_SPIN_LOOPS`] spin hints
/// apart (a few microseconds in all, as Go's `sync.Mutex` spins before it sleeps).
const GUARD_SPINS: usize = 4;
const GUARD_SPIN_LOOPS: usize = 64;

/// The `Err` text of a would-block socket op in a context whose table cell is `Refuse` (an eager
/// `Executor` job, `block::mode`'s Socket/Connect row). One copy for every socket op.
pub(super) fn sock_would_block_msg(op: &str) -> String {
    format!(
        "{op} would block: an Executor job doesn't own its thread — blocking here would starve \
         every other job and `parallel:` nursery sharing the pool. Do this socket op inside \
         `spawn:` or a `parallel:` nursery instead, where it parks rather than blocking a shared \
         thread."
    )
}

/// The shared fault for a `send` to a CLOSED channel. ONE const: the top-of-`send` guard, the `wait:` send arm and the eager
/// blocked-sender loop must all emit byte-identical text. Go panics `send on closed channel` here.
pub(super) const CLOSED_SEND: &str = "send on a closed channel";

/// W7-17 — end a timer park EARLY, because the run's `--timeout` expired before the timer's own
/// deadline. Used by both `timer(ms)` park sites ([`Vm::chan_recv_step`]'s timer branch and
/// [`Vm::op_wait_poll`]'s timer arm), whose jobs are otherwise armed for their own deadline.
///
/// **This must leave STATE, not just a wake, and that is the whole reason it trips `cancel`.** The
/// timer job is submitted BEFORE the fiber actually parks (`park_recv`/`wait_suspend` only mark it;
/// [`MnSched::park`]/[`MnSched::park_wait`] do the parking, later, behind the core lock). A job that
/// fires in that window finds an EMPTY bucket, so a bare wake is simply lost — and the fiber then
/// parks with its one job already spent (`op_wait_poll`'s `timer_armed` CAS cannot re-arm it), i.e. a
/// hang past the very deadline that exists to prevent hangs. The park-gap re-check reads exactly four
/// things — a queued value, `closed`, `done_latch`, and the fiber's SCOPE CANCEL — and the first three
/// all mean "the timer fired", which is a lie here. So the cancel flag is the one truthful state that
/// closes the gap: set it, and a fiber still in flight requeues instead of parking, while one already
/// parked is woken by the `close_wake` below (which delivers nothing — that is all `close_wake` does,
/// it never sets `closed`; `recv_wake` already reuses it the same way).
///
/// Either way the fiber re-runs its op and faults at the op's own `--timeout` checkpoint, which is
/// ordered ABOVE the cancel checkpoint — so the verdict is the honest `timed_out` hard halt, not
/// `cancelled`. Ordering is safe in both directions: the store happens-before this thread takes the
/// core lock in `close_wake`, so a `park_wait` that wins the lock either already sees the flag (and
/// requeues) or parks and is then found by `close_wake`.
pub(super) fn deadline_gap_wake(
    sched: &Arc<MnSched>,
    key: usize,
    core: &Arc<ChannelCore>,
    scope_cancel: &Option<Arc<AtomicBool>>,
) {
    if let Some(c) = scope_cancel {
        crate::vm::trip_cancel_flag(c);
    }
    sched.close_wake(key, core);
}

/// W15-3 — non-blocking write-all: push `data[*sent..]` until every byte is accepted, advancing
/// `*sent` per `write(2)`. `Ok(())` once `*sent == data.len()`; a `WouldBlock` (or any other error)
/// returns with `*sent` holding the bytes already sent, so the caller can park and resume there.
pub(super) fn write_from(
    stream: &mut std::net::TcpStream,
    data: &[u8],
    sent: &mut usize,
) -> std::io::Result<()> {
    while *sent < data.len() {
        match std::io::Write::write(stream, &data[*sent..]) {
            Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
            Ok(n) => *sent += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// B1 — the outcome of decoding one socket chunk (+ the socket's carried tail) as UTF-8.
pub(super) enum Decoded {
    /// A complete, valid `str` (possibly empty — the EOF sentinel).
    Text(String),
    /// A recoverable `Err` message (genuinely-invalid bytes, or a partial codepoint left at EOF).
    Fail(String),
    /// The bytes so far are a strict prefix of ONE codepoint (≤3 bytes, now carried): no complete
    /// codepoint to hand back yet — take more bytes off the fd.
    NeedMore,
}

/// M3.3 — THE text of a same-task re-entry fault on a box guard (Q1, Q2): `guard` is the guard the
/// task holds, `kind` the box, `nested` the calls that re-enter, `closure` the closure they run in.
fn reentry_text(guard: &str, kind: &str, nested: &str, closure: &str) -> String {
    format!(
        "deadlock: this task already holds the {guard} guard on this {kind} box — a nested \
         {nested} on the same box inside its own {closure} closure"
    )
}

impl Vm {
    /// B3.1 — clone out the shared `Arc<ChannelCore>` behind a `Channel` handle (refcount bump). The
    /// `Arc` is held only for the duration of the calling method, so locking it does not borrow the
    /// heap, leaving `self` free for the re-entrant value paths (`invoke_value`).
    pub(super) fn channel_core(&self, h: GcRef) -> Arc<ChannelCore> {
        match self.heap.get(h) {
            Obj::Channel(core) => Arc::clone(core),
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
            | Obj::Closure { .. }
            | Obj::Module(_)
            | Obj::Native { .. }
            | Obj::Builtin(_)
            | Obj::Cffi(_)
            | Obj::Ptr(_)
            | Obj::Shared(_)
            | Obj::RwShared(_)
            | Obj::Atomic(_)
            | Obj::AtomicInt(_)
            | Obj::Executor(_)
            | Obj::Socket(_)
            | Obj::Listener(_)
            | Obj::Writer(_)
            | Obj::Reader(_)
            | Obj::Generator(_) => unreachable!("channel_core on non-channel"),
        }
    }

    core_accessor!(shared_core, Shared, SharedCore);
    core_accessor!(rwshared_core, RwShared, RwSharedCore);
    core_accessor!(atomic_core, Atomic, AtomicCore);
    core_accessor!(atomic_int_core, AtomicInt, AtomicIntCore);

    /// TICKET-016 (W8-3) — take the process-global update guard for a `Shared`/`RwShared` box
    /// identified by `key` (its core's stable `Arc` address), bracketed by the demote-in-place
    /// pair so a blocking wait does not starve the pool. Faults with a specific message for a
    /// self-held re-entry (a nested `set`/`update`/`write` on the SAME box inside its own closure)
    /// vs. a longer cross-task/cross-box wait-for cycle (e.g. AB-BA).
    ///
    /// TICKET-194: a free guard is taken with no cancellation point. Only a guard that would wait
    /// reaches `wait_halt`. Stage 1 (bounded by `GUARD_DEMOTE_BUDGET`, in place, DEC-016) then waits
    /// as `guard_wait_block` does ([`Vm::guard_free_then_take`]). Stage 1 reads the halts when the
    /// guard comes free (TICKET-200).
    ///
    /// A fiber with no host frame under it (`BlockMode::Park`) parks instead: a short spin, then it
    /// files its wait-for edge and keeps its [`core::WaitTicket`] in its state (`guard_ticket`),
    /// sets `guard_suspend` and returns [`Vm::park_sentinel`] `Err`, which the box method
    /// turns into a rewind ([`Vm::shared_method`]). The re-run takes the guard through the ticket.
    pub(super) fn take_update_guard<'a>(
        &mut self,
        key: GuardKey<'a>,
        what: &str,
        span: Span,
    ) -> Result<core::UpdateGuard, RuntimeError> {
        // A ticket kept across a park is this re-run's place in line; any other is stale.
        let held = self.guard_ticket.take().filter(|t| t.is_for(key));
        if held.is_none()
            && let Some(guard) = core::try_update_guard(key, self.guard_token)
        {
            return Ok(guard);
        }
        let reentry = match held {
            Some(_) => None,
            None => self.guard_reentry(key),
        };
        let result = match reentry {
            Some(cycle) => Err(cycle),
            None if self.block_mode(WaitSpec::Guard) == BlockMode::Park => {
                let ticket = held.unwrap_or_else(|| core::WaitTicket::new(key));
                self.guard_park(ticket, span)?
            }
            None => {
                // M3.3 — the ticket keeps a waiting writer counted from here to its take or its
                // exit, across both stages and the permit handoff (Go writer preference).
                let mut ticket = held.unwrap_or_else(|| core::WaitTicket::new(key));
                self.wait_halt(span)?;
                let until = std::time::Instant::now() + GUARD_DEMOTE_BUDGET;
                // One wait over both stages: the permit is given back for all of it.
                let mut reg = self.wait_permit(WaitSpec::Guard);
                let staged = loop {
                    let left = until.saturating_duration_since(std::time::Instant::now());
                    match self.guard_free_then_take(&reg, &mut ticket, None, left, span) {
                        Err(halt) => break Some(Err(halt)),
                        Ok(Ok(Some(g))) => break Some(Ok(Ok(g))),
                        Ok(Err(c)) => break Some(Ok(Err(c))),
                        Ok(Ok(None)) if left.is_zero() => break None,
                        Ok(Ok(None)) => {}
                    }
                };
                let r = match staged {
                    Some(r) => r,
                    None => self.guard_wait_block(&mut reg, key, &mut ticket, what, span),
                };
                self.wait_end(reg, &[]);
                r?
            }
        };
        let kind = key.lock.kind;
        result.map_err(|cycle| match cycle {
            GuardCycle::SelfHeld => self.err(
                reentry_text("update", kind, "set/update/write", "update/write"),
                span,
            ),
            GuardCycle::ReadHeld => self.err(reentry_text("read", kind, "set/write", "read"), span),
            GuardCycle::Cycle => self.err(
                "deadlock: two or more tasks each hold a Shared/RwShared update guard the \
                 other is waiting for (a lock cycle)"
                    .to_string(),
                span,
            ),
        })
    }

    /// The `Park` half of [`Vm::take_update_guard`]: a wait that does not end at once is a
    /// cancellation point and a deadline checkpoint, then spins [`GUARD_SPINS`] short rounds,
    /// then parks the fiber holding `ticket` (`Ok(Err(cycle))` when its wait-for edge closes a
    /// cycle). A parked fiber is woken by a release ([`core::BoxLock::wake_parked`]).
    fn guard_park(
        &mut self,
        mut ticket: core::WaitTicket,
        span: Span,
    ) -> Result<Result<core::UpdateGuard, GuardCycle>, RuntimeError> {
        let me = self.guard_token;
        self.wait_halt(span)?;
        self.deadline_halt(span)?;
        for _ in 0..GUARD_SPINS {
            if let Some(g) = ticket.try_take(me) {
                return Ok(Ok(g));
            }
            for _ in 0..GUARD_SPIN_LOOPS {
                std::hint::spin_loop();
            }
        }
        if let Err(cycle) = ticket.park_edge(me) {
            return Ok(Err(cycle));
        }
        let kind = ticket.lock().kind;
        self.guard_suspend = Some(ticket.lock().park_key());
        self.guard_ticket = Some(ticket);
        self.park_site = Some((super::deadlock::Wait::guard(kind).phrase(), span));
        Err(Self::park_sentinel(span))
    }

    /// The `Err` a guard or Executor join park returns up to its method, which sees
    /// `guard_suspend` / `exec_join_suspend` set and rewinds instead ([`Vm::shared_method`],
    /// [`Vm::executor_method`]); never a fault.
    pub(super) fn park_sentinel(span: Span) -> RuntimeError {
        super::exec::plain_err(String::new(), span)
    }

    /// M3.3 — a failed fast acquire of `key` that is this task's own re-entry, the one wait that is
    /// certain never to end: the task holds the write guard (`SelfHeld`), writes a box it reads
    /// (`ReadHeld`, Q2), or reads again a box it reads while a writer waits for that outer read
    /// (`Cycle`: the writer waits for this task, this task for the writer). A fast acquire fails on
    /// a box the task reads only when a writer waits, so the third case needs no second load.
    fn guard_reentry(&self, key: GuardKey<'_>) -> Option<GuardCycle> {
        if key.lock.writer() == self.guard_token {
            return Some(GuardCycle::SelfHeld);
        }
        let reads = self.held_reads.contains(&(Arc::as_ptr(key.lock) as usize));
        match key.mode {
            GuardMode::Write if reads => Some(GuardCycle::ReadHeld),
            GuardMode::Read if reads => Some(GuardCycle::Cycle),
            GuardMode::Write | GuardMode::Read => None,
        }
    }

    /// TICKET-193/194 — THE guard order: wait for the guard to look free holding no width permit,
    /// take the permit back ([`Vm::wait_retake`]), then take the guard with no wait. So the permit
    /// comes before the guard; a lost race gives it back again. `Ok(None)` is still held after `budget`, or lost the race;
    /// `Err` is a halt or this wait's deadlock cut.
    /// Stage 1 of [`Vm::take_update_guard`] and [`Vm::guard_wait_block`] both call it. The `ticket`
    /// stays live across the permit handoff.
    ///
    /// TICKET-200 — a wait that ends because the guard came free reads the halts
    /// ([`Vm::block_halts`]) before it takes the guard, because the free may be the halt's own cut
    /// of the holder (an `os.exit` or a cancel unwinding the holder's `update`).
    fn guard_free_then_take(
        &mut self,
        reg: &WaitReg,
        ticket: &mut core::WaitTicket,
        taking: Option<&std::sync::atomic::AtomicBool>,
        budget: std::time::Duration,
        span: Span,
    ) -> Result<Result<Option<core::UpdateGuard>, GuardCycle>, RuntimeError> {
        let me = self.guard_token;
        match self.wait_door(&[], |_| {
            core::await_update_guard_free(ticket, me, Some(budget))
        }) {
            Err(c) => return Ok(Err(c)),
            Ok(false) => return Ok(Ok(None)),
            Ok(true) => {}
        }
        self.block_halts(span)?;
        // The sched-wide verdict cut this in-place waiter: it ends here with its own wait instead
        // of taking the guard. Both stages leave on it at once (stage 1 no longer spins to its
        // budget first). A verdict's cut of the holder is the lock's own `CUT` (the box is no
        // progress for a waiter, `core::UpdateGuard::release_cut`).
        if let Some(sched) = self.mn.as_ref().map(Arc::clone)
            && self.in_place_cut(&sched.lock(), reg.tok())
        {
            return Err(self.cut_fault(&sched, span));
        }
        self.wait_retake(reg);
        // A registered waiter marks its take before it tries (`core::GuardWait::taking`).
        let mark = |on: bool| {
            if let Some(t) = taking {
                t.store(on, std::sync::atomic::Ordering::SeqCst);
            }
        };
        mark(true);
        Ok(Ok(match ticket.try_take(me) {
            None => {
                mark(false);
                self.wait_rerelease(reg);
                None
            }
            g => g,
        }))
    }

    /// TICKET-063 — the unbounded half of [`Vm::take_update_guard`], reached once the 5 ms bounded
    /// acquire has given up. Registers the wait as a waiter (never `inflight` — a guard wait has
    /// no promised external progress; see `SchedCore::waiters`' doc), registers this thread as a
    /// blocked party so the process-wide verdict can see it, and polls
    /// [`Vm::block_halt_check`] every [`super::DEMOTE_POLL_BACKOFF`] so `--timeout`, cancel and
    /// `os.exit` reach a guard waiter exactly like every other blocking-in-place site. The counters
    /// are un-accounted via `guard_wait_exit` on every exit path (no `?` before that call).
    fn guard_wait_block<'a>(
        &mut self,
        reg: &mut WaitReg,
        key: GuardKey<'a>,
        ticket: &mut core::WaitTicket,
        what: &str,
        span: Span,
    ) -> Result<Result<core::UpdateGuard, GuardCycle>, RuntimeError> {
        let owned = core::GuardWait {
            lock: Arc::clone(key.lock),
            mode: key.mode,
            taking: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            admitted: ticket.admission(),
        };
        let taking = Arc::clone(&owned.taking);
        self.guard_wait_enter(reg, owned, what, span)?;
        let out = loop {
            // TICKET-193 — permit BEFORE guard. Wait for the guard to come free holding no permit,
            // take the permit, then take the guard without waiting. Taking the guard first parks
            // its OWNER behind permit holders that wait in place for that same guard, so every
            // handoff cost one GUARD_DEMOTE_BUDGET (T=2: 7172 timeouts, 18.9 s).
            match self.guard_free_then_take(
                reg,
                ticket,
                Some(&taking),
                super::DEMOTE_POLL_BACKOFF,
                span,
            ) {
                Ok(Ok(Some(g))) => break Ok(Ok(g)),
                Ok(Err(cycle)) => break Ok(Err(cycle)),
                Err(e) => break Err(e),
                Ok(Ok(None)) => {}
            }
            if let Err(e) = self.block_halt_check(span) {
                break Err(e);
            }
            if let Some(sched) = self.mn.as_ref().map(Arc::clone) {
                let c = sched.lock();
                if self.in_place_cut(&c, reg.tok()) {
                    drop(c);
                    // The waiter names its own wait (the text and span its party site carries
                    // above), as on the process-wide verdict: a guard wait is not a channel wait.
                    break Err(self.cut_fault(&sched, span));
                }
                if sched.is_deadlocked(&c) {
                    // TICKET-232 — the next pass ends the wait through the run halt or `terminate`.
                    drop(c);
                    sched.fault_own_deadlock();
                }
            }
        };
        self.guard_wait_exit(reg);
        out
    }

    /// `AtomicInt(v)` — pop the int init, wrap it in a fresh lock-free `Arc<AtomicIntCore>`. The checker
    /// guarantees the single arg is an int; a boxed BigInt is narrowed via `int_of`. `#[inline(never)]`
    /// so its locals stay out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    pub(super) fn new_atomic_int(&mut self, _span: Span) -> Result<Value, RuntimeError> {
        let init = self.pop();
        let n = self.int_of(init);
        Ok(Value::obj(self.heap.alloc(Obj::AtomicInt(Arc::new(
            AtomicIntCore {
                v: std::sync::atomic::AtomicI64::new(n),
            },
        )))))
    }

    /// `Atomic(v)` — pop the init and hold it, by handle (M3.2), in a fresh `Arc<AtomicCore>`.
    /// `#[inline(never)]` so its locals stay out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    pub(super) fn new_atomic(&mut self) -> Value {
        let init = self.pop();
        Value::obj(
            self.heap
                .alloc(Obj::Atomic(Arc::new(AtomicCore::new(init)))),
        )
    }

    /// `std.time.timer(ms)` (`Kind::InterceptTimer`): a fresh `Channel[bool]` stamped with `now + ms`.
    /// Delivery is handled at `recv` time (in the receiver's scheduler), NOT here, so a timer made at
    /// the top level can be `recv`'d inside a `--parallel` child. `#[inline(never)]` so the
    /// `Instant`/`Duration` math stays out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    pub(super) fn new_timer(&mut self, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        let ms = match args {
            [v] if let Some(ms) = self.int_val(*v) => ms.max(0) as u64,
            _ => {
                let got = args.first().map_or("nothing", |v| self.type_name(*v));
                return Err(self.err(format!("timer(ms) expects int, got {got}"), span));
            }
        };
        // Saturate a pathological `ms` to a far-future deadline rather than panic on `Instant` overflow
        // (mirrors the `sleep_ms` offload path).
        let deadline = std::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(ms))
            .unwrap_or_else(|| {
                std::time::Instant::now() + std::time::Duration::from_secs(86_400 * 365)
            });
        let core = Arc::new(ChannelCore {
            timer: Some(deadline),
            ..Default::default()
        });
        Ok(Value::obj(self.heap.alloc(Obj::Channel(core))))
    }

    core_accessor!(executor_core, Executor, ExecutorCore);
    core_accessor!(listener_core, Listener, ListenerCore);
}
