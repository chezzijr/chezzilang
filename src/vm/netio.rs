// vm::netio — split out of vm/mod.rs. `super::*` == the `vm` module.
// Channels, Shared/RwShared/Atomic, sockets/listeners, netpoller parks.

use super::*;

/// §2c1 — the RAII pair [`Vm::block_party_guard`] hands back: the process-wide blocked-party
/// registration (absent when this thread is not a counted party) plus the `body_blocked` marks on
/// every eager nursery this thread owns. Both are released together when the block returns.
///
/// The marks are what let a genuine `main`-plus-sibling deadlock still FAULT under §2c1's eager
/// start: a top-level nursery's `body_open` spans essentially the whole program, and it vetoes the
/// deadlock predicate, so without clearing it for the duration of the block the verdict could never
/// fire. See [`super::JoinScope::body_blocked`].
pub(super) struct BlockGuard {
    pub(super) _party: Option<quiesce::PartyGuard>,
    /// Whether this guard also raised `awaiting_builder` — a NESTED-JOIN block does, a channel block
    /// does not. See `MnSched::set_body_blocked`.
    pub(super) awaiting: bool,
    pub(super) bodies: Vec<(Arc<MnSched>, usize)>,
    /// This block's wait, published on every sched in `bodies` for the duration. See
    /// `SchedCore::waiters` — this is what keeps the `body_blocked` relaxation from false-faulting
    /// a rendezvous. `None` for a nested-join block, which waits on no channel.
    pub(super) wait: Option<Arc<quiesce::PartyWait>>,
}

impl Drop for BlockGuard {
    fn drop(&mut self) {
        for (s, scope) in &self.bodies {
            s.set_body_wait(*scope, self.wait.as_ref(), false, self.awaiting);
            // TICKET-211 — the body runs again on its own thread: it takes bit 0 back.
            if *scope == 0 {
                s.body_runner(false);
            }
        }
    }
}

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

/// The shared fault for a `send` on a FULL bounded channel that cannot park (top level with no
/// nursery, or inside a native callback). ONE const so every non-parkable full-send path emits
/// byte-identical text (parity). Mirrors `chan_recv_step`'s empty-recv deadlock note.
///
/// §2c1 — a spawned task starts running at its `spawn`, not at the nursery's join, so this verdict
/// means no task that could receive is spawned at all, or the one that was has already exited: the
/// hint names that rather than a nursery-ordering quirk.
pub(super) const FULL_SEND_DEADLOCK: &str = "send on a full channel: deadlock — the bounded channel is at \
    capacity and no runnable task can receive to free a slot. (Make sure a task that receives from \
    this channel is spawned with `spawn:` and is still running.)";

/// The rendezvous (cap 0) sibling of [`FULL_SEND_DEADLOCK`] (TICKET-136, W14-35): a rendezvous
/// channel has no slots, so "at capacity" is false for it — the send can never complete because no
/// receiver is coming. Picked by [`send_deadlock_msg`] wherever the channel's cap is known.
pub(super) const RENDEZVOUS_SEND_DEADLOCK: &str = "send on a rendezvous channel: deadlock — the channel has \
    no buffer and no runnable task can receive from it. (Make sure a task that receives from this \
    channel is spawned with `spawn:` and is still running.)";

/// The send-deadlock text for a channel of capacity `cap` (`Some(0)` = rendezvous).
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

pub(super) fn send_deadlock_msg(cap: Option<usize>) -> &'static str {
    if cap == Some(0) {
        RENDEZVOUS_SEND_DEADLOCK
    } else {
        FULL_SEND_DEADLOCK
    }
}

/// The shared fault for a `send` to a CLOSED channel. ONE const for the same reason
/// [`FULL_SEND_DEADLOCK`] is one: the top-of-`send` guard, the `wait:` send arm and the eager
/// blocked-sender loop must all emit byte-identical text. Go panics `send on closed channel` here.
pub(super) const CLOSED_SEND: &str = "send on a closed channel";

/// The shared fault for a `recv` on a CLOSED-and-drained channel — the twin of [`CLOSED_SEND`], and
/// one const for the same byte-identical-text reason (it is raised from both the demote arm and the
/// ordinary `chan_recv_step` arm of the same `match`).
pub(super) const CLOSED_RECV: &str = "receive on a closed channel";

/// The shared fault for a `recv` on an EMPTY channel that cannot park. Raised by the native-callback
/// arm of [`Vm::chan_recv_step`], which cannot block at all, and by any party that blocked in place
/// and was then judged deadlocked by the process-wide verdict ([`crate::vm::quiesce`]): ONE const so
/// every spelling of the same verdict is byte-identical.
pub(super) const EMPTY_RECV_DEADLOCK: &str = "recv on an empty channel: deadlock — no runnable task can send. \
    (Make sure a task that sends to this channel is spawned with `spawn:` and is still running.)";

/// The `wait:` sibling of [`EMPTY_RECV_DEADLOCK`] — every arm empty and nobody left to send.
pub(super) const EMPTY_WAIT_DEADLOCK: &str = "wait on channels that are all empty: deadlock — no runnable task \
    can send. (Make sure a task that sends to one of these channels is spawned with `spawn:` and is \
    still running.)";

/// TICKET-223 — the report of a party blocked on a `Shared`/`RwShared` guard when the deadlock
/// verdict names it. Replaces the "parallel: block" text the guard wait used to borrow.
const GUARD_DEADLOCK: &str = "update guard wait: deadlock — the task holding this Shared/RwShared \
    guard is blocked and no runnable task can release it";

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

impl Vm {
    /// B3.1 — clone out the shared `Arc<ChannelCore>` behind a `Channel` handle (refcount bump). The
    /// `Arc` is held only for the duration of the calling method, so locking it does not borrow the
    /// heap, leaving `self` free for the re-entrant value paths (`from_wire`, `invoke_value`).
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
    /// holding no permit (DEC-141), the same order as `guard_wait_block` ([`Vm::guard_free_then_take`]).
    /// Stage 1 reads the halts when the guard comes free (TICKET-200).
    pub(super) fn take_update_guard(
        &mut self,
        key: usize,
        what: &str,
        span: Span,
    ) -> Result<core::UpdateGuard, RuntimeError> {
        let result = match acquire_update_guard_within(
            key,
            self.guard_token,
            Some(std::time::Duration::ZERO),
        ) {
            Ok(Some(guard)) => Ok(guard),
            Err(cycle) => Err(cycle),
            Ok(None) => {
                self.wait_halt(span)?;
                let until = std::time::Instant::now() + GUARD_DEMOTE_BUDGET;
                self.width_release();
                let staged = loop {
                    let left = until.saturating_duration_since(std::time::Instant::now());
                    // No `?` in this loop: a halt must still reach `width_acquire` below.
                    match self.guard_free_then_take(key, left, span) {
                        Err(halt) => break Some(Err(halt)),
                        Ok(Ok(Some(g))) => break Some(Ok(Ok(g))),
                        Ok(Err(c)) => break Some(Ok(Err(c))),
                        Ok(Ok(None)) if left.is_zero() => break None,
                        Ok(Ok(None)) => {}
                    }
                };
                self.width_acquire();
                match staged {
                    Some(r) => r?,
                    None => self.guard_wait_block(key, what, span)?,
                }
            }
        };
        result.map_err(|cycle| match cycle {
            GuardCycle::SelfHeld => self.err(
                "deadlock: this task already holds the update guard on this Shared box — a \
                 nested set/update/write on the same box inside its own update/write closure"
                    .to_string(),
                span,
            ),
            GuardCycle::Cycle => self.err(
                "deadlock: two or more tasks each hold a Shared/RwShared update guard the \
                 other is waiting for (a lock cycle)"
                    .to_string(),
                span,
            ),
        })
    }

    /// TICKET-193/194 — THE guard order: wait for the guard to look free holding NO width permit
    /// (DEC-141), take the permit, then take the guard with a zero budget. The caller holds no
    /// permit. `Ok(Some)` and `Err` return holding the permit; `Ok(None)` (still held after
    /// `budget`, or lost the race) returns without it. Stage 1 of [`Vm::take_update_guard`] and
    /// [`Vm::guard_wait_block`] both call it.
    ///
    /// TICKET-200 — a wait that ends because the guard came free reads the halts
    /// ([`Vm::block_halts`]) before it takes the guard, because the free may be the halt's own cut
    /// of the holder (an `os.exit` or a cancel unwinding the holder's `update`). A halt returns
    /// `Err` holding no permit, like `Ok(None)`.
    fn guard_free_then_take(
        &mut self,
        key: usize,
        budget: std::time::Duration,
        span: Span,
    ) -> Result<Result<Option<core::UpdateGuard>, GuardCycle>, RuntimeError> {
        match core::await_update_guard_free(key, self.guard_token, Some(budget)) {
            Err(c) => return Ok(Err(c)),
            Ok(false) => return Ok(Ok(None)),
            Ok(true) => {}
        }
        self.block_halts(span)?;
        self.width_acquire();
        let zero = Some(std::time::Duration::ZERO);
        Ok(
            match acquire_update_guard_within(key, self.guard_token, zero) {
                Ok(None) => {
                    self.width_release();
                    Ok(None)
                }
                r => r,
            },
        )
    }

    /// TICKET-063 — the unbounded half of [`Vm::take_update_guard`], reached once the 5 ms bounded
    /// acquire has given up. Registers the wait as a waiter (never `inflight` — a guard wait has
    /// no promised external progress; see `SchedCore::waiters`' doc), registers this thread as a
    /// blocked party so the process-wide verdict can see it, and polls
    /// [`Vm::block_halt_check`] every [`super::DEMOTE_POLL_BACKOFF`] so `--timeout`, cancel and
    /// `os.exit` reach a guard waiter exactly like every other blocking-in-place site. The counters
    /// are un-accounted via `guard_wait_exit` on every exit path (no `?` before that call).
    fn guard_wait_block(
        &mut self,
        key: usize,
        what: &str,
        span: Span,
    ) -> Result<Result<core::UpdateGuard, GuardCycle>, RuntimeError> {
        let reg = self.guard_wait_enter(key, what, span)?;
        // TICKET-141 — the holder of this guard may be a preempted `update` closure on a gated
        // sibling thread; hold no width permit while the guard is busy. TICKET-193 — take the
        // permit back BEFORE the guard (the loop below).
        self.width_release();
        let _party = self.block_party_guard(
            quiesce::PartyWait::Guard(key, self.guard_token),
            Some((GUARD_DEADLOCK, span)),
        );
        let out = loop {
            // TICKET-193 — permit BEFORE guard. Wait for the guard to come free holding no permit,
            // take the permit, then take the guard without waiting. Taking the guard first parks
            // its OWNER behind permit holders that wait in place for that same guard, so every
            // handoff cost one GUARD_DEMOTE_BUDGET (T=2: 7172 timeouts, 18.9 s).
            match self.guard_free_then_take(key, super::DEMOTE_POLL_BACKOFF, span) {
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
                if c.cut_in_place() {
                    drop(c);
                    // The waiter names its own wait (the text and span its party site carries
                    // above), as on the process-wide verdict: a guard wait is not a channel wait.
                    break Err(sched.deadlock_err.at_site(GUARD_DEADLOCK, span));
                }
                if sched.is_deadlocked(&c) {
                    // TICKET-232 — the next pass ends the wait through the run halt or `terminate`.
                    drop(c);
                    sched.fault_own_deadlock();
                }
            }
        };
        self.guard_wait_exit(reg);
        self.width_acquire();
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

    /// `Atomic(v)` — pop the init, box its wire form behind a fresh `Arc<AtomicCore>`. `#[inline(never)]`
    /// so its locals stay out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    pub(super) fn new_atomic(&mut self, span: Span) -> Result<Value, RuntimeError> {
        let init = self.pop();
        // A non-sendable init (a frame-holding generator / module/native/FFI handle) faults gracefully
        // with the `NewAtomic` span — the box is a shared cross-thread cell.
        let init = self.to_wire_crossable(init, span)?;
        Ok(Value::obj(self.heap.alloc(Obj::Atomic(Arc::new(
            // The summary starts `WS_UNKNOWN` (like every other core constructor): the first GC
            // pass walks the initial payload once and memoizes it.
            AtomicCore {
                v: Mutex::new(init),
                ..Default::default()
            },
        )))))
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
