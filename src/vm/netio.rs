// vm::netio — split out of vm/mod.rs. `super::*` == the `vm` module.
// Channels, Shared/RwShared/Atomic, sockets/listeners, netpoller parks.

use super::core::{Pending, PendingOp, SendOutcome, Settled};
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
    _party: Option<quiesce::PartyGuard>,
    /// Whether this guard also raised `awaiting_builder` — a NESTED-JOIN block does, a channel block
    /// does not. See `MnSched::set_body_blocked`.
    awaiting: bool,
    bodies: Vec<(Arc<MnSched>, usize)>,
    /// This block's wait, published on every sched in `bodies` for the duration. See
    /// `SchedCore::waiters` — this is what keeps the `body_blocked` relaxation from false-faulting
    /// a rendezvous. `None` for a nested-join block, which waits on no channel.
    wait: Option<Arc<quiesce::PartyWait>>,
}

impl Drop for BlockGuard {
    fn drop(&mut self) {
        for (s, scope) in &self.bodies {
            s.set_body_wait(*scope, self.wait.as_ref(), false, self.awaiting);
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
const FULL_SEND_DEADLOCK: &str = "send on a full channel: deadlock — the bounded channel is at \
    capacity and no runnable task can receive to free a slot. (Make sure a task that receives from \
    this channel is spawned with `spawn:` and is still running.)";

/// The rendezvous (cap 0) sibling of [`FULL_SEND_DEADLOCK`] (TICKET-136, W14-35): a rendezvous
/// channel has no slots, so "at capacity" is false for it — the send can never complete because no
/// receiver is coming. Picked by [`send_deadlock_msg`] wherever the channel's cap is known.
const RENDEZVOUS_SEND_DEADLOCK: &str = "send on a rendezvous channel: deadlock — the channel has \
    no buffer and no runnable task can receive from it. (Make sure a task that receives from this \
    channel is spawned with `spawn:` and is still running.)";

/// The send-deadlock text for a channel of capacity `cap` (`Some(0)` = rendezvous).
/// The `Err` text of a would-block socket op in a context whose table cell is `Refuse` (an eager
/// `Executor` job, `block::mode`'s Socket/Connect row). One copy for every socket op.
fn sock_would_block_msg(op: &str) -> String {
    format!(
        "{op} would block: an Executor job doesn't own its thread — blocking here would starve \
         every other job and `parallel:` nursery sharing the pool. Do this socket op inside \
         `spawn:` or a `parallel:` nursery instead, where it parks rather than blocking a shared \
         thread."
    )
}

fn send_deadlock_msg(cap: Option<usize>) -> &'static str {
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
const CLOSED_RECV: &str = "receive on a closed channel";

/// The shared fault for a `recv` on an EMPTY channel that cannot park. Raised by the native-callback
/// arm of [`Vm::chan_recv_step`], which cannot block at all, and by any party that blocked in place
/// and was then judged deadlocked by the process-wide verdict ([`crate::vm::quiesce`]): ONE const so
/// every spelling of the same verdict is byte-identical.
const EMPTY_RECV_DEADLOCK: &str = "recv on an empty channel: deadlock — no runnable task can send. \
    (Make sure a task that sends to this channel is spawned with `spawn:` and is still running.)";

/// The `wait:` sibling of [`EMPTY_RECV_DEADLOCK`] — every arm empty and nobody left to send.
const EMPTY_WAIT_DEADLOCK: &str = "wait on channels that are all empty: deadlock — no runnable task \
    can send. (Make sure a task that sends to one of these channels is spawned with `spawn:` and is \
    still running.)";

/// Test-only instrumentation: how many waits [`Vm::block_wait_tick`] has performed, process-wide.
/// A COVERAGE floor for [`BLOCK_WAITS_SLEPT_WHILE_READY`] — "this program really did block on a
/// channel" — never a measurement: libtest runs the whole lib suite in ONE process, so a concurrent
/// test's blocking also lands here. Only ever read as a delta, and only ever compared with `>=`.
#[cfg(test)]
pub(crate) static BLOCK_WAITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// TICKET-134 — test-only: hold the window between the owner-fault rung and the verdict open until
/// the owner's nursery has recorded a fault. Fires once per run, only when the run's own
/// `HostConfig.env` carries this key, so no other lib test can trip it (libtest runs the whole lib
/// suite in one process, and `run_file_with` runs the VM on its own thread, so neither a global flag
/// nor a thread-local can be scoped to one test — the per-run `HostConfig.env` can).
#[cfg(test)]
pub(crate) const OWNER_FAULT_WINDOW_ENV: &str = "CHEZZI_TEST_OWNER_FAULT_WINDOW";

/// Test-only instrumentation: **W7-13's defect signature** — a [`Vm::block_wait_tick`] wait that
/// slept its whole [`DEMOTE_POLL_BACKOFF`] tick and yet found the channel READY when it woke, i.e. a
/// wakeup that was lost because it landed while nobody was on the condvar.
///
/// `wait_timeout_while` makes this UNREACHABLE by construction: it re-evaluates the predicate under
/// the guard before returning, so `timed_out()` can only be `true` while the predicate still says
/// NOT ready. The bare `wait_timeout` this replaced had no such guarantee, which is exactly the bug.
/// So a healthy build increments this ZERO times no matter how loaded the machine is — which is what
/// lets `eager_handshake_is_driven_by_wakeups_not_by_the_poll_timeout` assert on it directly, with no
/// wall-clock threshold to flake, and process-globally, with no neighbour able to pollute it (a
/// neighbour would have to hit the same defect, and then failing is right).
///
/// **Zero false positives depends on every readiness term being written under `core.q`**, because the
/// re-check runs while [`Vm::block_wait_tick`] still holds the guard the wait returned: a term some
/// other thread could flip in that window would be counted as a stall that never happened. All of
/// them are — a queued value and `closed` under `ChanState`, and `done_latch` since W7-13r(b) stores
/// it under `core.q` too. Move one back outside that lock and this counter turns flaky.
///
/// ONE EXCEPTION to "unreachable", found by review rather than reasoned away: on a POISONED `core.q`,
/// `wait_timeout_while` propagates the inner wait's `Err` WITHOUT running its post-wait re-check, so
/// the `into_inner` below can hand back `timed_out() == true` on an already-ready channel. Reaching it
/// needs another lib test to panic while holding some `ChannelCore::q` — a run that is already failing
/// (the bare `unwrap()`s elsewhere in this file panic on that same poison), so the cost is a
/// misleading second failure, not a false green.
#[cfg(test)]
pub(crate) static BLOCK_WAITS_SLEPT_WHILE_READY: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The eager `wait:` twin of [`BLOCK_WAITS`] — test-only instrumentation of how many times
/// `op_wait_poll`'s blocking arm-0 wait ran, process-wide. A COVERAGE floor for
/// [`WAIT_ARM0_SLEPT_WHILE_READY`], never a measurement, for the same reason `BLOCK_WAITS` isn't:
/// libtest runs the whole lib suite in one process, so a concurrent test's waits also land here.
/// Only ever read as a delta, and only ever compared with `>=` (TICKET-059).
#[cfg(test)]
pub(crate) static WAIT_ARM0_BLOCKS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The eager `wait:` twin of [`BLOCK_WAITS_SLEPT_WHILE_READY`] — a `wait_timeout_while` that slept its
/// whole tick and yet found arm 0 already ready when it woke, i.e. a lost wakeup. Zero by
/// construction on a healthy build for the same reason as its sibling: `wait_timeout_while`
/// re-evaluates the predicate under the guard before returning. Only ever compared with `== 0`
/// (TICKET-059).
#[cfg(test)]
pub(crate) static WAIT_ARM0_SLEPT_WHILE_READY: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

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
fn deadline_gap_wake(
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
fn write_from(
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
            _ => unreachable!("channel_core on non-channel"),
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
                    match self.guard_free_then_take(key, left) {
                        Ok(Some(g)) => break Some(Ok(g)),
                        Err(c) => break Some(Err(c)),
                        Ok(None) if left.is_zero() => break None,
                        Ok(None) => {}
                    }
                };
                self.width_acquire();
                match staged {
                    Some(r) => r,
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
    fn guard_free_then_take(
        &mut self,
        key: usize,
        budget: std::time::Duration,
    ) -> Result<Option<core::UpdateGuard>, GuardCycle> {
        if !core::await_update_guard_free(key, self.guard_token, Some(budget))? {
            return Ok(None);
        }
        self.width_acquire();
        match acquire_update_guard_within(key, self.guard_token, Some(std::time::Duration::ZERO)) {
            Ok(None) => {
                self.width_release();
                Ok(None)
            }
            r => r,
        }
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
        let _party = self.block_party_guard(quiesce::PartyWait::Guard(key, self.guard_token));
        let out = loop {
            // TICKET-193 — permit BEFORE guard. Wait for the guard to come free holding no permit,
            // take the permit, then take the guard without waiting. Taking the guard first parks
            // its OWNER behind permit holders that wait in place for that same guard, so every
            // handoff cost one GUARD_DEMOTE_BUDGET (T=2: 7172 timeouts, 18.9 s).
            match self.guard_free_then_take(key, super::DEMOTE_POLL_BACKOFF) {
                Ok(Some(g)) => break Ok(Ok(g)),
                Err(cycle) => break Ok(Err(cycle)),
                Ok(None) => {}
            }
            if let Err(e) = self.block_halt_check(super::DEADLOCK_MSG, span) {
                break Err(e);
            }
            if let Some(sched) = self.mn.as_ref().map(Arc::clone) {
                let mut c = sched.lock();
                if c.terminate {
                    drop(c);
                    break Err(sched.deadlock_err.clone());
                }
                if sched.is_deadlocked(&c) {
                    c.flag_deadlock(&sched.deadlock_err);
                    drop(c);
                    sched.notify_waiters();
                    break Err(sched.deadlock_err.clone());
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

    /// `timer(ms)` — pop the `ms` int, push a fresh `Channel[bool]` stamped with `now + ms`. Delivery is
    /// handled at `recv` time (in the receiver's scheduler), NOT here, so a timer made at the top level
    /// can be `recv`'d inside a `--parallel` child. `#[inline(never)]` so the `Instant`/`Duration` math
    /// stays out of `step`'s (recursion-path) stack frame.
    #[inline(never)]
    pub(super) fn new_timer(&mut self, span: Span) -> Result<Value, RuntimeError> {
        let v = self.pop();
        let ms = if let Some(ms) = self.int_val(v) {
            ms.max(0) as u64
        } else {
            return Err(self.err(
                format!("timer(ms) expects int, got {}", self.type_name(v)),
                span,
            ));
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

    /// D6 — clone out the shared `Arc<SocketCore>`/`Arc<ListenerCore>` behind a handle (refcount bump),
    /// mirroring [`channel_core`](Vm::channel_core). The `Arc` is held only for the calling method, so
    /// locking the fd does not borrow the heap.
    pub(super) fn socket_core(&self, h: GcRef) -> Arc<SocketCore> {
        match self.heap.get(h) {
            Obj::Socket(core) => Arc::clone(core),
            _ => unreachable!("socket_core on non-socket"),
        }
    }

    core_accessor!(listener_core, Listener, ListenerCore);

    /// D6 — build a `Result::Ok(v)` / `Result::Err(msg)` for a socket op (mirrors `lower_native`'s
    /// `Ok`/`Err` arms — the surface contract is `read/write/accept -> Result`).
    pub(super) fn sock_ok(&mut self, v: Value) -> Value {
        self.alloc_enum("Result", "Ok", vec![v])
    }
    pub(super) fn sock_err(&mut self, msg: impl Into<String>) -> Value {
        let ev = self.alloc_str(msg.into());
        self.alloc_enum("Result", "Err", vec![ev])
    }

    /// N3(a) — the ONE builder for the "read took a partial codepoint then could not finish it" error,
    /// shared by every path that can hit it: the poll-once return, the netpoller-park timeout re-entry,
    /// and the in-callback demote timeout. `owed` is the carried (retained) byte count. Distinct from
    /// `Err("timeout")` — which is documented as "nothing arrived" — because 1-3 bytes ARE off the wire
    /// (kept on the socket), so a retry finishes the codepoint byte-exactly.
    pub(super) fn sock_incomplete_err(&mut self, owed: usize) -> Value {
        self.sock_err(format!(
            "incomplete utf-8: the read landed mid-codepoint ({owed} byte(s) carried and retained) — \
             read this socket again to finish it"
        ))
    }

    /// N2/N3(a) — drop the per-op fiber latches (`poll_deadline` timeout budget + `poll_partial`
    /// taken-partial flag) once a socket op is over, UNLESS it parked (`poll_park` set ⇒ the very same
    /// call resumes and still owns them). Called from every socket/listener op arm so the NEXT op on
    /// this fiber starts with a fresh budget and no stale partial flag. Symmetric-clear is load-bearing:
    /// a leaked `poll_deadline` would corrupt the next read's timeout, a leaked `poll_partial` would
    /// make it lie "incomplete" when nothing arrived.
    pub(super) fn drop_poll_latch(&mut self) {
        if self.poll_park.is_none() {
            self.poll_deadline = None;
            self.poll_partial = None;
            self.poll_written = None;
        }
    }

    /// B1 — decode ONE `read` chunk off a socket into the `Result[str]` the surface promises. The ONLY
    /// decode point on the socket path (both the fast path and the in-callback demote poller route here
    /// — a second copy is how the lossy decode got duplicated in the first place).
    ///
    /// `String::from_utf8_lossy` used to sit at both sites: it silently turned every non-UTF-8 byte into
    /// U+FFFD (binary payloads corrupted, no error) AND mangled perfectly valid text whose codepoint
    /// straddled the `read(n)` boundary (the ordinary read-in-a-loop idiom). The two cases are distinct
    /// and `Utf8Error` tells them apart:
    /// * `error_len() == None` — TRUNCATED tail: the bytes so far are a valid prefix of a codepoint. Keep
    ///   the (≤3-byte) tail in [`SocketCore::carry`] and prepend it to the next read. If the chunk holds
    ///   no complete codepoint at all, return `None` = "need more bytes" — the caller re-reads (it must
    ///   NOT return `Ok("")`, which every `while chunk != "":` loop reads as EOF).
    /// * `error_len() == Some(_)` — genuinely INVALID bytes: a recoverable `Err` naming the real limit
    ///   (the str seam decodes; binary payloads are read with `read_bytes`), never a silent U+FFFD.
    ///
    /// NON-DESTRUCTIVE either way: the valid text that decoded BEFORE the problem byte is always handed
    /// back, and everything from the problem byte on stays in [`SocketCore::carry`]. So an invalid-utf-8
    /// `Err` is STICKY (the same bytes re-decode and re-err on the next read) rather than eating the
    /// chunk — a recoverable `Err` that silently drops up to `MAX_SOCKET_READ` of already-received
    /// payload would just be a different flavour of the corruption B1 fixes.
    ///
    /// `eof` (the fd returned 0 bytes for a `read(n>0)`) keeps today's `Ok("")` EOF sentinel — unless a
    /// carry is still owed, i.e. the peer closed mid-codepoint: that is a real error, not a silent drop.
    ///
    /// PURE (no `&mut self`, no locking): the caller holds the `carry` guard ACROSS its fd read and
    /// passes `&mut *guard` here, so read-off-the-fd and carry-update are ONE critical section (two
    /// fibers aliasing one socket must decode in wire order — see [`SocketCore::carry`]).
    pub(super) fn decode_carry(carry: &mut Vec<u8>, chunk: &[u8], eof: bool) -> Decoded {
        if eof {
            let owed = carry.len();
            carry.clear();
            return if owed == 0 {
                Decoded::Text(String::new()) // the EOF sentinel, unchanged
            } else {
                Decoded::Fail(format!(
                    "invalid utf-8 at eof: the peer closed mid-codepoint ({owed} trailing byte(s))"
                ))
            };
        }
        // The hot path (no carry, a fully-valid chunk) decodes the fd's buffer BORROWED — one alloc
        // for the resulting `String`, exactly what `from_utf8_lossy(..).into_owned()` cost. Only a
        // pending carry has to splice, and a carry is ≤3 bytes on every non-error path.
        let bytes: std::borrow::Cow<'_, [u8]> = if carry.is_empty() {
            std::borrow::Cow::Borrowed(chunk)
        } else {
            let mut b = std::mem::take(carry);
            b.extend_from_slice(chunk);
            std::borrow::Cow::Owned(b)
        };
        let err = match std::str::from_utf8(&bytes) {
            Ok(s) => return Decoded::Text(s.to_string()),
            Err(e) => e,
        };
        // Whatever decoded BEFORE the problem byte is real text the peer sent: hand it back. Never
        // drop it — a `read` that consumed bytes off the fd and returned neither them nor an error
        // about them is silent data loss (the exact family B1 exists to kill).
        let good = err.valid_up_to();
        let prefix = std::str::from_utf8(&bytes[..good]).expect("valid_up_to prefix is utf-8");
        let prefix = Decoded::Text(prefix.to_string());
        // Everything from the problem byte on stays CARRIED — nothing is consumed-and-dropped.
        //   * truncated tail (`error_len() == None`): ≤3 bytes, the next read prepends them and the
        //     codepoint completes.
        //   * genuinely INVALID bytes (`error_len() == Some(_)`): a str-only seam can never hand them
        //     back, so the carry makes the `Err` STICKY — every later read re-decodes the same bytes
        //     and re-errs identically. A caller that logs the Err and keeps reading (what a `Result`
        //     invites) therefore cannot silently shred the stream; it must `close()`.
        *carry = bytes[good..].to_vec();
        match (err.error_len(), good) {
            // Truncated: nothing complete yet ⇒ the caller must take more bytes off the fd.
            (None, 0) => Decoded::NeedMore,
            (None, _) => prefix,
            // Invalid, but valid text came first: deliver that text now; the bad bytes re-err next read.
            (Some(_), 1..) => prefix,
            (Some(_), _) => Decoded::Fail(
                "invalid utf-8 on the socket: std.net read is str-only — read binary payloads with \
                 Socket.read_bytes. The bytes stay carried (read_bytes hands them back byte-exactly), \
                 so every str read on this socket now returns this error"
                    .to_string(),
            ),
        }
    }

    /// B1 — materialize a [`Decoded`] into the `Result[str]` value (`None` = NeedMore: the caller must
    /// take more bytes off the fd). Split from [`Vm::decode_carry`] so the allocating half runs with no
    /// socket lock held.
    pub(super) fn decoded_value(&mut self, d: Decoded) -> Option<Value> {
        match d {
            Decoded::NeedMore => None,
            Decoded::Text(s) => {
                let sv = self.alloc_str(s);
                Some(self.sock_ok(sv))
            }
            Decoded::Fail(m) => Some(self.sock_err(m)),
        }
    }

    /// D6 — `std.net.connect(addr)` / `listen(addr)`: allocate a non-blocking `Socket`/`Listener`
    /// handle (or a `Result::Err` on a bad address / bind failure). Intercepted in `invoke_native`
    /// because it allocates a heap handle over an `Arc`'d core — a pure off-heap native can't.
    ///
    /// D6b — `connect` is now a **true non-blocking** connect: an in-progress handshake (`EINPROGRESS`)
    /// parks the fiber on the socket's writability rather than pinning a worker for the round trip. The
    /// connecting socket is stashed in `pending_connect`; the netpoller wakes the fiber on writability
    /// and [`Vm::run_one_fiber`] completes it via [`Vm::finish_pending_connect`] (read `SO_ERROR`) and
    /// pushes the resulting `Socket` — the bytecode call site never re-runs. The instant (loopback)
    /// case still returns immediately.
    pub(super) fn net_connect_or_listen(
        &mut self,
        name: &str,
        args: Vec<Value>,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let addr = if let Some(v) = args.first()
            && let Some(sh) = v.as_obj()
            && let Obj::Str(s) = self.heap.get(sh)
        {
            s.to_string()
        } else {
            return Err(self.err(format!("std.net.{name} expects an address string"), span));
        };
        match name {
            "connect" => match crate::native::net::connect_nonblocking(&addr) {
                // Connected synchronously — wrap + return at once. RARE, and this comment used to
                // claim it was "the common loopback case", which is measured false on Linux: a
                // non-blocking `connect` reports `EINPROGRESS` even to a LIVE loopback listener
                // (and to a closed loopback port — `connect_ex(('127.0.0.1', <closed>))` → `115`).
                // The arm below is the normal path, not the fallback, which is why W7-59's gate
                // choice there decides `net.connect`'s whole behaviour rather than a corner of it.
                Ok((stream, false)) => {
                    Ok(self.alloc_socket_ok(stream, core::next_poll_key(), core::new_in_flight()))
                }
                // Handshake in flight: park the fiber on writability under the M:N engine; off it,
                // block until the handshake settles — everywhere except an eager `Executor` job,
                // whose thread is shared (W7-59).
                Ok((stream, true)) => {
                    let connect_mode = self.block_mode(WaitSpec::Connect);
                    if connect_mode == BlockMode::Park {
                        self.park_on_connect(stream, span);
                        Ok(Value::nil()) // parked sentinel; `poll_park` gates the result-push at `do_call`
                    } else if connect_mode == BlockMode::Refuse {
                        // W7-59 — an eager `Executor` job. A job does not own its thread: it runs on the
                        // bounded, process-wide `vm::pool` (`worker_count()`, never grown on demand) with
                        // no `MnSched` under it to spin a replacement, so blocking here steals width from
                        // every other job and every `parallel:` nursery sharing that pool — measured at
                        // `CHEZZI_THREADS=1` as a 10 s pin on a black-hole address. Same family as
                        // `W7-40`'s R2, and the same message the four sibling ops give this context.
                        //
                        // This `Connect` row is deliberately NARROWER than the siblings' `Socket`
                        // row of `block::mode`, and the difference is not an oversight.
                        // `accept`/`read` wait on a CHEZZI peer — a fiber that can only run on the very
                        // thread they would block — so blocking the one thread that owns it is
                        // self-starvation (`W7-40` R1). A `connect` handshake is completed by the
                        // KERNEL, so no chezzi party is starved by waiting for it, and both ancestors
                        // block: CPython `socket.connect` from the main thread returns in 0.1 ms, Go
                        // `net.Dial` from the main goroutine in 314 µs — refusing it here would be a
                        // divergence with nothing behind it.
                        Ok(self.sock_err(sock_would_block_msg("connect")))
                    } else {
                        // Everywhere the thread is the program's own — top-level `main` on the
                        // default engine, a `connect` inside a native callback on M:N: block, but
                        // through the SHARED demote loop rather than a private sleep-spin, so the wait
                        // gets that loop's escapes (`--timeout`, `cancel`, a run-wide `os.exit` — W7-47 —
                        // and a torn-down nursery) and, on a worker shell, `block_enter`'s
                        // replacement worker.
                        //
                        // The 10 s connect cap is deliberately NOT clamped by `self.deadline`, unlike the
                        // spin this replaces. `demote_block_socket` re-reads the run deadline at the top
                        // of every iteration and caps its kernel wait at `DEMOTE_POLL_BACKOFF`, so a
                        // `--timeout` is observed within 5 ms and raised as a HARD `Err`. Clamping would
                        // make the op's OWN deadline expire in the same instant, and the op's expiry is
                        // the CATCHABLE `Err("timeout")` — i.e. the clamp would turn W7-18's swallow into
                        // a race instead of preventing it.
                        let dl = std::time::Instant::now()
                            + std::time::Duration::from_secs(CONNECT_BLOCK_TIMEOUT_SECS);
                        let fd = stream.as_raw_fd();
                        // The closure is `FnMut`, so it cannot move `stream` out on the ready edge —
                        // hold it in an `Option` and `take()` it there. It must outlive the wait either
                        // way: it owns the fd the poller watches.
                        let mut pending = Some(stream);
                        let v = self.demote_block_socket(
                            fd,
                            poller::Interest::Write,
                            Some(dl),
                            span,
                            move |vm| {
                                let Some(s) = pending.as_ref() else {
                                    // Unreachable: the ready edge below is the only `take`, and it also
                                    // ends the loop.
                                    return SockPoll::Ready(Ok(
                                        vm.sock_err("connect failed: already completed")
                                    ));
                                };
                                match crate::native::net::finish_connect(s) {
                                    // SO_ERROR clear AND the peer is reachable ⇒ connected.
                                    Ok(()) if s.peer_addr().is_ok() => {
                                        let s = pending.take().expect("checked above");
                                        SockPoll::Ready(Ok(vm.alloc_socket_ok(
                                            s,
                                            core::next_poll_key(),
                                            core::new_in_flight(),
                                        )))
                                    }
                                    Err(e) => SockPoll::Ready(Ok(
                                        vm.sock_err(format!("connect failed: {e}"))
                                    )),
                                    Ok(()) => SockPoll::WouldBlock, // not settled yet
                                }
                            },
                        )?;
                        // W7-18 — kept as a fence, no longer the mechanism. `demote_block_socket`'s own
                        // rung raises the run deadline as a HARD `Err` within 5 ms, so this fires only
                        // if a future change re-clamps the op deadline (see above) or reorders that
                        // loop's two deadline checks — either of which would let a `--timeout` come
                        // back as the CATCHABLE `Err("timeout")` and be swallowed by a `recover:`,
                        // exactly what the hard-abort contract forbids. It costs one `Instant::now()`,
                        // and only when a `--timeout` is armed at all.
                        self.deadline_halt(span)?;
                        Ok(v)
                    }
                }
                Err(e) => Ok(self.sock_err(format!("{addr}: {e}"))),
            },
            "listen" => match crate::native::net::listen_nonblocking(&addr) {
                Ok(listener) => {
                    let core = Arc::new(ListenerCore {
                        listener: Mutex::new(Some(listener)),
                        key: core::next_poll_key(),
                        in_flight: core::new_in_flight(),
                        closed: core::new_closed(),
                    });
                    let v = Value::obj(self.heap.alloc(Obj::Listener(core)));
                    Ok(self.sock_ok(v))
                }
                Err(e) => Ok(self.sock_err(format!("{addr}: {e}"))),
            },
            _ => unreachable!("net_connect_or_listen on '{name}'"),
        }
    }

    /// D6b — wrap a connected `TcpStream` in a `Socket` handle and return `Ok(Socket)`. `key`/`in_flight`
    /// become the socket's poll identity for later `read`/`write` parks (a fresh pair for a synchronous
    /// connect; the connect's own pair, reused, for one that parked — its `in_flight` was cleared on
    /// inject).
    pub(super) fn alloc_socket_ok(
        &mut self,
        stream: std::net::TcpStream,
        key: usize,
        in_flight: Arc<AtomicBool>,
    ) -> Value {
        let core = Arc::new(SocketCore {
            stream: Mutex::new(Some(stream)),
            key,
            in_flight,
            closed: core::new_closed(),
            carry: Mutex::new(Vec::new()),
        });
        let v = Value::obj(self.heap.alloc(Obj::Socket(core)));
        self.sock_ok(v)
    }

    /// D6b — finish a connect that parked on writability: `SO_ERROR` clear ⇒ `Ok(Socket)`, else
    /// `Err(msg)`. Reuses the connect's poll key + guard so the resulting socket keeps a stable identity.
    pub(super) fn finish_pending_connect(&mut self, cip: ConnectInProgress) -> Value {
        match crate::native::net::finish_connect(&cip.stream) {
            Ok(()) => self.alloc_socket_ok(cip.stream, cip.key, cip.in_flight),
            Err(e) => self.sock_err(format!("connect failed: {e}")),
        }
    }

    /// D6b — park the current fiber on a connecting socket's writability. Stash the connecting stream
    /// in `pending_connect` (it owns the fd the poller will watch, so it must outlive the park) and set
    /// the `poll_park` sentinel; the worker loop hands both to the netpoller. Unlike a `read`/`write`
    /// park there is NO `ip` rewind — `net.connect`'s call site already popped its args and pushed
    /// nothing (`do_call` saw `paused()`), so on resume [`Vm::run_one_fiber`] finishes the connect and
    /// pushes the `Socket` exactly where the call would have, and execution continues past the call.
    pub(super) fn park_on_connect(&mut self, stream: std::net::TcpStream, span: Span) {
        let key = core::next_poll_key();
        let in_flight = core::new_in_flight();
        in_flight.store(true, Ordering::Release); // mark parked (matches `park_on_fd`'s swap(true))
        let fd = stream.as_raw_fd();
        self.pending_connect = Some(ConnectInProgress {
            stream,
            key,
            in_flight: Arc::clone(&in_flight),
            span,
        });
        // A `connect` never carries a user timeout (the `connect` surface takes only an address), so it
        // parks until readiness, a `drain_family` re-inject on a sibling fault — or, W7-18, the RUN's
        // `--timeout` deadline, which is the only thing that can set `deadline` here. That makes
        // `poll_timed_out` on a connect resume unambiguous: it is always the hard halt, never an op
        // timeout, and the `pending_connect` arm in `run_one_fiber` raises it as one.
        self.poll_park = Some(PollPark {
            key,
            fd,
            interest: poller::Interest::Write,
            in_flight,
            closed: core::new_closed(),
            deadline: self.deadline,
        });
    }

    /// R1/B1 — `Socket.read_bytes(n) -> Result[bytes]` / `read_bytes(n, timeout_ms)`: the BINARY read.
    /// No decode at all, so no carry/`NeedMore` loop — the contract is the natural byte one: it returns
    /// AT MOST `n` bytes (unlike the str `read`, whose `n` bounds only the NEW fd bytes and which may
    /// hand back up to `n + 3` when it prepends a carried codepoint tail).
    ///
    /// It DRAINS the carry first: bytes a previous str `read` left behind — including the undecodable
    /// bytes its sticky `Err("invalid utf-8 …")` refused to deliver — are handed back here byte-exactly
    /// (that is the escape hatch the str Err points at; ignoring the carry would make a mixed
    /// `read`/`read_bytes` socket silently lossy). Only when the carry is empty does it touch the fd.
    /// `got == 0` on a `read_bytes(n>0)` is EOF → `Ok(b"")`. `read_bytes(0)` is a no-op `Ok(b"")` that
    /// still errs on a CLOSED socket (same shape as `read(0)`). Would-block: park on the netpoller /
    /// demote in-callback / fail loud off the M:N engine, exactly like `read`, with the same LATCHED
    /// `poll_deadline` (a re-park must not re-arm the timeout budget).
    fn socket_read_bytes(
        &mut self,
        h: GcRef,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        self.arity_range_err("read_bytes", args, 1, 2, span)?;
        if self.poll_timeout_check(span)? {
            return Ok(self.sock_err("timeout"));
        }
        let timeout = self.parse_timeout_ms(args.get(1), span)?;
        let n = match args.first() {
            Some(v) if self.is_integral(*v) => {
                (self.int_of(*v).max(0) as usize).min(MAX_SOCKET_READ)
            }
            _ => return Err(self.err("read_bytes expects an int byte count".into(), span)),
        };
        let core = self.socket_core(h);
        if n == 0 {
            if core
                .stream
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
            {
                return Ok(self.sock_err("read_bytes on a closed socket"));
            }
            let bv = Value::obj(self.heap.alloc(Obj::Bytes(Box::default())));
            return Ok(self.sock_ok(bv));
        }
        let deadline = timeout
            .filter(|t| !t.poll_once)
            .map(|t| *self.poll_deadline.get_or_insert(t.deadline));
        let mut buf = vec![0u8; n];
        let attempt = {
            // LOCK ORDER: `carry` OUTER, `stream` INNER (see `SocketCore::carry`).
            let mut carry = core.carry.lock().unwrap_or_else(|e| e.into_inner());
            let mut guard = core.stream.lock().unwrap_or_else(|e| e.into_inner());
            let Some(stream) = guard.as_mut() else {
                return Ok(self.sock_err("read_bytes on a closed socket"));
            };
            if !carry.is_empty() {
                let take = n.min(carry.len());
                Ok(carry.drain(..take).collect::<Vec<u8>>())
            } else {
                match std::io::Read::read(stream, &mut buf) {
                    Ok(got) => Ok(buf[..got].to_vec()),
                    Err(e) => Err((e, stream.as_raw_fd())),
                }
            }
        };
        // Allocate only AFTER the locks drop.
        match attempt {
            Ok(bytes) => {
                let bv = Value::obj(self.heap.alloc(Obj::Bytes(bytes.into_boxed_slice())));
                Ok(self.sock_ok(bv))
            }
            Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if timeout.is_some_and(|t| t.poll_once) {
                    return Ok(self.sock_err("timeout"));
                }
                let target = PollPark {
                    key: core.key,
                    fd,
                    interest: poller::Interest::Read,
                    in_flight: Arc::clone(&core.in_flight),
                    closed: Arc::clone(&core.closed),
                    deadline,
                };
                if let Some(v) = self.park_on_fd(h, args, target, "read_bytes", span)? {
                    return Ok(v); // parked (sentinel), or refused (`would block`)
                }
                // No fiber to park: block the thread in place, where that starves nobody
                // (the `Socket` row of `block::mode`; `park_on_fd` returned the refusal elsewhere).
                self.demote_block_socket(fd, poller::Interest::Read, deadline, span, move |vm| {
                    let mut b = vec![0u8; n];
                    let r = {
                        let mut carry = core.carry.lock().unwrap_or_else(|e| e.into_inner());
                        let mut guard = core.stream.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(stream) = guard.as_mut() else {
                            return SockPoll::Ready(Ok(
                                vm.sock_err("read_bytes on a closed socket")
                            ));
                        };
                        if !carry.is_empty() {
                            let take = n.min(carry.len());
                            Ok(carry.drain(..take).collect::<Vec<u8>>())
                        } else {
                            match std::io::Read::read(stream, &mut b) {
                                Ok(got) => Ok(b[..got].to_vec()),
                                Err(e) => Err(e),
                            }
                        }
                    };
                    match r {
                        Ok(bytes) => {
                            let bv =
                                Value::obj(vm.heap.alloc(Obj::Bytes(bytes.into_boxed_slice())));
                            SockPoll::Ready(Ok(vm.sock_ok(bv)))
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            SockPoll::WouldBlock
                        }
                        Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                    }
                })
            }
            Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
        }
    }

    /// D6/B1 — `Socket.read(n) -> Result[str]` / `read(n, timeout_ms)`. On a would-block, on an M:N
    /// worker shell the fiber PARKS on the netpoller (re-root the receiver, rewind `ip` so the op
    /// re-executes on resume, set the `poll_park` sentinel — mirrors the channel `recv` park, but
    /// routed to the poller). Off a worker shell (top level) there is no fiber to park, so the op
    /// fails loud (a documented v1 fallback — net targets `--parallel`).
    ///
    /// Decodes through [`Vm::decode_carry`] (never `from_utf8_lossy`). Contract: `n` bounds the NEW
    /// bytes taken off the fd; a ≤3-byte incomplete-codepoint tail carried from the previous read is
    /// prepended, so a `read(n)` can return up to `n + 3` bytes and never fewer than the peer sent.
    /// `read(0)` is a no-op `Ok("")` — it never touches the fd and never turns a pending carry into a
    /// false EOF, but it still reports a CLOSED socket (`Ok("")` there would be indistinguishable from
    /// the EOF sentinel).
    ///
    /// A `read` whose bytes end mid-codepoint may BLOCK past its first successful fd read: it needs the
    /// rest of that codepoint, because a str-only seam cannot hand back half a character. (This is the
    /// same contract as Go's `bufio.Reader.ReadRune` and Python's text-mode socket file: a text reader
    /// blocks for a whole rune. The peer OWES those 1–3 bytes; the escapes are `timeout_ms` and the
    /// peer's close, which errors rather than dropping the tail.) `timeout_ms` bounds the WHOLE call on
    /// EVERY path — the netpoller park (the deadline is latched on the fiber, [`Vm::poll_deadline`], so
    /// re-parking to finish a codepoint does NOT restart the budget) and the in-callback demote loop
    /// ([`Vm::demote_block_socket`]) alike. The carry is RETAINED across a timeout `Err` (those bytes
    /// are still owed to the next read), and a poll-once (`timeout_ms == 0`) read that took a partial
    /// codepoint says so — `Err("incomplete utf-8: …")`, not the `Err("timeout")` that means "nothing
    /// arrived".
    fn socket_read(&mut self, h: GcRef, args: &[Value], span: Span) -> Result<Value, RuntimeError> {
        // The optional trailing int bounds readiness (D6c). On a timeout the netpoller re-injects this
        // fiber with `poll_timed_out` set; the rewound op re-runs and lands HERE — so check it at entry
        // (after the `run_until` loop-top cancel check, which a sibling fault wins) and return Err.
        self.arity_range_err("read", args, 1, 2, span)?;
        if self.poll_timeout_check(span)? {
            // N3(a) — if this read took a partial codepoint off the wire before the deadline fired
            // (`poll_partial` was latched at the NeedMore point and survived the park), classify it as
            // `incomplete utf-8` (those bytes are carried/retained), NOT `timeout` (which means nothing
            // arrived). Same rule as the poll-once path below.
            return Ok(match self.poll_partial {
                Some(owed) => self.sock_incomplete_err(owed),
                None => self.sock_err("timeout"),
            });
        }
        let timeout = self.parse_timeout_ms(args.get(1), span)?;
        // Cap the per-call buffer: a huge `read(n)` (caller-controlled) must not eagerly allocate
        // gigabytes before a byte arrives (review). The caller already loops for large payloads —
        // `read` returns the actual count.
        let n = match args.first() {
            Some(v) if self.is_integral(*v) => {
                (self.int_of(*v).max(0) as usize).min(MAX_SOCKET_READ)
            }
            _ => return Err(self.err("read expects an int byte count".into(), span)),
        };
        let core = self.socket_core(h);
        // `read(0)` (or a negative / caller-computed-to-zero `n`): nothing to take off the fd. Return
        // the empty string WITHOUT reading — a zero-length `Read::read` returns `Ok(0)` unconditionally,
        // so feeding it to the decode/re-read loop below would spin forever whenever a carry is pending
        // (it can neither make progress nor would-block). It must STILL report a closed socket, though:
        // the stream lock's `None` arm is the only closed-fd detector on this path, and answering `Ok("")`
        // for a closed socket is indistinguishable from the EOF sentinel (review #1).
        if n == 0 {
            if core
                .stream
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
            {
                return Ok(self.sock_err("read on a closed socket"));
            }
            let sv = self.alloc_str(String::new());
            return Ok(self.sock_ok(sv));
        }
        // The per-call deadline, latched on the fiber so it survives a park's ip-rewind re-execution.
        let deadline = timeout
            .filter(|t| !t.poll_once)
            .map(|t| *self.poll_deadline.get_or_insert(t.deadline));
        // B1 — the loop only re-reads when the fd's bytes ended mid-codepoint AND held no complete
        // codepoint at all (`Decoded::NeedMore`): take more bytes for the rest of it. It is bounded —
        // a NeedMore carry is a strict prefix of ONE codepoint (≤3 bytes), so at most 3 data-bearing
        // re-reads can NeedMore before the codepoint completes (or the bytes are invalid ⇒ `Fail`).
        // The ordinary outcome of the re-read is WouldBlock, which falls into the park/demote branch
        // below (every arm of which returns).
        let mut buf = vec![0u8; n];
        // Did THIS call take bytes off the fd that completed no codepoint? Then a would-block is not a
        // "no data arrived" timeout, and saying so would be a lie (review #3/#8).
        let mut took_partial = false;
        loop {
            let attempt = {
                // LOCK ORDER: `carry` OUTER, `stream` INNER — the fd read and the carry update are ONE
                // critical section, or two fibers aliasing this socket decode out of wire order (a
                // valid multibyte stream would then err as "invalid utf-8"). See `SocketCore::carry`.
                let mut carry = core.carry.lock().unwrap_or_else(|e| e.into_inner());
                let mut guard = core.stream.lock().unwrap_or_else(|e| e.into_inner());
                let Some(stream) = guard.as_mut() else {
                    return Ok(self.sock_err("read on a closed socket"));
                };
                // A carry that ALREADY decides must not wait on the fd first. After an invalid-utf-8
                // `Err` the offending bytes STAY carried (they are undeliverable through a str seam),
                // so a peer that sends nothing more would otherwise park us forever on bytes we already
                // hold. Re-decoding the carry against an EMPTY chunk settles it: `Fail` (sticky Err) or
                // `NeedMore` (a genuine truncated tail ⇒ go take its remaining bytes).
                let decided = match carry.is_empty() {
                    true => None,
                    false => match Self::decode_carry(&mut carry, &[], false) {
                        Decoded::NeedMore => None,
                        d => Some(d),
                    },
                };
                match decided {
                    Some(d) => Ok(d),
                    None => match std::io::Read::read(stream, &mut buf) {
                        // `got == 0` on a `read(n>0)` is EOF.
                        Ok(got) => Ok(Self::decode_carry(&mut carry, &buf[..got], got == 0)),
                        Err(e) => Err((e, stream.as_raw_fd())),
                    },
                }
            };
            match attempt {
                Ok(d) => match self.decoded_value(d) {
                    Some(v) => return Ok(v),
                    None => {
                        // Incomplete codepoint carried; take the rest of it off the fd.
                        took_partial = true;
                        // N3(a) — latch the taken-partial state on the fiber so a later timeout (the
                        // netpoller-park re-entry, which re-executes this op after `took_partial` is
                        // lost) reports `incomplete utf-8` instead of `timeout`. `owed` = the carried
                        // (retained) bytes; a fresh short lock, like the poll-once path.
                        self.poll_partial =
                            Some(core.carry.lock().unwrap_or_else(|e| e.into_inner()).len());
                        continue;
                    }
                },
                Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // `timeout_ms == 0` (poll-once): do NOT park — answer immediately. If this poll took
                    // a partial codepoint, say THAT: `Err("timeout")` is documented as "no data within
                    // timeout_ms", and reporting a deadline expiry for a read that removed 1-3 bytes from
                    // the wire is the same lie-about-your-data class B1 exists to kill. The bytes are
                    // retained on the socket, so a retry finishes the codepoint byte-exactly.
                    if timeout.is_some_and(|t| t.poll_once) {
                        if took_partial {
                            let owed = core.carry.lock().unwrap_or_else(|e| e.into_inner()).len();
                            return Ok(self.sock_incomplete_err(owed));
                        }
                        return Ok(self.sock_err("timeout"));
                    }
                    let target = PollPark {
                        key: core.key,
                        fd,
                        interest: poller::Interest::Read,
                        in_flight: Arc::clone(&core.in_flight),
                        closed: Arc::clone(&core.closed),
                        deadline,
                    };
                    if let Some(v) = self.park_on_fd(h, args, target, "read", span)? {
                        return Ok(v); // parked (sentinel; `poll_park` gates the push), or refused
                    }
                    // No netpoller-park: inside a native callback on M:N (`native_reentry > 0`, the
                    // Rust-stack `map`/sort loop can't snapshot-park) → DEMOTE + backoff-poll the
                    // non-blocking read in place (#3 socket half); top-level `main` on the default
                    // engine blocks in place too (Go-identical). Anywhere else the calling thread is
                    // shared, so blocking it starves the peer that would make the fd ready, and
                    // `park_on_fd` already returned the loud refusal (a `Refuse` cell of the `Socket`
                    // row of `block::mode`).
                    return self.demote_block_socket(
                        fd,
                        poller::Interest::Read,
                        deadline,
                        span,
                        move |vm| {
                            let mut b = vec![0u8; n];
                            let r = {
                                let mut carry =
                                    core.carry.lock().unwrap_or_else(|e| e.into_inner());
                                let mut guard =
                                    core.stream.lock().unwrap_or_else(|e| e.into_inner());
                                let Some(stream) = guard.as_mut() else {
                                    return SockPoll::Ready(Ok(
                                        vm.sock_err("read on a closed socket")
                                    ));
                                };
                                // Settle a carry that already decides BEFORE touching the fd —
                                // same guard, same order as the fast path (an invalid carry is
                                // sticky, so waiting on the fd for it would poll to the deadline
                                // for nothing).
                                let decided = match carry.is_empty() {
                                    true => None,
                                    false => match Vm::decode_carry(&mut carry, &[], false) {
                                        Decoded::NeedMore => None,
                                        d => Some(d),
                                    },
                                };
                                match decided {
                                    Some(d) => Ok(d),
                                    None => match std::io::Read::read(stream, &mut b) {
                                        Ok(got) => {
                                            Ok(Vm::decode_carry(&mut carry, &b[..got], got == 0))
                                        }
                                        Err(e) => Err(e),
                                    },
                                }
                            };
                            match r {
                                // Same decode guard as the fast path; a NeedMore (no complete
                                // codepoint yet) just re-polls, like a would-block.
                                Ok(d) => match vm.decoded_value(d) {
                                    Some(v) => SockPoll::Ready(Ok(v)),
                                    None => {
                                        // N3(a) — took a partial off the fd: latch it so the demote
                                        // loop's timeout branch (sched.rs) reports `incomplete
                                        // utf-8` rather than `timeout`.
                                        vm.poll_partial = Some(
                                            core.carry
                                                .lock()
                                                .unwrap_or_else(|e| e.into_inner())
                                                .len(),
                                        );
                                        SockPoll::WouldBlock
                                    }
                                },
                                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                    SockPoll::WouldBlock
                                }
                                Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                            }
                        },
                    );
                }
                Err((e, _)) => return Ok(self.sock_err(format!("{e}"))),
            }
        }
    }

    /// D6/R1 — `Socket.write(s) -> Result[int]` / `write_bytes(b) -> Result[int]` (+ optional
    /// `timeout_ms`). One write path — only the byte extraction differs. On a would-block the fiber
    /// PARKS on writability (M:N) or DEMOTE-polls it in-callback; off the M:N engine it fails loud.
    ///
    /// N2 — `timeout_ms` is LATCHED on the fiber ([`Vm::poll_deadline`]) exactly like `read`: a park
    /// rewinds `ip` and re-executes the op, so an un-latched `now + timeout_ms` would re-arm on every
    /// re-park and never expire. Extracted from `socket_method` so the `drop_poll_latch` clear on
    /// completion has ONE seam catching every early return (closed socket, poll-once, would-block).
    ///
    /// W15-3 — WRITE-ALL, like Go's `Conn.Write`: `Ok(len)` only once every byte is sent. A full
    /// buffer parks (or demote-polls) and resumes at the offset latched in [`Vm::poll_written`];
    /// any early stop — closed socket, deadline, poll-once `write(s, 0)`, OS error — is `Err`, and
    /// the bytes already sent are not reported.
    fn socket_write(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // The optional trailing int bounds writability.
        self.arity_range_err(method, args, 1, 2, span)?;
        if self.poll_timeout_check(span)? {
            return Ok(self.sock_err("timeout"));
        }
        let timeout = self.parse_timeout_ms(args.get(1), span)?;
        let data = if method == "write_bytes"
            && let Some(v) = args.first()
        {
            self.collect_bytes_arg("write_bytes", *v, span)?
        } else if let Some(v) = args.first()
            && let Some(sh) = v.as_obj()
            && let Obj::Str(s) = self.heap.get(sh)
        {
            s.as_bytes().to_vec()
        } else {
            return Err(self.err("write expects a str".into(), span));
        };
        // N2 — the per-call deadline, latched on the fiber so it survives a park's ip-rewind re-run
        // (identical discipline to `socket_read`).
        let deadline = timeout
            .filter(|t| !t.poll_once)
            .map(|t| *self.poll_deadline.get_or_insert(t.deadline));
        // W15-3 — the bytes already sent, latched on the fiber so a park's ip-rewind re-run resumes
        // at that offset instead of resending from byte 0.
        let mut sent = self.poll_written.unwrap_or(0).min(data.len());
        let core = self.socket_core(h);
        let attempt = {
            let mut guard = core.stream.lock().unwrap();
            let Some(stream) = guard.as_mut() else {
                return Ok(self.sock_err("write on a closed socket"));
            };
            match write_from(stream, &data, &mut sent) {
                Ok(()) => Ok(()),
                Err(e) => Err((e, stream.as_raw_fd())),
            }
        };
        match attempt {
            Ok(()) => Ok(self.sock_ok(Value::int(data.len() as i64))),
            Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if timeout.is_some_and(|t| t.poll_once) {
                    return Ok(self.sock_err("timeout"));
                }
                self.poll_written = Some(sent);
                let target = PollPark {
                    key: core.key,
                    fd,
                    interest: poller::Interest::Write,
                    in_flight: Arc::clone(&core.in_flight),
                    closed: Arc::clone(&core.closed),
                    deadline,
                };
                if let Some(v) = self.park_on_fd(h, args, target, "write", span)? {
                    return Ok(v);
                }
                // In-callback on M:N (or top-level `main` on the default engine) → demote +
                // backoff-poll the non-blocking write in place (#3 socket half).
                let mut sent = sent;
                self.demote_block_socket(fd, poller::Interest::Write, deadline, span, move |vm| {
                    let r = {
                        let mut guard = core.stream.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(stream) = guard.as_mut() else {
                            return SockPoll::Ready(Ok(vm.sock_err("write on a closed socket")));
                        };
                        write_from(stream, &data, &mut sent)
                    };
                    match r {
                        Ok(()) => SockPoll::Ready(Ok(vm.sock_ok(Value::int(data.len() as i64)))),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            SockPoll::WouldBlock
                        }
                        Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                    }
                })
            }
            Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
        }
    }

    /// D6 — `Socket` methods: `read(n) -> Result[str]` (see [`Vm::socket_read`] — B1: it decodes through
    /// [`Vm::decode_carry`], never `from_utf8_lossy`), `write(s) -> Result[int]`, `close() -> nil`. On a
    /// would-block, under the M:N engine the fiber PARKS on the netpoller (rewind `ip`, set the
    /// `poll_park` sentinel); off it, there is no fiber to park (net targets `--parallel`).
    pub(super) fn socket_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "read" => {
                let r = self.socket_read(h, args, span);
                // B1 — the read's absolute deadline is LATCHED on the fiber (`poll_deadline`) so a park
                // + ip-rewind re-execution keeps the ORIGINAL `timeout_ms` budget instead of restarting
                // it. This logical `read` is over unless it parked (`poll_park` set ⇒ the very same call
                // resumes) — so drop the latch here; the next `read` gets a fresh budget.
                self.drop_poll_latch();
                r
            }
            "read_bytes" => {
                let r = self.socket_read_bytes(h, args, span);
                // Same latch discipline as `read`: drop the deadline unless the op PARKED (the very
                // same call resumes) — a re-park must not re-arm the timeout budget.
                self.drop_poll_latch();
                r
            }
            // `write(s[, timeout_ms])` (str) and R1's `write_bytes(b[, timeout_ms])` (raw `bytes`)
            // are the SAME write path — only the byte-extraction differs.
            "write" | "write_bytes" => {
                let r = self.socket_write(h, method, args, span);
                // N2 — `write` now latches its deadline on the fiber like `read` (a re-park must not
                // re-arm the budget), so it MUST drop the latch on completion too.
                self.drop_poll_latch();
                r
            }
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.socket_core(h);
                // W15-1 — take the stream out first so a fiber `register`ing concurrently (already past
                // its `WouldBlock` but not yet under the registry lock) sees `closed` before it can arm
                // the fd; only then deregister a pending park, and drop the stream (closing the fd) LAST
                // so `deregister`'s epoll `delete` never races the close. Must not hold `stream`'s guard
                // across `deregister`: it takes the sched lock via `complete_offload`.
                let stream = core.stream.lock().unwrap().take();
                core.closed.store(true, Ordering::Release);
                poller::deregister(core.key);
                drop(stream);
                Ok(Value::nil())
            }
            _ => Err(self.err(format!("type Socket has no method '{method}'"), span)),
        }
    }

    /// D6/D6c — `Listener.accept() -> Result[Socket]` (+ optional `timeout_ms`). Parks on the listen
    /// fd's readability (a pending connection) under the M:N engine, like `Socket::read`; demotes
    /// in-callback; fails loud off the M:N engine.
    ///
    /// N2 — `timeout_ms` is LATCHED on the fiber ([`Vm::poll_deadline`]) like `read`/`write`: a park
    /// re-executes the op, so an un-latched deadline would re-arm on every re-park. Extracted from
    /// `listener_method` so the `drop_poll_latch` clear has ONE seam over every early return.
    fn listener_accept(
        &mut self,
        h: GcRef,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        // `accept()` or `accept(timeout_ms)` — the optional trailing int bounds how long to wait for
        // an inbound connection (D6c). Mirrors `Socket::read`'s timeout handling.
        self.arity_range_err("accept", args, 0, 1, span)?;
        if self.poll_timeout_check(span)? {
            return Ok(self.sock_err("timeout"));
        }
        let timeout = self.parse_timeout_ms(args.first(), span)?;
        // N2 — the per-call deadline, latched on the fiber so it survives a park's ip-rewind re-run.
        let deadline = timeout
            .filter(|t| !t.poll_once)
            .map(|t| *self.poll_deadline.get_or_insert(t.deadline));
        let core = self.listener_core(h);
        let attempt = {
            let guard = core.listener.lock().unwrap();
            let Some(listener) = guard.as_ref() else {
                return Ok(self.sock_err("accept on a closed listener"));
            };
            match listener.accept() {
                Ok((stream, _peer)) => Ok(stream),
                Err(e) => Err((e, listener.as_raw_fd())),
            }
        };
        match attempt {
            Ok(stream) => Ok(self.accept_socket_value(stream)),
            Err((e, fd)) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if timeout.is_some_and(|t| t.poll_once) {
                    return Ok(self.sock_err("timeout"));
                }
                let target = PollPark {
                    key: core.key,
                    fd,
                    interest: poller::Interest::Read,
                    in_flight: Arc::clone(&core.in_flight),
                    closed: Arc::clone(&core.closed),
                    deadline,
                };
                if let Some(v) = self.park_on_fd(h, args, target, "accept", span)? {
                    return Ok(v);
                }
                // In-callback on M:N (or top-level `main` on the default engine) → demote +
                // backoff-poll the non-blocking accept in place (#3 socket half).
                self.demote_block_socket(fd, poller::Interest::Read, deadline, span, move |vm| {
                    let r = {
                        let guard = core.listener.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(listener) = guard.as_ref() else {
                            return SockPoll::Ready(Ok(vm.sock_err("accept on a closed listener")));
                        };
                        listener.accept()
                    };
                    match r {
                        Ok((stream, _peer)) => SockPoll::Ready(Ok(vm.accept_socket_value(stream))),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            SockPoll::WouldBlock
                        }
                        Err(e) => SockPoll::Ready(Ok(vm.sock_err(format!("{e}")))),
                    }
                })
            }
            Err((e, _)) => Ok(self.sock_err(format!("{e}"))),
        }
    }

    /// D6 — `Listener` methods: `accept() -> Result[Socket]`, `close() -> nil`. `accept` parks on the
    /// listening fd's readability (a pending connection) under the M:N engine, like `Socket::read`.
    pub(super) fn listener_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "accept" => {
                let r = self.listener_accept(h, args, span);
                // N2 — `accept` latches its deadline on the fiber like `read`, so it MUST drop the
                // latch on completion too (a re-park must not re-arm the budget).
                self.drop_poll_latch();
                r
            }
            "addr" => {
                self.arity_err("addr", args, 0, span)?;
                let core = self.listener_core(h);
                let addr = {
                    let guard = core.listener.lock().unwrap();
                    match guard.as_ref() {
                        Some(l) => l
                            .local_addr()
                            .map(|a| a.to_string())
                            .map_err(|e| e.to_string()),
                        None => Err("addr on a closed listener".to_string()),
                    }
                };
                match addr {
                    Ok(a) => {
                        let v = self.alloc_str(a);
                        Ok(self.sock_ok(v))
                    }
                    Err(e) => Ok(self.sock_err(e)),
                }
            }
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.listener_core(h);
                // W15-1 — same order as `Socket.close`: see its comment.
                let listener = core.listener.lock().unwrap().take();
                core.closed.store(true, Ordering::Release);
                poller::deregister(core.key);
                drop(listener);
                Ok(Value::nil())
            }
            _ => Err(self.err(format!("type Listener has no method '{method}'"), span)),
        }
    }

    /// D6 — wrap an accepted `TcpStream` (set non-blocking) into a fresh `Socket` handle, as a
    /// `Result::Ok`.
    pub(super) fn accept_socket_value(&mut self, stream: std::net::TcpStream) -> Value {
        stream.set_nonblocking(true).ok();
        let core = Arc::new(SocketCore {
            stream: Mutex::new(Some(stream)),
            key: core::next_poll_key(),
            in_flight: core::new_in_flight(),
            closed: core::new_closed(),
            carry: Mutex::new(Vec::new()),
        });
        let v = Value::obj(self.heap.alloc(Obj::Socket(core)));
        self.sock_ok(v)
    }

    /// W7-18 — the resume half of the socket-park deadline story, shared by every op that can be
    /// re-injected by the netpoller (`read`, `read_bytes`, `write`, `accept`). Returns `true` when THIS
    /// op's own D6c `timeout_ms` is what fired, i.e. when the caller should surface its catchable
    /// `Err("timeout")` exactly as before.
    ///
    /// The two wake causes are told apart by the CLOCK, not by a second marker: `park_on_fd` clamped
    /// the park to `min(op deadline, run deadline)`, both are absolute `Instant`s, and `self.deadline`
    /// is `Some` only under `chezzi test --timeout` — so `now >= self.deadline` at resume is true iff
    /// the RUN deadline is what expired. A run-deadline expiry must be a hard, `recover:`-proof halt;
    /// an op-timeout expiry must stay an ordinary catchable value.
    ///
    /// Two orderings here are load-bearing and both were got wrong in the obvious version:
    ///
    /// 1. **Consume `poll_timed_out` FIRST, unconditionally.** Halting with `?` while the flag is still
    ///    set leaves it for the unwind: the first socket op inside any `defer` then consumes it and
    ///    reports a fabricated `Err("timeout")` for an op that was never given a `timeout_ms`, so the
    ///    cleanup silently does nothing. That is the "stopped promptly ≠ cleaned up" failure W7-16 was
    ///    caught on, and no existing fence sees it (their `defer`s only `print`).
    /// 2. **Inside a `defer`, a run-deadline wake is NOT this op's timeout** — hence `Ok(false)`, which
    ///    retries the syscall so a cleanup write that can complete immediately still does (W7-17's
    ///    `deferring > 0` suppression, same term `halt_requested` uses). If the retry would block,
    ///    `park_on_fd`'s ungated check halts it there — which is what makes that check load-bearing.
    ///
    /// Accepted degeneracy: with an op deadline SHORTER than the run deadline by less than the
    /// poller-inject-to-schedule latency, the op's honest timeout fires but the fiber is not scheduled
    /// until the run deadline has also passed, converting a catchable `Err("timeout")` into a hard
    /// halt. That window is scheduling latency — normally sub-millisecond, but it widens with worker
    /// contention (`--threads=1` plus a CPU-bound sibling makes it reachable), so it is bounded by
    /// load rather than by a constant. Accepted, not fenced, because it is correct by consequence: a
    /// fiber that resumes past the run deadline hard-halts at its very next checkpoint regardless, so
    /// the only thing lost is which of two aborts is reported, and a test that pinned it would pass
    /// either way.
    fn poll_timeout_check(&mut self, span: Span) -> Result<bool, RuntimeError> {
        let fired = std::mem::take(&mut self.poll_timed_out);
        match self.deadline_halt(span) {
            Ok(()) => Ok(fired),
            Err(e) if self.deferring == 0 => Err(e),
            Err(_) => Ok(false),
        }
    }

    /// D6 — the would-wait decision shared by every would-block socket op (`op` names it). Returns
    /// `Ok(Some(v))` when the op settles here: `v` is the parked sentinel (the fiber was parked on
    /// the netpoller) or, in a `Refuse` cell of the `Socket` row of `block::mode`, the op's
    /// `would block` `Err` value. Returns `Ok(None)` when the caller must block in place: on the two
    /// contexts that own their whole thread (an M:N in-callback demote, and top-level `main` on the
    /// default engine — Go-identical, and what makes the hello-world TCP server writable) it falls
    /// through to [`Vm::demote_block_socket`] and BLOCKS there, bounded only by the op's `timeout_ms`,
    /// the run's `--timeout`, or a cancel — and on top-level `main` under `chezzi run` **only the first
    /// of those three exists**: `--timeout` is a `chezzi test` flag (`chezzi run` rejects it as an
    /// unknown flag) and `main` has no scope cancel to trip, so an untimed op there blocks until SIGINT
    /// (see [`Vm::demote_block_socket`]'s doc); everywhere else — an eager `Executor` job, a
    /// callback on a non-M:N thread — it keeps the loud `Err("<op> would block: an Executor job
    /// doesn't own its thread …")`, because blocking a SHARED thread starves the very peer that
    /// would make the fd ready (both shapes measured as hangs; see that helper's doc).
    /// `Err` only for a **concurrent op on a shared socket**: oneshot epoll allows ONE registration per
    /// fd, so a second fiber reaching a would-block op while the first is parked (`in_flight` already
    /// set) faults cleanly rather than corrupting the poller registry (review: Critical). On the park
    /// path it restores the pre-call operand stack (receiver THEN args — the exact layout `CallMethod`
    /// re-pops; unlike a 0-arg `recv` park, `read(n)`/`write(s)` must re-push their args), rewinds `ip`
    /// so the op re-executes on resume, and sets the `poll_park` sentinel for the worker loop.
    ///
    /// D6c — `target.deadline` (the optional `timeout_ms`) is honored on this snapshot-park path (the
    /// netpoller wakes the fiber on readiness OR at the deadline) AND, since the deadline was threaded
    /// into [`Vm::demote_block_socket`], on the in-callback demote path too (`native_reentry > 0`, where
    /// this returns `Ok(false)` — the demote loop caps its kernel wait by the remaining budget and
    /// expires with the same timeout `Err`). Every socket op latches its deadline on the fiber the same
    /// way (`Vm::poll_deadline`, N2), so a re-park does not re-arm the budget.
    ///
    /// W7-18 — the park also observes the RUN's `--timeout` deadline, in two halves that are one fix:
    /// the halt below (a fiber must not park PAST a deadline that has already passed) and the clamp
    /// further down (a park already under way wakes at the sooner of the two deadlines). There is
    /// deliberately no [`deadline_gap_wake`] analogue here, and that is worth stating because W7-17
    /// needed one three functions away: there `timer::submit_at` armed a *job* before `MnSched::park`
    /// had filled the fiber's bucket, so an early fire found an empty bucket and was LOST. Here the
    /// deadline is not a job but a FIELD IN THE REGISTRY ROW, and [`poller::register`] inserts the row
    /// and the fiber together under the registry lock — the wake is re-derived by re-reading the
    /// registry (`next_timeout` / `fire_due_socket_timeouts`), so no fire can precede the park. An
    /// already-expired row just makes `next_timeout` return `ZERO`, and `register`'s `notify()` covers
    /// the insert-after-`next_timeout`-was-read window.
    pub(super) fn park_on_fd(
        &mut self,
        h: GcRef,
        args: &[Value],
        target: PollPark,
        op: &str,
        span: Span,
    ) -> Result<Option<Value>, RuntimeError> {
        // W7-18 — `--timeout` ABOVE the cancellation checkpoint, mirroring W7-17's ordering in
        // `chan_recv_step`: the deadline outranks a cancel, so a fiber reaching here after the run
        // deadline reports the honest hard halt rather than `cancelled`. UNGATED by `deferring`
        // (unlike `poll_timeout_check`'s entry check): everything above a park settles without
        // blocking, and a `defer` that would PARK past the deadline is a hang, not cleanup.
        self.deadline_halt(span)?;
        // TICKET-194 — a refused op never waits, so it is not a cancellation point (owner decision
        // 1): `Refuse` is decided BEFORE `wait_halt`, or a pending cancel cuts an op that returns its
        // `would block` `Err` at once.
        let mode = self.block_mode(WaitSpec::Socket);
        if mode == BlockMode::Refuse {
            return Ok(Some(self.sock_err(sock_would_block_msg(op))));
        }
        // CANCELLATION CHECKPOINT — a socket op that would wait is a cancel-delivery point
        // (the single choke point for `accept`/`read`/`write`): the check sits OUTSIDE the
        // `mn.is_some()` gate, because top-level `main` (and any other non-worker-shell context) runs
        // the op as a BLOCKING syscall below and would otherwise have no cancel-delivery point at a
        // socket at all. On M:N a cancelled fiber must also not RE-park: `poller::drain_family`
        // re-injects a poller-parked fiber on cancel and the rewound op re-runs here — without this
        // check it would would-block and re-park forever (the every-instruction check that used to
        // kill it at the dispatch loop top is gone; see `run_until`), wedging the nursery.
        self.wait_halt(span)?;
        if mode == BlockMode::Park {
            // The `in_flight` guard: at most one op may be parked on a socket at a time. A second
            // concurrent op on a shared socket (`Arc`) faults rather than overwrite the registry entry
            // (which would drop the first fiber + leak `inflight`) or double-`add` the fd (EEXIST panic).
            if target.in_flight.swap(true, Ordering::AcqRel) {
                return Err(self.err(
                    "concurrent operation on a shared socket is not supported".into(),
                    span,
                ));
            }
            self.push(Value::obj(h)); // receiver (deeper on the stack)
            for &a in args {
                self.push(a); // its args, in order, back on top
            }
            self.frames.last_mut().unwrap().ip -= 1;
            // W7-18 — wake at the SOONER of the op's own D6c budget and the run's `--timeout`
            // deadline, so a park with no `timeout_ms` at all (`deadline: None` — the shape that
            // HUNG: a nursery `l.accept()` nobody connects to) is still reached by the wall clock.
            // `poll_timeout_check` then tells the two causes apart at resume by re-reading the clock,
            // which is why no second marker beside `poll_timed_out` is needed.
            //
            // Clamp `target.deadline` ONLY — never `self.poll_deadline` (the per-op budget latch, N2,
            // which survives an ip-rewind re-park): `demote_block_socket` reads that latch as the op's
            // own budget and expiring it yields a CATCHABLE `Err("timeout")`, so folding the run
            // deadline into it would report a hard `--timeout` abort as an ordinary socket timeout.
            let target = PollPark {
                deadline: match (target.deadline, self.deadline) {
                    (Some(op), Some(run)) => Some(op.min(run)),
                    (op, run) => op.or(run),
                },
                ..target
            };
            self.poll_park = Some(target);
            Ok(Some(Value::nil())) // parked sentinel; `poll_park` gates the result-push at `do_call`
        } else {
            Ok(None)
        }
    }

    /// `Channel[T]` methods (C2/C4): `send` (move-on-send, deep-copied in), `recv` (FIFO; empty =
    /// deadlock fault under the sequential executor), `len`.
    pub(super) fn channel_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "send" => {
                self.arity_err("send", args, 1, span)?;
                // B3.1: serialize once into the core (the wire form IS the airlock copy). A
                // non-sendable value (a frame-holding generator, or a module/native/FFI handle) faults
                // gracefully with `send`'s span — the value crosses a heap boundary into the receiver.
                let w = self.to_wire_crossable(args[0], span)?;
                // TICKET-185 — `closed` is judged ONCE, inside `chan_send_step`'s commit (a fresh
                // send) or by the offer's own `Pending` (a parked one). Unbounded: enqueue
                // immediately. Bounded or rendezvous with no taker: publish an offer and block —
                // park the fiber (`SendStep::Parked` — the receiver+value were re-rooted), block in
                // place, demote, or, in a context that cannot block, fault. On park,
                // `do_method_call` skips the result push.
                match self.chan_send_step(h, w, args[0], span)? {
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
            "try_send" => {
                self.arity_err("try_send", args, 1, span)?;
                let w = self.to_wire_crossable(args[0], span)?;
                let core = self.channel_core(h);
                let sent = matches!(self.send_commit(h, &core, w, None), SendOutcome::Sent);
                Ok(Value::bool(sent))
            }
            "recv" => {
                self.arity_err("recv", args, 0, span)?;
                // D5 owe #3 (Path C) — a `recv` reached INSIDE a native callback on the M:N engine
                // can't snapshot-park, so `recv_step_or_demote` DEMOTES the worker thread: block in
                // place on the channel condvar + spin a replacement, resuming on a sibling `send`
                // (Go's `handoffp`). `demote_recv_block` is itself closed-aware (a `close` faults the
                // demoted recv).
                match self.recv_step_or_demote(h, span)? {
                    RecvStep::Got(w) => {
                        self.wake_senders(h); // freed a slot — wake a parked bounded sender
                        Ok(self.from_wire(w))
                    }
                    // A `give` filled this receiver's own slot: it committed no sender.
                    RecvStep::Filled(w) => Ok(self.from_wire(w)),
                    // `chan_recv_step` already re-rooted the receiver + set `suspend`; the sentinel is
                    // never observed (`do_method_call` gates the result-push on `suspend`).
                    RecvStep::Parked => Ok(Value::nil()),
                    // Closed-and-drained: a distinct fault (not the deadlock fault) — no producer left.
                    RecvStep::ClosedEmpty => Err(self.err(CLOSED_RECV.to_string(), span)),
                }
            }
            "try_recv" => {
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
                let ready = core.recv_ready(&mut core.q.lock().unwrap());
                let got = match ready {
                    core::RecvReady::Value(w) => {
                        self.wake_senders(h); // a real pop freed a slot — wake a parked bounded sender
                        Some(self.from_wire(w))
                    }
                    core::RecvReady::Fired => Some(Value::bool(true)),
                    core::RecvReady::Closed | core::RecvReady::Wait => None,
                };
                Ok(match got {
                    Some(v) => self.alloc_enum("Option", "Some", vec![v]),
                    None => self.alloc_enum("Option", "None", vec![]),
                })
            }
            // `close()` marks the channel closed (idempotent) and wakes every parked / demoted
            // receiver so each re-runs and observes the close: a `for v in ch:` ends, a bare `recv`
            // faults. Mirrors `send`'s wake fan-out but delivers no value.
            "close" => {
                self.arity_err("close", args, 0, span)?;
                let core = self.channel_core(h);
                // TICKET-185 — close every parked sender's offer in the same hold that sets
                // `closed` (Go: a parked sender's value is NOT delivered once the channel is closed;
                // the woken sender settles `Closed` and faults `send on a closed channel`). A value
                // already committed to a receiver's slot stays delivered.
                core.q.lock().unwrap().close();
                // Same routing as `send_commit`: an inline outermost-`parallel:` builder VM
                // (`self.mn == None`) closing a channel must wake enlisted, parked receivers via the
                // held `mn_enlist_sched`, not just the local condvar. (Cross-nursery flat scheduler #2.)
                if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
                    let key = self.channel_core_ptr(h);
                    sched.close_wake(key, &core);
                } else {
                    // Wake any demoted OS thread blocked on this core's condvar (in-callback recv).
                    core.cv.notify_all();
                    // Cooperative engine: re-add every sibling fiber parked on this channel's `recv`.
                    self.wake_on_send(h);
                }
                Ok(Value::nil())
            }
            // `trip()` flips the manual level-trigger latch (the primitive behind `std.cancel`'s
            // `done()`): the channel is then permanently ready (`recv`/`try_recv`/`wait` yield `true`).
            // Idempotent. Reuses `close()`'s exact wake fan-out so a parked `recv`/`wait` re-runs and
            // observes the latch — but does NOT set `closed` (a closed+empty `wait` arm is *skipped*;
            // we need it *ready*).
            "trip" => {
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
                if let Some(sched) = self.mn.clone().or_else(|| self.mn_enlist_sched.clone()) {
                    let key = self.channel_core_ptr(h);
                    sched.close_wake(key, &core);
                } else {
                    core.cv.notify_all();
                    self.wake_on_send(h);
                }
                Ok(Value::nil())
            }
            "len" => {
                self.arity_err("len", args, 0, span)?;
                // The buffer only (TICKET-185): a parked sender's offer is not buffered — Go
                // measures `len: 0` on an unbuffered channel with a parked sender.
                let n = self.channel_core(h).q.lock().unwrap().len();
                Ok(Value::int(n as i64))
            }
            // `cap()` reports the channel's capacity: `-1` for unbounded `Channel[T]()`, `0` for
            // rendezvous `Channel[T](0)`, `n` for bounded `Channel[T](n)`.
            // A capacity above 2^62 boxes as `Obj::BigInt` (`Value::int` would wrap).
            "cap" => {
                self.arity_err("cap", args, 0, span)?;
                let cap = self.channel_core(h).cap.map_or(-1i64, |c| c as i64);
                Ok(self.make_int(cap))
            }
            _ => Err(self.err(format!("type Channel has no method '{method}'"), span)),
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
        w: WireValue,
        orig: Value,
        span: Span,
    ) -> Result<SendStep, RuntimeError> {
        let core = self.channel_core(h);
        // A re-run after a park: settle the offer this `send` published before it parked, BEFORE
        // the cancel checkpoint — a taken offer means the `send` happened.
        if let Some(op) = self.pending.take() {
            if op.p.is_queued() && !(self.native_reentry == 0 && self.halt_requested().is_some()) {
                // A stray wake: still queued — re-park on the same offer.
                self.pending = Some(op);
                self.park_send(h, orig);
                return Ok(SendStep::Parked);
            }
            if let Some(r) = self.send_settled(op, span) {
                return r;
            }
        }
        if core.cap.is_none() {
            // Unbounded — never blocks.
            return match self.send_commit(h, &core, w, None) {
                SendOutcome::Closed => Err(self.err(CLOSED_SEND.to_string(), span)),
                _ => Ok(SendStep::Sent),
            };
        }
        // The table decides how this send blocks; a `Refuse` context publishes no offer.
        let mode = self.block_mode(WaitSpec::Send);
        let p = (mode != BlockMode::Refuse).then(Pending::new);
        match self.send_commit(h, &core, w, p.as_ref().map(|p| (p, 0))) {
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
        core.cv.notify_all();
        if mode == BlockMode::Park {
            // A real M:N WORKER snapshot-parks: the worker loop drives `send_suspend` →
            // `Disp::SendPark`.
            self.pending = Some(op);
            self.park_send(h, orig);
            return Ok(SendStep::Parked);
        }
        if mode == BlockMode::Demote {
            return self.demote_send_block(core, op, span);
        }
        self.block_send(&core, op, span)
    }

    /// A party that owns its OS thread — an eager `Executor` job, or the top-level `main` thread —
    /// blocks until its offer settles, the mirror of the empty-`recv` case in `chan_recv_step`
    /// ([`Vm::block_wait_tick`]). The party registration is scoped to the WAIT only — see the same
    /// rule spelled out in [`Vm::block_recv`].
    fn block_send(
        &mut self,
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
            let party = self.block_party_guard(quiesce::PartyWait::Send(Arc::clone(&p)));
            r = self.block_wait_tick(core, msg, span, |_| !p.is_queued());
            drop(party);
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
        w: WireValue,
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
            return sched.send_commit(key, core, w, offer, wid);
        }
        let sum = crate::vm::core::wire_summary(&w); // OFF-LOCK — see `ChanState::push`
        let out = core
            .q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .send(core.cap, sum, w, offer);
        if matches!(out, SendOutcome::Sent) {
            core.cv.notify_all();
            self.wake_on_send(h); // wake a receiver parked on this channel's `recv`
        }
        out
    }

    /// Park the running fiber on a full bounded `send`: re-root the receiver AND the value argument on
    /// the operand stack (send is 1-arg, unlike recv's 0-arg — both must be re-pushed or the rewound
    /// `CallMethod(send)` mis-reads the stack), rewind `ip` so the send re-executes on resume, and set
    /// the `send_suspend` sentinel. The scheduler / worker loop files the fiber into the channel's wait
    /// set; a sibling `recv` freeing a slot ([`Vm::wake_senders`]) wakes it. The value is re-serialized
    /// on the re-run (`to_wire_at(orig)` is idempotent), so the wire form built before parking is dropped.
    pub(super) fn park_send(&mut self, h: GcRef, value: Value) {
        self.push(Value::obj(h)); // receiver (deeper on the stack)
        self.push(value); // its one arg, back on top
        self.frames.last_mut().unwrap().ip -= 1;
        self.send_suspend = Some(h);
    }

    /// Bounded-channel backpressure — after a `recv` frees a slot on a BOUNDED channel, wake any fiber
    /// parked on a full `send` to it. No-op for an unbounded channel (no sender ever parks there — the
    /// common `recv` path pays only a `cap.is_none()` check). Routes exactly like `channel_send_wire`'s
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
            core.cv.notify_all();
            self.wake_on_send_key_kind(Arc::as_ptr(core) as usize, WakeKind::Settled);
        }
    }

    /// The ONE entry for a `recv` that may run inside a native re-entry (TICKET-136, W14-16): the
    /// `recv` method and `for v in ch:`. A native re-entry — a callback, a `defer`, a generator
    /// resume — has a host-stack loop frame that cannot be snapshot-parked, so an M:N fiber DEMOTES
    /// (block in place, spin a replacement worker) instead of faulting. A `timer(ms)` channel is
    /// excluded: it has no sibling sender, and [`Vm::chan_recv_step`] synthesises its value at any
    /// re-entry. A new site that calls `chan_recv_step` directly brings W14-16 back.
    pub(super) fn recv_step_or_demote(
        &mut self,
        h: GcRef,
        span: Span,
    ) -> Result<RecvStep, RuntimeError> {
        let spec = if self.channel_core(h).timer.is_some() {
            WaitSpec::Timer
        } else {
            WaitSpec::Recv
        };
        // A timer's Demote cell is served inside `chan_recv_step`: its value is synthesised at the
        // deadline, never queued, so `demote_recv_block`'s pop loop cannot serve it.
        if spec == WaitSpec::Recv && self.block_mode(spec) == BlockMode::Demote {
            return self.demote_recv_block(h, span);
        }
        self.chan_recv_step(h, span)
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
            && let Settled::Got(_, w) = op.settle()
        {
            return Ok(RecvStep::Filled(w));
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
        let ready = core.recv_ready(&mut core.q.lock().unwrap());
        match ready {
            core::RecvReady::Value(w) => return Ok(RecvStep::Got(w)),
            core::RecvReady::Fired => return Ok(RecvStep::Got(WireValue::Bool(true))),
            core::RecvReady::Closed => return Ok(RecvStep::ClosedEmpty),
            core::RecvReady::Wait => {}
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
                                sched_job.send_wake(key, &core_job, WireValue::Bool(true));
                            } else {
                                deadline_gap_wake(&sched_job, key, &core_job, &gap_cancel);
                            }
                            sched_job.inflight.fetch_sub(1, Ordering::Relaxed);
                        }),
                    );
                    self.park_recv(h);
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
                    return Ok(RecvStep::Got(WireValue::Bool(true)));
                }
                // Every other context (`BlockMode::InlineSleep`: the top-level VM, the inline
                // outermost-`parallel:` builder VM, an eager `Executor` job's `Vm`): inline-sleep to
                // the deadline, synthesise.
                //
                // W7-16 — the wait is CHUNKED, not one `thread::sleep`: this deadline is ours, so it
                // stays a cancellation + `--timeout` checkpoint for its whole duration. Pre-fix an
                // eager `Executor` job here ran the full 3 s through a `shutdown_now()` at 50 ms (and
                // through `--timeout`), while the same `timer(ms).recv()` in a nursery — which parks,
                // above — was cancelled at 55 ms. Same primitive, two answers.
                self.block_until_deadline(deadline, span)?;
                return Ok(RecvStep::Got(WireValue::Bool(true)));
            }
        }
        // M:N snapshot-park path (empty-open parks the fiber; the worker loop files it into the wait
        // set). A fiber woken only to be cancelled must not re-park — `wait_halt` above already
        // returned in that case. `MnSched::park` re-checks the queue, close, latch and cancel under
        // the sched lock, so no pre-park pop is needed here; an in-place waiter re-reads
        // `recv_ready` at `block_recv`'s loop head.
        let recv_mode = self.block_mode(WaitSpec::Recv);
        if recv_mode == BlockMode::Park {
            self.park_recv(h);
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
            return self.block_recv(&core, span);
        }
        // A native callback with no thread of its own to block on: the host stack cannot be unwound
        // to park either. Fault, as before.
        Err(self.err(EMPTY_RECV_DEADLOCK.to_string(), span))
    }

    /// Park the running fiber on an empty `recv`: re-root the receiver on the operand stack, rewind
    /// `ip` so the current op (`CallMethod(recv)` or `ChanRecvOrClosed`) re-executes on resume, and
    /// set the `suspend` sentinel. The scheduler / worker loop files the fiber into the channel's
    /// wait set; a sibling `send`/`close` wakes it.
    pub(super) fn park_recv(&mut self, h: GcRef) {
        self.push(Value::obj(h));
        self.frames.last_mut().unwrap().ip -= 1;
        self.suspend = Some(h);
    }

    /// One tick of a blocking wait: honour the halts, then wait up to [`DEMOTE_POLL_BACKOFF`]. Shared
    /// by [`Vm::block_recv`] and the blocking full-`send`.
    ///
    /// A job dispatched by an eager `submit` has no nursery scheduler and no [`MnSched`], and neither
    /// does the top-level `main` thread, so their blocking ops used to fall to the "no scheduler" arms
    /// and declare a deadlock on the spot. That verdict was TRUE while jobs only ran at the drain — the
    /// submitter was blocked inside `shutdown()`, so no runnable task could send — and became a LIE the
    /// moment jobs start at `submit`, because the submitter is still running and may send on the very
    /// next statement. Both kinds of party BLOCK here instead, which is also what Python's
    /// `ThreadPoolExecutor` does, and the *process-wide* verdict in [`crate::vm::quiesce`] decides when
    /// there is really nobody left to feed them.
    ///
    /// The wait is a BOUNDED poll, not an untimed `cv.wait`, for the same reason
    /// [`Vm::demote_recv_block`]'s is: a lost wakeup then costs latency instead of the whole run, and
    /// the two halts that must stay un-swallowable get re-checked every tick. `--timeout` is checked
    /// here explicitly because a blocked job never reaches `jump_checked`'s loop back-edge, which is
    /// where every other path observes the deadline — without this, `chezzi test --timeout` could not
    /// kill a job blocked forever on a channel, which is exactly the hang eager execution makes easier
    /// to write.
    ///
    /// **W7-13 — `ready` is re-checked under the SAME lock hold that the wait consumes, and that is
    /// the whole point of the parameter.** The caller has already tried its operation and failed, but
    /// it dropped `core.q` to do so and then ran [`Vm::eager_halt_check`] (which takes `exec_registry`
    /// and per-core `eager` locks — a wide window). A `notify_all` from a consumer landing anywhere in
    /// that gap reaches a condvar NOBODY IS ON YET and is simply lost, so the caller then slept the
    /// full [`DEMOTE_POLL_BACKOFF`] with its value already waiting. Measured on the 50-handoff cap-1
    /// pipeline: 7 of 15 runs paid a whole extra 5 ms tick, in exact 5 ms quanta.
    ///
    /// `Condvar::wait_timeout_while` closes it — it evaluates the predicate under the guard BEFORE
    /// sleeping, so a wakeup that arrived while the lock was free is observed instead of missed (and
    /// it re-checks on spurious wakeups for free). It also re-checks the predicate AFTER each inner
    /// wait, so `timed_out()` implies "still not ready" — which is what
    /// [`BLOCK_WAITS_SLEPT_WHILE_READY`] (test builds only) turns into a load-independent regression
    /// detector: revert this call to a bare `wait_timeout` and that counter goes nonzero.
    ///
    /// The wake it is waiting for was never missing:
    /// [`Vm::wake_senders`] already fires on all six pop paths, and for an eager job it lands on
    /// `core.cv`. `block_halt_check` MUST stay before the lock — the no-lock-cycle argument on
    /// the process-wide verdict depends on the registry never being taken under `ChannelCore::q`.
    ///
    /// This only makes the CHANNEL conditions instant. The halts `block_halt_check` acts on — the
    /// `--timeout` deadline, a cancel, the deadlock verdict — are not in any predicate and are still
    /// observed once per tick, so cancellation is now the SLOWEST thing in this loop rather than the
    /// fastest. That bound is unchanged by this fix, not introduced by it.
    ///
    /// TICKET-141 — releases this thread's width permit for the tick and re-takes it after.
    fn block_wait_tick(
        &mut self,
        core: &Arc<ChannelCore>,
        deadlock_msg: &str,
        span: Span,
        ready: impl FnMut(&mut crate::vm::core::ChanState) -> bool,
    ) -> Result<(), RuntimeError> {
        self.width_release();
        let r = self.block_wait_tick_in_place(core, deadlock_msg, span, ready);
        self.width_acquire();
        r
    }

    fn block_wait_tick_in_place(
        &mut self,
        core: &Arc<ChannelCore>,
        deadlock_msg: &str,
        span: Span,
        mut ready: impl FnMut(&mut crate::vm::core::ChanState) -> bool,
    ) -> Result<(), RuntimeError> {
        self.block_halt_check(deadlock_msg, span)?;
        let q = core.q.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg_attr(not(test), allow(unused_mut))]
        let (mut guard, waited) = core
            .cv
            .wait_timeout_while(q, DEMOTE_POLL_BACKOFF, |g| !ready(g))
            .unwrap_or_else(|e| e.into_inner());
        #[cfg(test)]
        {
            BLOCK_WAITS.fetch_add(1, Ordering::Relaxed);
            if waited.timed_out() && ready(&mut guard) {
                BLOCK_WAITS_SLEPT_WHILE_READY.fetch_add(1, Ordering::Relaxed);
            }
        }
        drop((guard, waited));
        Ok(())
    }

    /// TICKET-134 — test-only: hold the window between the owner-fault rung and the verdict open
    /// until the owner's nursery has recorded a fault, so the check-then-check race
    /// [`Vm::block_halt_check`] closes is deterministically reachable. No-op unless armed via
    /// [`OWNER_FAULT_WINDOW_ENV`] in this run's own `HostConfig.env`.
    #[cfg(test)]
    fn owner_fault_window_hook(&self) {
        if self.eager_scheds.is_empty() {
            return;
        }
        let armed = self
            .host
            .env
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(OWNER_FAULT_WINDOW_ENV)
            .is_some();
        if !armed {
            return;
        }
        let t0 = std::time::Instant::now();
        while self.halt_requested().is_none() && t0.elapsed() < std::time::Duration::from_secs(10) {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Register this thread as a blocked party for as long as the returned guard lives, so the
    /// process-wide verdict can see it parked. The party half is `None` when this thread is not a
    /// counted party.
    ///
    /// §2c1 — it ALSO marks every eager nursery this thread owns as `body_blocked` for the same span.
    /// A body that is parked here cannot reach another `spawn`, so its `body_open` flag must stop
    /// vetoing the deadlock predicate (see [`super::JoinScope::body_blocked`]). This is the one funnel
    /// every counted-party block goes through, which is why the mark lives here rather than at each
    /// blocking site. Unmarked on drop, in `BlockGuard`'s `Drop`.
    pub(super) fn block_party_guard(&self, wait: quiesce::PartyWait) -> BlockGuard {
        // The wait is published WITH the `body_blocked` mark, in one `SchedCore` acquisition per
        // sched (`set_body_wait`) — see its doc for the race that two acquisitions leave open. The
        // party (P) is taken after, with no `SchedCore` held, so the documented P → A order holds.
        // ONE `Arc<PartyWait>` is shared by the sched registration and the party, so the two can
        // never disagree about what this thread is waiting for.
        let wait = Arc::new(wait);
        let mut g = self.blocked_bodies_guard_with(false, Some(Arc::clone(&wait)));
        g._party = self
            .block_ctx()
            .judged()
            .then(|| self.quiesce.block_shared(wait, self.wake_set()));
        g
    }

    /// §2c1 — the `body_blocked` half of [`Vm::block_party_guard`] on its own: mark every eager
    /// nursery scope open on THIS thread as unable to inject, for as long as the guard lives.
    ///
    /// Used directly wherever the body stops running without registering a party — a NESTED eager
    /// nursery's join, where the enclosing body sits in `mn_worker_loop` rather than in a channel
    /// wait. Without it the enclosing scope's `body_open` vetoes the deadlock predicate for the whole
    /// duration of that join, and a genuine nested deadlock hangs
    /// (`parallel_cross_nursery_genuine_nested_deadlock_still_faults`).
    pub(super) fn blocked_bodies_guard(&self, awaiting_builder: bool) -> BlockGuard {
        self.blocked_bodies_guard_with(awaiting_builder, None)
    }

    /// [`Vm::blocked_bodies_guard`] plus the wait this block is on — published on every eager sched of
    /// this thread, atomically with the `body_blocked` mark.
    fn blocked_bodies_guard_with(
        &self,
        awaiting_builder: bool,
        wait: Option<Arc<quiesce::PartyWait>>,
    ) -> BlockGuard {
        let g = BlockGuard {
            _party: None,
            awaiting: awaiting_builder,
            bodies: self
                .eager_scheds
                .iter()
                .flatten()
                // TICKET-103 — a fiber-owned nursery's body is a counted fiber, and marking its scope
                // `awaiting_builder` would veto a genuine deadlock. Continuations need no entry here:
                // `set_body_wait` marks the whole family.
                .filter(|s| !s.fiber_owned)
                .map(|s| {
                    s.sched
                        .set_body_wait(s.scope, wait.as_ref(), true, awaiting_builder);
                    (Arc::clone(&s.sched), s.scope)
                })
                .collect(),
            wait,
        };
        // TICKET-159 (W13-27) — the body just stopped injecting: farm runners for whatever it already
        // queued. AFTER `set_body_wait` has published `body_blocked`, which the claim reads.
        for (sched, _) in &g.bodies {
            self.farm_blocked_body_helpers(sched);
        }
        g
    }

    /// The `chezzi test --timeout` wall-clock halt, on its own so the ops that PARK a fiber can observe
    /// it too ([`Vm::chan_recv_step`], [`Vm::op_wait_poll`]) — a parked fiber reaches neither
    /// `jump_checked`'s loop back-edge nor [`Vm::block_halt_check`], so without a checkpoint at the op
    /// itself the deadline has no path to it at all (W7-17).
    ///
    /// Deliberately NOT gated on `native_reentry` (unlike the cancellation checkpoint it sits beside at
    /// those two call sites): a `--timeout` is a HARD abort that must always win, this only ever returns
    /// `Err` — it never unwinds VM state — and `block_until_deadline` already returns this same error
    /// from inside a native callback. Free when the cap is off: `Instant::now()` is read only when
    /// `self.deadline` is `Some`.
    pub(super) fn deadline_halt(&self, span: Span) -> Result<(), RuntimeError> {
        if let Some(dl) = self.deadline
            && std::time::Instant::now() >= dl
        {
            return Err(self
                .err(
                    format!("test exceeded --timeout ({}ms)", self.timeout_ms),
                    span,
                )
                .timed_out());
        }
        Ok(())
    }

    /// W7-47 — a run-wide `os.exit` issued by SOMEBODY ELSE (typically an eager `Executor` job), for
    /// the blocking loops whose party would otherwise never learn of it. Produces verbatim the shape
    /// `reduce_task_slots` already produces for a joined child's exit (`sched.rs`): `pending_exit` set
    /// plus the `"exit"` sentinel `Err`, which unwinds past every `recover:` to the driver.
    ///
    /// Deliberately does NOT set `Cut::Cancelled` — that would SWALLOW the outcome (`run_outcome`),
    /// which is the opposite of what an exit needs.
    ///
    /// Returns the error rather than a `Result` because most call sites are demote loops that must run
    /// their un-accounting (`running += 1`, `unregister_waiter`, …) BETWEEN learning
    /// of the exit and returning it, exactly as their cancel arms do.
    pub(super) fn run_exit_err(&mut self, span: Span) -> Option<RuntimeError> {
        // gaps.md W7-57 — NEVER while a `defer` is running, the one suppression `halt_requested`
        // has always had. A `defer` IS the cleanup a halt exists to run; killing it PART-WAY leaves
        // inconsistent state and is worse than either running it or skipping it — and it is
        // nondeterministic, since whether the exit lands mid-body depends on timing (measured: a
        // sibling `defer` that printed 2/8, and one killed after its `ENTER` line). Guarded HERE, at
        // the single funnel every rung routes through, rather than at the seven call sites.
        //
        // Cost: an infinitely-looping `defer` delays the exit. That is a pathological program and
        // `--timeout` (checked ABOVE this rung at every site) already covers it; a half-run cleanup on
        // an ordinary program does not trade for it.
        if self.deferring > 0 {
            return None;
        }
        let code = self.quiesce.pending()?;
        self.pending_exit = Some(code);
        Some(self.err("exit".to_string(), span))
    }

    /// The halts a party blocked in place must observe. Split out of [`Vm::block_wait_tick`] so the
    /// multi-channel `wait:` path — which polls N arms instead of waiting on one condvar, and so
    /// cannot share the tick — honours exactly the same three, rather than being the one blocking op a
    /// `--timeout` cannot reach.
    fn block_halt_check(&mut self, deadlock_msg: &str, span: Span) -> Result<(), RuntimeError> {
        // Checked HERE because a blocked job never reaches `jump_checked`'s loop back-edge, which is
        // where every other path observes the deadline. Without it `chezzi test --timeout` could not
        // kill a job blocked forever on a channel — exactly the hang eager execution makes easier to
        // write. No `back_edge_tick` throttle: this runs once per `DEMOTE_POLL_BACKOFF`, not per op.
        self.deadline_halt(span)?;
        // `shutdown_now`'s cooperative stop (D4), an enclosing scope's cancel and (TICKET-188) a child
        // fault of a nursery this party owns all arrive here.
        if let Some(e) = self.take_halt(span) {
            return Err(e);
        }
        // W7-47 — a run-wide `os.exit` from another party. BELOW cancel, so a party that already holds
        // a cancel flag keeps unwinding as `Cancelled` exactly as before (only a party with no cancel
        // flag at all — precisely top-level `main` — reaches this rung). ABOVE the deadlock verdict,
        // because an `Exit` outranks a synthesized `Deadlocked` — the same precedence
        // `reduce_task_slots` encodes, and what makes a `recv`-blocked `main` report the exit code
        // instead of a "deadlock" that is really somebody else's exit.
        if let Some(e) = self.run_exit_err(span) {
            return Err(e);
        }
        // TICKET-062 (W10-16) / TICKET-096 — a child fault recorded while the exit rung ran (DEC-134:
        // read again below the exit, and again after a positive verdict).
        if let Some(e) = self.take_halt(span) {
            return Err(e);
        }
        // TICKET-134 — test-only seam: widen the check-then-check window between the rung above and
        // the verdict below so a racing fault is deterministically reachable in a test. No-op outside
        // `#[cfg(test)]` and unless armed.
        #[cfg(test)]
        self.owner_fault_window_hook();
        // The process-wide deadlock verdict (`future.md` §2d step 0), checked LAST so the two real
        // halts still outrank it. Every counted party is registered as blocked and none of their wait
        // conditions is satisfiable ⇒ nothing in this run can ever move again, so this party faults
        // with its own site's message. No debounce: W7-12 needed two consecutive observations to rule
        // out "a value landed a microsecond before I looked", and the satisfiability re-check
        // ([`quiesce::PartyWait::satisfiable`]) answers that question directly instead of waiting a
        // tick to guess at it — a value that landed IS a satisfiable wait, so the verdict declines.
        if self.block_ctx().judged() && self.quiesce.quiesced(&self.exec_registry) {
            // TICKET-134 — the child can record its fault and complete between the rung above and
            // this verdict. A verdict that saw the nursery complete took the SchedCore lock after the
            // child's fault-slot write, so this re-read sees the fault.
            if let Some(e) = self.take_halt(span) {
                return Err(e);
            }
            // TICKET-136 — `run_exit_err` is suppressed inside a `defer` (W7-57), so a pending run-wide
            // `os.exit` from another party must be read HERE, or a `defer` judged deadlocked would
            // report `deadlock` for somebody else's exit. The exit outranks the verdict (Go: exit 3).
            if self.deferring > 0
                && let Some(code) = self.quiesce.pending()
            {
                self.pending_exit = Some(code);
                return Err(self.err("exit".to_string(), span));
            }
            return Err(self.err(deadlock_msg.to_string(), span).deadlock());
        }
        // TICKET-052 — an eager `Executor` job (`mn.is_none()`) about to wait another tick hands its
        // pool thread to a replacement, so the job that would unblock it can still get a thread.
        self.yield_pool_slot(Some(DEMOTE_POLL_BACKOFF));
        Ok(())
    }

    /// Sleep until `deadline`, observing the same halts every other blocking-in-place path does
    /// ([`Vm::block_halt_check`]) once per [`DEMOTE_POLL_BACKOFF`] — the CONTINUOUS-checkpoint
    /// contract for a wait whose deadline WE own (`time.sleep_ms`, a `timer(ms)` channel's `recv`).
    ///
    /// **Why chunked `thread::sleep` and not a condvar.** A plain sleep has no channel to wait on, so
    /// a waker would have to be notified from every cancel-trip site AND still carry a timeout for the
    /// wall-clock deadline (which nobody notifies at all). `DEMOTE_POLL_BACKOFF` is the same bound
    /// every other blocking path here already pays ([`Vm::block_wait_tick`], `demote_recv_block`,
    /// `demote_block_socket`): ≤5 ms of cancel latency, 200 wakes/s per SLEEPING thread.
    ///
    /// **The sleeper is deliberately NOT registered** as a blocked party ([`Vm::block_party_guard`]).
    /// It is a live, unregistered party, so `blocked < live` and the process-wide verdict always
    /// declines — the safe direction (it can only delay someone else's fault, never fabricate one),
    /// and exactly what `inflight` does for the M:N side of the same sleep. A `PartyWait::Sleep` would
    /// be a false-deadlock generator: a sleeper's wait is never unsatisfiable, it always ends.
    /// `block_halt_check`'s `deadlock_msg` is therefore unreachable from here — the argument is
    /// inherited, not intended.
    ///
    /// **`--max-heap` reaches this loop only through the CANCEL arm, and only when the over-allocating
    /// task shares a cancel scope with the sleeper** — a nursery sibling or an `Executor` job, whose
    /// over-memory hard halt (`executor_hard_halt`) trips a flag this loop reads (measured: 365 ms).
    /// It does NOT reach a sleeping **top-level `main`**, which has no cancel flag and whose own heap
    /// is not the one growing — `--max-heap` is a per-`Vm` live-heap cap, so there is nothing here for
    /// a sleeper in a different heap to observe (measured: the sleep runs in full, 3005 ms, then the
    /// OVER-MEMORY verdict lands). `--timeout` has no such gap: it is an absolute wall-clock deadline
    /// this loop reads directly.
    ///
    /// TICKET-141 — releases this thread's width permit for the whole wait and re-takes it after.
    pub(super) fn block_until_deadline(
        &mut self,
        deadline: std::time::Instant,
        span: Span,
    ) -> Result<(), RuntimeError> {
        self.width_release();
        let r = self.block_until_deadline_in_place(deadline, span);
        self.width_acquire();
        r
    }

    fn block_until_deadline_in_place(
        &mut self,
        deadline: std::time::Instant,
        span: Span,
    ) -> Result<(), RuntimeError> {
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(());
            }
            self.block_halt_check(EMPTY_RECV_DEADLOCK, span)?;
            std::thread::sleep(DEMOTE_POLL_BACKOFF.min(deadline - now));
        }
    }

    /// Block on an empty `recv` until a value arrives, instead of declaring a deadlock (see
    /// [`Vm::block_wait_tick`]). Used by an eagerly-dispatched `Executor` job and by the top-level
    /// `main` thread — both own their OS thread and have no scheduler to park a fiber into. The settle
    /// order matches [`Vm::demote_recv_block`]'s exactly — a queued value beats a `trip()` latch,
    /// which beats closed-and-drained, which beats a cancel — so the two blocking-in-place paths
    /// cannot disagree about what a channel is saying.
    pub(super) fn block_recv(
        &mut self,
        core: &Arc<ChannelCore>,
        span: Span,
    ) -> Result<RecvStep, RuntimeError> {
        loop {
            // TICKET-185 — a rendezvous receiver publishes its SLOT in the same hold as the empty
            // re-check, so a sender that arrives during the wait commits the slot instead of
            // finding nobody. A fresh `Pending` per iteration; the previous one is settled below.
            let p = Pending::new();
            {
                let mut q = core.q.lock().unwrap_or_else(|e| e.into_inner());
                match core.recv_ready(&mut q) {
                    core::RecvReady::Value(w) => return Ok(RecvStep::Got(w)),
                    core::RecvReady::Fired => return Ok(RecvStep::Got(WireValue::Bool(true))),
                    core::RecvReady::Closed => return Ok(RecvStep::ClosedEmpty),
                    core::RecvReady::Wait => {}
                }
                if core.cap == Some(0) {
                    q.slot(&p, 0);
                }
            }
            let op = PendingOp::new(Arc::clone(&p), vec![(Arc::clone(core), 0, false)]);
            // Registered ONLY for the wait below — a per-iteration guard, so it is dropped before the
            // next `pop` attempt at the loop head. **That scoping is load-bearing, and holding the
            // registration across the attempt is a false-deadlock bug**: `pop()` and un-registering
            // are not one atomic step, so a party still registered while it consumes a value is
            // counted as parked at the very instant it made progress. Measured (a 300-handoff
            // gate/data pipeline, `an_eager_wait_block_is_woken_by_its_arm_not_by_the_poll_timeout`):
            // the consumer waits on `data`, the producer pops `gate` and is momentarily "blocked on an
            // empty gate" between the pop and the return — all parties registered, none satisfiable —
            // and the run faulted 6/10. The inverse costs nothing: an unregistered party makes
            // `blocked < live`, which only DECLINES a verdict (a delayed fault, never a wrong one).
            let party = self.block_party_guard(quiesce::PartyWait::Recv(
                Arc::clone(core),
                Some(Arc::clone(&p)),
            ));
            // Ready == the settle conditions the loop head consumes (a value to take, `closed`, a
            // `trip()` latch), plus this receiver's own slot being filled, so the wait cannot sleep
            // through a state the next iteration would immediately take (W7-13). All are written
            // under `core.q` (`done_latch` since W7-13r(b)), so re-checking them under the guard the
            // wait consumes closes the window.
            let r = self.block_wait_tick(core, EMPTY_RECV_DEADLOCK, span, |g| {
                g.recv_ready_for(Some(&p))
                    || g.closed
                    || core.done_latch.load(Ordering::Relaxed)
                    || !p.is_queued()
            });
            drop(party);
            // Settle before acting on a fault: a filled slot is a delivered value.
            if let Settled::Got(_, w) = op.settle() {
                return Ok(RecvStep::Filled(w));
            }
            r?;
        }
    }

    /// `wait:` runtime (§6d) — execute [`Op::WaitPoll`]. The `n` arm channel handles are on the
    /// operand stack (`stack[base..base+n]`, source order). Poll source order: the first channel with
    /// a queued value (or a fired timer) wins → drop the handles, push the value, jump to that arm's
    /// body. A closed+empty arm is skipped. Nothing ready → run `else` (jump), else fault (all-closed)
    /// or block: an M:N snapshot-park, an M:N in-callback demote, or an in-place condvar wait for a
    /// party that owns its OS thread (an eager `Executor` job / top-level `main`, plus — for a TIMED
    /// wait only — either of those inside a native callback). A live timer arm is just another arm on
    /// every one of those; only the inline outermost-`parallel:` builder mid-body with no eager core
    /// (`BlockCtx::Builder { job: false }`) still inline-sleeps to the soonest deadline (`gaps.md` N10, and
    /// W7-14 for why the remaining inline-sleep is exactly that narrow).
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
        if self.deferring == 0 {
            self.deadline_halt(span)?;
        }
        let mut soonest: Option<(usize, std::time::Instant)> = None;
        let mut all_closed = true;
        let mut off = 0usize;
        for i in 0..n {
            let slot = base + off;
            let Some(h) = self.stack[slot].as_obj() else {
                unreachable!("wait arm operand is not a channel handle");
            };
            off += slot_width(meta.is_send[i]);
            let core = self.channel_core(h);
            if meta.is_send[i] {
                // SEND arm — `send`'s own decision (`Vm::send_commit`, no offer): the value moves
                // (buffer room, a parked receiver's slot, unbounded) → take the arm; a closed channel
                // is READY-and-FAULTS (Go's panic-on-send-to-closed; the exact bare-`send` message),
                // SELECTED not skipped — first-ready wins in source order; otherwise not ready.
                let val = self.stack[slot + 1];
                let w = self.to_wire_crossable(val, span)?;
                match self.send_commit(h, &core, w, None) {
                    SendOutcome::Sent => {
                        self.take_wait_send_arm(base, meta.arm_targets[i]);
                        return Ok(());
                    }
                    SendOutcome::Closed => return Err(self.err(CLOSED_SEND.to_string(), span)),
                    SendOutcome::Full | SendOutcome::Offered => {
                        all_closed = false; // live — a receiver will take its offer
                        continue;
                    }
                }
            }
            let ready = core.recv_ready(&mut core.q.lock().unwrap());
            match ready {
                core::RecvReady::Value(w) => {
                    self.wake_senders(h); // a `wait:` arm freed a slot — wake a parked bounded sender
                    let v = self.from_wire(w);
                    self.take_wait_arm(base, v, meta.arm_targets[i]);
                    return Ok(());
                }
                core::RecvReady::Fired => {
                    self.take_wait_arm(base, Value::bool(true), meta.arm_targets[i]);
                    return Ok(());
                }
                core::RecvReady::Closed => {}
                core::RecvReady::Wait => {
                    all_closed = false;
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
        // Every arm closed+empty, no `else`, no timer → distinct fault. (A live timer arm set
        // `all_closed = false` above, so this fires only when there is genuinely nothing to wait on.)
        if all_closed {
            return Err(self.err("wait: all channels closed".to_string(), span));
        }
        // Block on all live arms. The arm operands are on the stack (they root the channels + re-supply
        // the poll on resume). `keys[i]` = (channel handle, is_send), in arm order.
        let keys: Vec<(GcRef, bool)> = {
            let mut v = Vec::with_capacity(n);
            let mut off = 0usize;
            for &is_send in &meta.is_send {
                let h = self.stack[base + off]
                    .as_obj()
                    .expect("wait arm operand is not a channel handle");
                v.push((h, is_send));
                off += slot_width(is_send);
            }
            v
        };
        let has_send = keys.iter().any(|&(_, is_send)| is_send);
        let wait_spec = WaitSpec::Wait {
            deadline: soonest.is_some(),
            has_send,
        };
        let wait_mode = self.block_mode(wait_spec);
        // The `Refuse` cells (the inline outermost-`parallel:` builder with no worker loop and no
        // deadline to sleep to): nothing may block, so nothing is published.
        if wait_mode == BlockMode::Refuse {
            let msg = if has_send {
                FULL_SEND_DEADLOCK
            } else {
                EMPTY_WAIT_DEADLOCK
            };
            return Err(self.err(msg.to_string(), span));
        }
        if wait_mode == BlockMode::Park {
            // W7-17 — the ungated park checkpoint (see `chan_recv_step`'s): everything above settled
            // without blocking, so a hard abort had no business preempting it, but a PARK past the
            // deadline reaches no back-edge and no `block_halt_check` and would hang — including inside
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
        let mut off = 0usize;
        for (i, &(h, is_send)) in keys.iter().enumerate() {
            let slot = base + off;
            off += slot_width(is_send);
            let core = self.channel_core(h);
            if is_send {
                let val = self.stack[slot + 1];
                let w = self.to_wire_crossable(val, span)?;
                let sum = crate::vm::core::wire_summary(&w);
                let mut g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                if g.closed || g.send_ready_for(core.cap, Some(&p)) {
                    ready = true;
                    break;
                }
                g.offer(&p, i as u32, sum, w);
                drop(g);
                offered.push(h);
            } else if wait_mode != BlockMode::Park {
                let mut g = core.q.lock().unwrap_or_else(|e| e.into_inner());
                if g.recv_ready_for(Some(&p)) || core.done_latch.load(Ordering::Relaxed) {
                    ready = true;
                    break;
                }
                if !g.closed && core.cap == Some(0) {
                    g.slot(&p, i as u32);
                }
            }
            op.at.push((core, i as u32, is_send));
        }
        if ready {
            let settled = op.settle();
            return self
                .wait_settled(settled, base, meta, span)
                .unwrap_or_else(|| {
                    self.frames.last_mut().unwrap().ip -= 1; // re-run this WaitPoll: it polls again
                    Ok(())
                });
        }
        // A published offer wakes no parked fiber (see `chan_send_step`): a parked cap-0 receiver
        // holds a live slot, so the failed `give` proves none can take it. Only a party blocking in
        // place re-checks a predicate, and the channel's condvar reaches it.
        for h in offered {
            self.channel_core(h).cv.notify_all();
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
                // cancellation checkpoint already returned in that case (on BOTH engines).
                let sched = self.mn.clone().unwrap();
                let key = self.channel_core_ptr(keys[i].0);
                let core_job = self.channel_core(keys[i].0);
                let sched_job = Arc::clone(&sched);
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
                            if std::time::Instant::now() >= deadline {
                                sched_job.send_wake(key, &core_job, WireValue::Bool(true));
                            } else {
                                deadline_gap_wake(&sched_job, key, &core_job, &gap_cancel);
                            }
                            sched_job.inflight.fetch_sub(1, Ordering::Relaxed);
                        }),
                    );
                }
            }
            self.pending = Some(op);
            self.frames.last_mut().unwrap().ip -= 1; // re-run this WaitPoll on resume
            self.wait_suspend = Some(keys);
            return Ok(());
        }
        let arms: Vec<(Arc<ChannelCore>, bool)> = keys
            .iter()
            .map(|&(h, is_send)| (self.channel_core(h), is_send))
            .collect();
        // M:N inside a native callback (`native_reentry > 0`): a host-stack loop frame sits between the
        // worker loop and here, so we cannot snapshot-park. Demote: block this worker in place until
        // an arm may be ready (its offers and slots stay published), then REWIND so this op settles
        // and re-polls. A live timer arm (`soonest`) bounds the wait (so a real send still beats the
        // timer on the re-poll). Lower throughput but sound — the documented v1 limit (§6d).
        if wait_mode == BlockMode::Demote {
            if let Err(e) = self.demote_wait_block(arms, &p, soonest, span) {
                let settled = op.settle();
                return self
                    .wait_settled(settled, base, meta, span)
                    .unwrap_or(Err(e));
            }
            self.pending = Some(op);
            self.frames.last_mut().unwrap().ip -= 1;
            return Ok(());
        }
        // **W7-14 — a live timer arm must not swallow the siblings, and this pair of gates is the
        // whole fix.** A party that owns its OS thread (an eager `Executor` job, the top-level `main`
        // thread — with or without a native callback frame under it) blocks in place with the timer
        // as one more arm and the wait merely CLAMPED to its deadline (below). The one `InlineSleep`
        // cell left after `--serial`'s removal: the INLINE outermost-`parallel:` builder mid-body
        // (`BlockCtx::Builder { job: false }`: `mn == None`, `mn_enlist_sched == Some`) with no eager
        // `Executor` core. It has no worker loop to drive a park, so a live timer arm inline-sleeps to
        // the soonest deadline — the alternative is the all-parties-blocked fault, which would be
        // wrong here. Its published offers and slots stay live for the sleep, so a receiver or sender
        // that arrives mid-window still wins, as in Go.
        if let Some((i, deadline)) = soonest
            && wait_mode == BlockMode::InlineSleep
        {
            // W7-17 — CHUNKED, not a bare `thread::sleep`: the deadline is OURS, so it is a checkpoint
            // for its whole duration.
            let slept = self.block_until_deadline(deadline, span);
            let settled = op.settle();
            if let Some(r) = self.wait_settled(settled, base, meta, span) {
                return r;
            }
            slept?;
            self.take_wait_arm(base, Value::bool(true), meta.arm_targets[i]);
            return Ok(());
        }
        // A party that owns its OS thread — an eager `Executor` job, or the top-level `main` thread,
        // now including one inside a native callback (TICKET-062, W10-1) — blocks instead of declaring
        // a deadlock, like the empty-`recv` and full-`send` cases, then REWINDs so the dispatch loop
        // re-runs this `WaitPoll`, which settles and re-polls every arm. Rewinding rather than looping
        // in place also means the halts land on the ordinary back-edge checkpoint.
        if wait_mode == BlockMode::InPlace {
            // Registered FIRST, before the halt check, so this party counts ITSELF as blocked; after
            // it, a lone `wait:`-blocked party would forever see `blocked < live` and never fault.
            // The registration is an OR-set over every arm (§2d's OR-edge: ready on ANY arm is
            // progress), so the verdict declines while any one of them is feedable.
            let (first, is_send0) = arms[0].clone(); // non-empty: an all-closed arm set returned above
            let party =
                self.block_party_guard(quiesce::PartyWait::Wait(arms, Some(Arc::clone(&p))));
            if let Err(e) = self.block_halt_check(EMPTY_WAIT_DEADLOCK, span) {
                drop(party);
                let settled = op.settle();
                return self
                    .wait_settled(settled, base, meta, span)
                    .unwrap_or(Err(e));
            }
            // W7-13r(a) — wait on the FIRST arm's condvar with the tick as the timeout (every other
            // arm is observed within a tick). **The predicate must mirror what the re-run SETTLES on,
            // arm kind by arm kind — not what merely "changed"**: a wrong predicate is a live-lock
            // (a recv arm that read `|| g.closed` spun at 99% CPU on Go's ordinary
            // `select { case <-done: ; case v := <-work: }` with `done` closed). So:
            //   * any arm is ready once the `wait:`'s `Pending` settled (an offer taken, a slot
            //     filled, an offer closed);
            //   * a RECV arm is also ready on a value it can take (a buffered value or another
            //     party's live offer) or a `trip()` latch. NOT `closed` (the poll skips a dead arm).
            //
            // **The timer clamp — W7-14.** A live timer arm shortens the tick to its deadline, so the
            // re-poll takes the timer arm AT its deadline; before it, a sibling's value wins.
            let tick = soonest.map_or(DEMOTE_POLL_BACKOFF, |(_, d)| {
                DEMOTE_POLL_BACKOFF.min(d.saturating_duration_since(std::time::Instant::now()))
            });
            let arm0_ready = |g: &mut crate::vm::core::ChanState| {
                !p.is_queued()
                    || (!is_send0
                        && (g.recv_ready_for(Some(&p)) || first.done_latch.load(Ordering::Relaxed)))
            };
            // TICKET-168 — DEC-141's bracket list missed this wait; a body wait: hangs at T=1 without it.
            self.width_release();
            let q = first.q.lock().unwrap_or_else(|e| e.into_inner());
            #[cfg_attr(not(test), allow(unused_mut))]
            let (mut guard, waited) = first
                .cv
                .wait_timeout_while(q, tick, |g| !arm0_ready(g))
                .unwrap_or_else(|e| e.into_inner());
            #[cfg(test)]
            {
                WAIT_ARM0_BLOCKS.fetch_add(1, Ordering::Relaxed);
                if waited.timed_out() && arm0_ready(&mut guard) {
                    WAIT_ARM0_SLEPT_WHILE_READY.fetch_add(1, Ordering::Relaxed);
                }
            }
            drop((guard, waited));
            self.width_acquire();
            drop(party);
            self.pending = Some(op);
            self.frames.last_mut().unwrap().ip -= 1;
            return Ok(());
        }
        // Inside a native callback: the host stack cannot be unwound to park and there is no thread
        // of our own to block on (mirrors `chan_recv_step`'s callback fault).
        Err(self.err(EMPTY_WAIT_DEADLOCK.to_string(), span))
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
            Settled::Got(i, w) => {
                let v = self.from_wire(w);
                self.take_wait_arm(base, v, meta.arm_targets[i as usize]);
                Some(Ok(()))
            }
            Settled::Closed => Some(Err(self.err(CLOSED_SEND.to_string(), span))),
            Settled::Cancelled => None,
        }
    }

    /// Commit a chosen `wait` arm: drop the `n` channel handles (`stack[base..]`), push the received
    /// value, and jump to the arm body's target ip (the bind/assign/discard prologue).
    pub(super) fn take_wait_arm(&mut self, base: usize, value: Value, target: usize) {
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

    /// `Shared[T]` methods (C3/C4): `get` (copies out), `set` (copies in), `update` (read-modify-write
    /// via the re-entrant call path). The box is re-rooted on
    /// the operand stack across `update`'s nested call (the receiver was popped in `do_method_call`).
    pub(super) fn shared_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "get" => {
                self.arity_err("get", args, 0, span)?;
                // Clone the wire form out under the lock, then reconstruct into this heap (one
                // round-trip == the old deep_clone-out).
                let w = self.shared_core(h).v.lock().unwrap().clone();
                Ok(self.from_wire(w))
            }
            "set" => {
                self.arity_err("set", args, 1, span)?;
                let w = self.to_wire_crossable(args[0], span)?;
                let core = self.shared_core(h);
                let key = Arc::as_ptr(&core) as usize;
                let _guard = self.take_update_guard(key, "a Shared update guard", span)?;
                core.store(w);
                Ok(Value::nil())
            }
            "update" => {
                self.arity_err("update", args, 1, span)?;
                let f = args[0];
                let core = self.shared_core(h);
                // TICKET-016 (W8-3): the whole read-modify-write is serialised across threads by the
                // box's update guard, taken via the process-global wait-for graph (`core.rs`) instead
                // of a bare `Mutex` — so a same-box re-entry FAULTS (the length-1 cycle) instead of
                // hanging, and a cross-box wait cycle faults instead of hanging undetected. The value
                // lock `v` is still held only briefly — read here, write at the end — so the closure
                // may freely re-enter `get` (or `update` on a *different*, non-cyclic box). The
                // handle is re-rooted on the operand stack so the nested call's GC keeps the core's
                // contents traced (the receiver was popped off the stack in `do_method_call`).
                let key = Arc::as_ptr(&core) as usize;
                let _guard = self.take_update_guard(key, "a Shared update guard", span)?;
                let w = core.v.lock().unwrap().clone();
                let cur = self.from_wire(w);
                self.push(Value::obj(h));
                let next = self.reentered(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                let next = next?;
                let stored = self.to_wire_crossable(next, span)?;
                core.store(stored);
                Ok(Value::nil())
            }
            _ => Err(self.err(format!("type Shared has no method '{method}'"), span)),
        }
    }

    /// TICKET-192 — the ONE probe of a stored `RwShared` `Map`/`Set`: which position holds a key equal
    /// to `needle` (whose hash is `qh`)? Walks the `HashIndex` candidates for `qh`, so it is O(1)
    /// expected. Returns the position and, when `with_value`, the rebuilt value of a `Map` entry.
    ///
    /// Per candidate it takes the read guard, clones the key wire (and the value wire), rebuilds both
    /// with ONE `from_wire_piece` map per entry (DEC-154), and DROPS the guard before the `eq`, which
    /// may run user code that re-enters this box. Each guard re-reads `core.generation`: if it moved
    /// since the first guard, a write shifted positions under the probe and it restarts from the first
    /// candidate. A miss is therefore exact as of the last guard, and a hit was present as of its own.
    ///
    /// `Ok(None)` also covers a stored value that is no longer a `Map`/`Set` (a concurrent `set`);
    /// the callers check the element kind before probing.
    pub(super) fn rwshared_probe(
        &mut self,
        core: &Arc<RwSharedCore>,
        qh: u64,
        needle: Value,
        with_value: bool,
        span: Span,
    ) -> Result<Option<(usize, Option<Value>)>, RuntimeError> {
        'restart: loop {
            let mut first_gen: Option<u64> = None;
            let mut j = 0;
            loop {
                let (pos, k, v) = {
                    let g = core.v.read().unwrap();
                    let now = core.generation.load(std::sync::atomic::Ordering::Relaxed);
                    match first_gen {
                        None => first_gen = Some(now),
                        Some(was) if was != now => continue 'restart,
                        Some(_) => {}
                    }
                    let (pos, kw, vw) = match &*g {
                        WireValue::Map { entries, .. } => match entries.candidates(qh).get(j) {
                            Some(&p) => (
                                p,
                                entries[p].1.clone(),
                                with_value.then(|| entries[p].2.clone()),
                            ),
                            None => return Ok(None),
                        },
                        WireValue::Set { entries, .. } => match entries.candidates(qh).get(j) {
                            Some(&p) => (p, entries[p].1.clone(), None),
                            None => return Ok(None),
                        },
                        _ => return Ok(None),
                    };
                    // TICKET-154: ONE map per ENTRY, so a value that aliases its own key rebuilds as
                    // one object.
                    let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                    let k = self.from_wire_piece(&g, kw, &mut rb);
                    let v = vw.map(|vw| self.from_wire_piece(&g, vw, &mut rb));
                    (pos, k, v)
                };
                // ROOT both reconstructions (fresh, Rust-local) across the possibly re-entrant eq.
                let roots = [k, v.unwrap_or_else(Value::nil)];
                if self.with_roots(&roots, |vm| vm.values_equal_guarded(k, needle, 0, span))? {
                    return Ok(Some((pos, v)));
                }
                j += 1;
            }
        }
    }

    /// TICKET-192 — the ONE single-entry writer of a stored `RwShared[Map[K, V]]`: `set_key`,
    /// `remove_key`, `get_or_insert`. Takes the update guard exactly as `set`/`write` do (DEC-016),
    /// probes with [`rwshared_probe`](Self::rwshared_probe), and splices the one entry into the stored
    /// table instead of re-encoding the map:
    ///
    /// 1. Insert serializes key then value in ONE memo seeded at the table's id ceiling, so the new
    ///    ids never collide with stored ones (a whole `get()` would otherwise tie two entries).
    /// 2. Overwrite keeps the STORED key (CPython `d[k] = v`) and serializes the value alone.
    /// 3. Remove and overwrite splice in place only when every stored piece stands alone (`flat`):
    ///    otherwise another entry may `Backref` into the one replaced, so they decode the whole map,
    ///    mutate it on the heap and re-encode it (the pre-TICKET-192 cost).
    ///
    /// The summary moves by the spliced pieces' bytes and never turns DIRTY into CLEAN.
    pub(super) fn rwshared_map_write_entry(
        &mut self,
        h: GcRef,
        op: EntryWrite,
        span: Span,
    ) -> Result<Value, RuntimeError> {
        let (name, key, val) = match op {
            EntryWrite::Put(k, v) => ("set_key", k, v),
            EntryWrite::Remove(k) => ("remove_key", k, Value::nil()),
            EntryWrite::GetOrInsert(k, v) => ("get_or_insert", k, v),
        };
        // Root the receiver, key and value across a user `hash` (VM re-entry, may GC).
        let qh = self.hash_key_rooted(key, &[Value::obj(h), key, val], span)?;
        let core = self.rwshared_core(h);
        let _guard =
            self.take_update_guard(Arc::as_ptr(&core) as usize, "a RwShared update guard", span)?;
        if !matches!(&*core.v.read().unwrap(), WireValue::Map { .. }) {
            return Err(self.err(format!("RwShared.{name} requires a map element"), span));
        }
        self.with_roots(&[Value::obj(h), key, val], |vm| {
            let with_value = !matches!(op, EntryWrite::Put(..));
            let probed = vm.rwshared_probe(&core, qh, key, with_value, span)?;
            let pos = match (op, probed) {
                (EntryWrite::GetOrInsert(..), Some((_, v))) => {
                    return Ok(v.expect("get_or_insert probes with the value"));
                }
                (EntryWrite::Remove(_), None) => {
                    return Ok(vm.alloc_enum("Option", "None", vec![]));
                }
                (_, p) => p,
            };
            // Hold the removed value (a fresh rebuild) rooted until it is returned.
            let removed = pos.and_then(|(_, v)| v).unwrap_or_else(Value::nil);
            vm.with_roots(&[removed], |vm| {
                let (ceiling, flat) = {
                    let g = core.v.read().unwrap();
                    match &*g {
                        WireValue::Map { entries, .. } => (
                            entries.id_ceiling(),
                            entries
                                .flat()
                                .unwrap_or_else(|| Vm::wire_pieces_are_self_contained(&g)),
                        ),
                        _ => unreachable!("the update guard keeps the element a map"),
                    }
                };
                if pos.is_some() && !flat {
                    vm.rwshared_map_write_entry_slow(&core, op, qh, span)?;
                } else {
                    vm.rwshared_map_splice(
                        &core,
                        op,
                        pos.map(|(i, _)| i),
                        qh,
                        ceiling,
                        flat,
                        span,
                    )?;
                }
                Ok(match op {
                    EntryWrite::Put(..) => Value::nil(),
                    EntryWrite::Remove(_) => vm.alloc_enum("Option", "Some", vec![removed]),
                    EntryWrite::GetOrInsert(_, v) => v,
                })
            })
        })
    }

    /// The in-place half of [`rwshared_map_write_entry`](Self::rwshared_map_write_entry): `pos` is the
    /// probed position (`None` = insert). The caller holds the update guard, so `pos` is stable.
    #[allow(clippy::too_many_arguments)]
    fn rwshared_map_splice(
        &mut self,
        core: &Arc<RwSharedCore>,
        op: EntryWrite,
        pos: Option<usize>,
        qh: u64,
        ceiling: u32,
        flat: bool,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let none = super::fxhash::FxHashMap::<u32, GcRef>::default();
        let mut memo = super::sched::WireMemo::seeded(ceiling);
        let (kw, vw) = match (op, pos) {
            (EntryWrite::Put(k, v) | EntryWrite::GetOrInsert(k, v), None) => {
                let kw = self.to_wire_crossable_memo(k, span, &mut memo)?;
                (
                    Some(kw),
                    Some(self.to_wire_crossable_memo(v, span, &mut memo)?),
                )
            }
            (EntryWrite::Put(_, v), Some(_)) => {
                (None, Some(self.to_wire_crossable_memo(v, span, &mut memo)?))
            }
            _ => (None, None),
        };
        let entry_flat = kw
            .iter()
            .chain(vw.iter())
            .all(|w| w.backrefs_resolvable(&none));
        let (add_bytes, add_dirty) = kw.iter().chain(vw.iter()).fold((0, false), |acc, w| {
            let (b, d) = super::core::wire_summary(w);
            (acc.0 + b, acc.1 | d)
        });
        let next_id = memo.next_id();
        core.with_map_mut(|m| {
            let sub_bytes = match (kw, vw, pos) {
                (Some(kw), Some(vw), None) => {
                    m.push((qh, kw, vw));
                    0
                }
                (None, Some(vw), Some(i)) => super::core::wire_summary(&m.replace_value(i, vw)).0,
                (None, None, Some(i)) => {
                    let (_, k, v) = m.remove_at(i);
                    std::mem::size_of::<u64>()
                        + super::core::wire_summary(&k).0
                        + super::core::wire_summary(&v).0
                }
                _ => unreachable!("splice: op and position disagree"),
            };
            let add_bytes = if pos.is_none() {
                add_bytes + std::mem::size_of::<u64>()
            } else {
                add_bytes
            };
            m.raise_ceiling(next_id);
            m.set_flat(flat && entry_flat);
            core.summary.adjust(add_bytes, add_dirty, sub_bytes);
        })
        .expect("the update guard keeps the element a map");
        Ok(())
    }

    /// The decode-mutate-re-encode half of
    /// [`rwshared_map_write_entry`](Self::rwshared_map_write_entry), for a remove or overwrite on a
    /// map whose pieces do not all stand alone. The caller holds the update guard and roots the key
    /// and value.
    fn rwshared_map_write_entry_slow(
        &mut self,
        core: &Arc<RwSharedCore>,
        op: EntryWrite,
        qh: u64,
        span: Span,
    ) -> Result<(), RuntimeError> {
        let w = core.v.read().unwrap().clone();
        let mv = self.from_wire(w);
        let mh = mv.as_obj().expect("a stored map rebuilds as a heap map");
        self.with_roots(&[mv], |vm| {
            match op {
                EntryWrite::Put(k, v) => vm.map_upsert_in_heap(mh, qh, k, v, span)?,
                EntryWrite::Remove(k) => {
                    if let Some(i) = vm.map_probe(mh, qh, k, span)? {
                        let Obj::Map(m) = vm.heap.get_mut(mh) else {
                            unreachable!()
                        };
                        m.remove_at(i);
                    }
                }
                EntryWrite::GetOrInsert(..) => unreachable!("an insert never takes the slow path"),
            }
            let stored = vm.to_wire_crossable(mv, span)?;
            core.store(stored);
            Ok(())
        })
    }

    /// `RwShared[T]` methods: `get`/`set` (read/write-guarded copy out/in), `read(f)` (SHARED read
    /// guard: clone out, drop guard, run `f`, return its result — NO write-back), `write(f)`
    /// (EXCLUSIVE write guard: a write-locked read-modify-write, the `Shared.update` shape under a
    /// `RwLock`). As with `Shared.update`, the lock guard is
    /// dropped across the user closure (a `RwLock` guard is not reentrant) and the receiver is
    /// re-rooted on the operand stack so the nested call's GC keeps the core's contents traced (the
    /// receiver was popped off the stack in `do_method_call`). `write`'s whole RMW is serialised
    /// across threads by a separate `update_lock`, held UNCONDITIONALLY for the entire RMW — the
    /// `RwLock` write guard alone is NOT enough because it is dropped across the closure, so two
    /// writers could clone the same base and lose an update (same discipline as `Shared.update`). A
    /// closure that re-acquires the SAME box's write lock (or a write inside a read) deadlocks — a
    /// documented edge, mirroring `Shared.update`'s same-box re-entry limit.
    pub(super) fn rwshared_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "get" => {
                self.arity_err("get", args, 0, span)?;
                // Clone the wire form out under the SHARED read guard, reconstruct into this heap.
                let w = self.rwshared_core(h).v.read().unwrap().clone();
                Ok(self.from_wire(w))
            }
            "set" => {
                self.arity_err("set", args, 1, span)?;
                // TICKET-154: same rule as every other cross-heap store; the read views resolve aliases on the rebuild side.
                let w = self.to_wire_crossable(args[0], span)?;
                let core = self.rwshared_core(h);
                let key = Arc::as_ptr(&core) as usize;
                let _guard = self.take_update_guard(key, "a RwShared update guard", span)?;
                core.store(w);
                Ok(Value::nil())
            }
            "read" => {
                self.arity_err("read", args, 1, span)?;
                let f = args[0];
                let core = self.rwshared_core(h);
                // SHARED read guard: clone the value out, then DROP the guard before invoking `f`
                // (the guard is not reentrant; dropping it also lets other readers/a writer proceed).
                // No write-back — `read` returns `f`'s result.
                let w = core.v.read().unwrap().clone();
                let cur = self.from_wire(w);
                self.push(Value::obj(h));
                let result = self.reentered(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                result
            }
            // TICKET-192: the single-entry writers of a `Map` element (update guard, O(1) expected).
            "set_key" => {
                self.arity_err("set_key", args, 2, span)?;
                self.rwshared_map_write_entry(h, EntryWrite::Put(args[0], args[1]), span)
            }
            "remove_key" => {
                self.arity_err("remove_key", args, 1, span)?;
                self.rwshared_map_write_entry(h, EntryWrite::Remove(args[0]), span)
            }
            "get_or_insert" => {
                self.arity_err("get_or_insert", args, 2, span)?;
                self.rwshared_map_write_entry(h, EntryWrite::GetOrInsert(args[0], args[1]), span)
            }
            "write" => {
                self.arity_err("write", args, 1, span)?;
                let f = args[0];
                let core = self.rwshared_core(h);
                // TICKET-016 (W8-3): the whole read-modify-write is serialised across threads by the
                // box's update guard (the process-global wait-for graph in `core.rs`), exactly like
                // `Shared.update`. The `RwLock` write guard alone is NOT enough: it must be DROPPED
                // across the user closure (not reentrant), so two `write`s could clone the same base
                // and lose an update. `get`/`read` never take this guard, so `write` nested in `read`
                // still persists; a same-box `write`/`set` re-entry FAULTS instead of hanging.
                let key = Arc::as_ptr(&core) as usize;
                let _guard = self.take_update_guard(key, "a RwShared update guard", span)?;
                let w = core.v.write().unwrap().clone();
                let cur = self.from_wire(w);
                self.push(Value::obj(h));
                let next = self.reentered(|vm| vm.invoke_value(f, vec![cur], span));
                self.pop();
                let next = next?;
                // TICKET-154: same rule as every other cross-heap store; the read views resolve aliases on the rebuild side.
                let stored = self.to_wire_crossable(next, span)?;
                core.store(stored);
                Ok(Value::nil())
            }
            // Zero-copy READ-view methods on a CONTAINER element of `RwShared[T]` — `List[E]`
            // (len/at/slice/for_each/fold), `Map[K,V]` (len/get_key/has/for_each_entry/fold_entries),
            // `Set[E]` (len/contains/for_each/fold). Checker-gated to the recognized container head.
            // They walk the stored heap-independent `WireValue::List`/`Map`/`Set` Vec and `from_wire`
            // ONE entry per step — O(1) memory, never materializing the whole inner (what `get`/`read`
            // do). This is deterministic by construction: the walk reads a heap-independent wire form,
            // so every reader sees identical elements. The read-only `len`/`at`/`slice` take the shared guard only for the brief clone
            // (no user code under it). `for_each`/`fold` RE-ACQUIRE the shared guard PER ELEMENT, clone
            // one wire element, DROP the guard, then run the closure — mirroring `read`'s
            // clone-out-then-drop, per element. The guard is NEVER held across the user closure (or the
            // GC's mark of `Obj::RwShared`, which re-locks `core.v`), so a nested read/write of the SAME
            // box, an AB-BA cross-box walk, and a GC pass triggered inside the closure can't deadlock —
            // the write-preferring `std::sync::RwLock` never sees a recursive read behind a queued writer.
            //
            // W7-11 — every per-piece rebuild goes through [`Vm::from_wire_piece`] rather than
            // `from_wire`, because a piece whose cycle closes through the ROOT container is not
            // self-contained and used to ABORT THE HOST. The helper takes `&WireValue` (the caller's
            // live guard), never the core: it must not re-acquire `core.v`, and the guard must still be
            // held across the rebuild so its fallback resolves the piece against the SAME serialization
            // it was cloned from (a second acquisition is the torn read `docs/gaps.md` W7-4 round 2 hit).
            // Holding it across `from_wire*` is safe and is exactly the window `at`/`slice` already
            // held: it allocs and nothing else, and `Heap::alloc` never collects, so no GC can re-lock
            // `core.v` underneath. The guard is still DROPPED before any user code (closure/hash/eq).
            "len" => {
                self.arity_err("len", args, 0, span)?;
                let core = self.rwshared_core(h);
                let g = core.v.read().unwrap();
                let n = match &*g {
                    WireValue::List { items, .. } => items.len() as i64,
                    WireValue::Map { entries, .. } => entries.len() as i64,
                    WireValue::Set { entries, .. } => entries.len() as i64,
                    _ => {
                        return Err(
                            self.err("RwShared.len requires a container element".into(), span)
                        );
                    }
                };
                Ok(self.make_int(n))
            }
            // `at(i) -> Option[E]` — out of range is `None`, not a fault, matching the language's
            // other named-accessor spellings: `get_key(k) -> Option[V]` below and
            // `std.json.at -> Option[Json]`. (`RwShared` itself has no `[]` — it does not satisfy the
            // `Index` protocol, since a view walks the stored wire and has no heap object to dispatch
            // a user `index()` on — so this is the ONLY read accessor here, and it reports absence
            // rather than faulting.) A wrong container HEAD is still a fault: that is a type error,
            // not a missing element. Negative indexing (`at(-1)`) is unchanged — `norm_index` first.
            "at" => {
                self.arity_err("at", args, 1, span)?;
                let i = self.int_of(args[0]);
                let core = self.rwshared_core(h);
                let g = core.v.read().unwrap();
                let ew = match &*g {
                    WireValue::List { items, .. } => match crate::slice::norm_index(i, items.len())
                    {
                        Some(u) => items[u].clone(),
                        None => {
                            drop(g);
                            return Ok(self.alloc_enum("Option", "None", vec![]));
                        }
                    },
                    _ => return Err(self.err("RwShared.at requires a list element".into(), span)),
                };
                let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                let v = self.from_wire_piece(&g, ew, &mut rb);
                drop(g); // the rebuild is done — release before the `Option` wrapper allocs
                Ok(self.alloc_enum("Option", "Some", vec![v]))
            }
            "slice" => {
                self.arity_err("slice", args, 2, span)?;
                let lo = self.int_of(args[0]);
                let hi = self.int_of(args[1]);
                let core = self.rwshared_core(h);
                let g = core.v.read().unwrap();
                let idxs = match &*g {
                    WireValue::List { items, .. } => {
                        crate::slice::slice_indices(Some(lo), Some(hi), None, items.len())
                            .map_err(|e| self.err(e.to_string(), span))?
                    }
                    _ => {
                        return Err(self.err("RwShared.slice requires a list element".into(), span));
                    }
                };
                // Materialize ONLY [lo:hi] into a fresh list, rooted on the operand stack across the
                // per-element `from_wire`s (defensive — `from_wire` only allocs and `alloc` never
                // collects, but rooting matches the list-HOF precedent and is future-proof).
                let res_h = self.heap.alloc(Obj::List(Vec::new()));
                self.push(Value::obj(res_h));
                // W7-4: ONE rebuild map across the sliced-out elements — `slice` is a SINGLE crossing
                // that returns a container (like `get`), so two sliced-out closures over the same
                // captured local land on ONE cell. A per-element view (`at`, `for_each`) is its own
                // crossing and keeps its own copy.
                let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                // W7-11 — the whole-container decision is made ONCE, before the first element, and
                // that is load-bearing (adversarial review, round 2). `from_wire_piece`'s fallback
                // rebuilds the container into this shared map, and the container arms of
                // `from_wire_memo` have NO first-wins dedupe (only `Cell` does) — so a fallback taken
                // at element k OVERWRITES `rb` for elements 0..k, orphaning the copies already pushed
                // into the result. Identity then depended on element ORDER: with only element 1
                // cyclic, `sl[1].back[0]` was a different object than `sl[0]` (CPython: the same one).
                // Deciding up front means every element is served from one container, whichever of
                // them needs it.
                if idxs.iter().any(|&i| match &*g {
                    WireValue::List { items, .. } => !items[i].backrefs_resolvable(&rb),
                    _ => false,
                }) {
                    let _whole = self.from_wire_memo((*g).clone(), &mut rb);
                }
                for idx in idxs {
                    let ew = match &*g {
                        WireValue::List { items, .. } => items[idx].clone(),
                        _ => unreachable!(),
                    };
                    let elem = self.from_wire_piece(&g, ew, &mut rb);
                    if let Obj::List(items) = self.heap.get_mut(res_h) {
                        items.push(elem);
                    }
                }
                self.pop();
                Ok(Value::obj(res_h))
            }
            // `for_each(f: fn(E) -> _)` on a `List[E]` OR `Set[E]` — a per-element side-effect scan.
            "for_each" => {
                self.arity_err("for_each", args, 1, span)?;
                let f = args[0];
                let core = self.rwshared_core(h);
                // Snapshot the length under a brief guard (dropped immediately).
                let n = match &*core.v.read().unwrap() {
                    WireValue::List { items, .. } => items.len(),
                    WireValue::Set { entries, .. } => entries.len(),
                    _ => {
                        return Err(self.err(
                            "RwShared.for_each requires a list or set element".into(),
                            span,
                        ));
                    }
                };
                self.push(Value::obj(h)); // root the receiver across nested GC
                // TICKET-154: an aliased store is materialized under ONE map and ONE guard first,
                // then walked with no guard held. A self-contained store keeps the walk below.
                if let Some(all) = self.rwshared_snapshot_pieces(&core) {
                    self.push(Value::obj(all));
                    let mut i = 0;
                    while let Some(elem) = self.snapshot_piece(all, i) {
                        self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                        i += 1;
                    }
                    self.pop();
                    self.pop();
                    return Ok(Value::nil());
                }
                for i in 0..n {
                    // RE-ACQUIRE the shared guard, clone ONE element, rebuild it, DROP the guard
                    // before the closure — never hold `core.v` across `invoke_value`/GC (see the
                    // arm's header comment). The rebuild stays INSIDE the guard so W7-11's fallback
                    // resolves the piece against the same serialization it was cloned from.
                    let g = core.v.read().unwrap();
                    let ew = match &*g {
                        WireValue::List { items, .. } => {
                            if i >= items.len() {
                                break; // the list shrank under a concurrent write — stop
                            }
                            items[i].clone()
                        }
                        WireValue::Set { entries, .. } => {
                            if i >= entries.len() {
                                break;
                            }
                            entries[i].1.clone()
                        }
                        _ => break, // replaced by a non-container under a concurrent set — stop
                    };
                    let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                    let elem = self.from_wire_piece(&g, ew, &mut rb);
                    drop(g);
                    self.guarded(|vm| vm.invoke_value(f, vec![elem], span))?;
                }
                self.pop();
                Ok(Value::nil())
            }
            // `fold(init, f: fn(R, E) -> R) -> R` on a `List[E]` OR `Set[E]`.
            "fold" => {
                self.arity_err("fold", args, 2, span)?;
                let init = args[0];
                let f = args[1];
                let core = self.rwshared_core(h);
                let n = match &*core.v.read().unwrap() {
                    WireValue::List { items, .. } => items.len(),
                    WireValue::Set { entries, .. } => entries.len(),
                    _ => {
                        return Err(
                            self.err("RwShared.fold requires a list or set element".into(), span)
                        );
                    }
                };
                self.push(Value::obj(h)); // root the receiver
                self.push(init); // root the accumulator; its slot sits below every nested frame's base
                let acc_slot = self.stack.len() - 1;
                // TICKET-154: see `for_each` -- one map, one guard, then walk with no guard held.
                if let Some(all) = self.rwshared_snapshot_pieces(&core) {
                    self.push(Value::obj(all));
                    let mut i = 0;
                    while let Some(elem) = self.snapshot_piece(all, i) {
                        let acc = self.stack[acc_slot];
                        let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, elem], span))?;
                        self.stack[acc_slot] = new;
                        i += 1;
                    }
                    self.pop(); // unroot the snapshot
                    let acc = self.pop();
                    self.pop();
                    return Ok(acc);
                }
                for i in 0..n {
                    // RE-ACQUIRE per element, rebuild under the guard, DROP before the closure (see
                    // the arm's header comment).
                    let g = core.v.read().unwrap();
                    let ew = match &*g {
                        WireValue::List { items, .. } => {
                            if i >= items.len() {
                                break;
                            }
                            items[i].clone()
                        }
                        WireValue::Set { entries, .. } => {
                            if i >= entries.len() {
                                break;
                            }
                            entries[i].1.clone()
                        }
                        _ => break,
                    };
                    let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                    let elem = self.from_wire_piece(&g, ew, &mut rb);
                    drop(g);
                    let acc = self.stack[acc_slot];
                    let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, elem], span))?;
                    self.stack[acc_slot] = new;
                }
                let acc = self.pop(); // unroot accumulator
                self.pop(); // unroot receiver
                Ok(acc)
            }
            // Set[E] membership: hash the query element ONCE (guard NOT held — `hash_value` may
            // dispatch a user `hash`/GC), then `rwshared_probe` (TICKET-192): the stored table's
            // `HashIndex` candidates only, each cloned under a guard that is DROPPED before the eq.
            // The guard is NEVER held across hash/eq (same deadlock invariant as `for_each`/`fold`).
            "contains" => {
                self.arity_err("contains", args, 1, span)?;
                let needle = args[0];
                // Root the receiver `h` AND `needle` across the hash: for a struct/enum/newtype
                // element `hash_value` dispatches the user `hash` (re-enters the VM, may GC), and
                // `h`/`needle` are off the operand stack (popped at dispatch) so they'd be collectable
                // mid-hash → the following `rwshared_core(h)` would hit a freed slot. Mirrors the
                // non-RwShared Set `in` path (arith.rs:913).
                let qh = self.hash_key_rooted(needle, &[Value::obj(h), needle], span)?;
                let core = self.rwshared_core(h);
                match &*core.v.read().unwrap() {
                    WireValue::Set { .. } => {}
                    _ => {
                        return Err(
                            self.err("RwShared.contains requires a set element".into(), span)
                        );
                    }
                };
                self.push(Value::obj(h)); // root the receiver
                self.push(needle); // root the query element across from_wire/eq (may GC)
                let found = self
                    .rwshared_probe(&core, qh, needle, false, span)?
                    .is_some();
                self.pop();
                self.pop();
                Ok(Value::bool(found))
            }
            // Map[K,V].has(k) -> bool — same hash-once + per-probe re-lock discipline as `contains`,
            // comparing the KEY (entry.1 is the key wire, entry.2 the value wire).
            "has" => {
                self.arity_err("has", args, 1, span)?;
                let key = args[0];
                // Root receiver `h` AND `key` across the hash — a struct/enum/newtype key's `hash`
                // re-enters the VM and may GC; both are off the operand stack here. Mirrors the
                // non-RwShared Map path (arith.rs:921).
                let qh = self.hash_key_rooted(key, &[Value::obj(h), key], span)?;
                let core = self.rwshared_core(h);
                match &*core.v.read().unwrap() {
                    WireValue::Map { .. } => {}
                    _ => return Err(self.err("RwShared.has requires a map element".into(), span)),
                };
                self.push(Value::obj(h));
                self.push(key);
                let found = self.rwshared_probe(&core, qh, key, false, span)?.is_some();
                self.pop();
                self.pop();
                Ok(Value::bool(found))
            }
            // Map[K,V].get_key(k) -> Option[V] — probe as `has`, rebuilding the VALUE wire under the
            // same guard as the key; `Some(v)` on an eq-match, `None` on a miss.
            "get_key" => {
                self.arity_err("get_key", args, 1, span)?;
                let key = args[0];
                // Root receiver `h` AND `key` across the hash — a struct/enum/newtype key's `hash`
                // re-enters the VM and may GC; both are off the operand stack here. Mirrors the
                // non-RwShared Map path (arith.rs:921).
                let qh = self.hash_key_rooted(key, &[Value::obj(h), key], span)?;
                let core = self.rwshared_core(h);
                match &*core.v.read().unwrap() {
                    WireValue::Map { .. } => {}
                    _ => {
                        return Err(
                            self.err("RwShared.get_key requires a map element".into(), span)
                        );
                    }
                };
                self.push(Value::obj(h));
                self.push(key);
                let result = self
                    .rwshared_probe(&core, qh, key, true, span)?
                    .and_then(|(_, v)| v);
                self.pop();
                self.pop();
                Ok(match result {
                    Some(v) => self.alloc_enum("Option", "Some", vec![v]),
                    None => self.alloc_enum("Option", "None", vec![]),
                })
            }
            // Map[K,V].for_each_entry(f: fn(K, V) -> _) — per-entry side-effect scan (2-arg closure).
            "for_each_entry" => {
                self.arity_err("for_each_entry", args, 1, span)?;
                let f = args[0];
                let core = self.rwshared_core(h);
                let n = match &*core.v.read().unwrap() {
                    WireValue::Map { entries, .. } => entries.len(),
                    _ => {
                        return Err(self.err(
                            "RwShared.for_each_entry requires a map element".into(),
                            span,
                        ));
                    }
                };
                self.push(Value::obj(h));
                // TICKET-154: see `for_each` -- an aliased map is materialized once, flattened
                // key-then-value, under one map and one guard.
                if let Some(all) = self.rwshared_snapshot_pieces(&core) {
                    self.push(Value::obj(all));
                    let mut i = 0;
                    while let (Some(k), Some(v)) =
                        (self.snapshot_piece(all, i), self.snapshot_piece(all, i + 1))
                    {
                        self.guarded(|vm| vm.invoke_value(f, vec![k, v], span))?;
                        i += 2;
                    }
                    self.pop();
                    self.pop();
                    return Ok(Value::nil());
                }
                for i in 0..n {
                    // Clone AND rebuild both halves of the entry under ONE guard, dropped before the
                    // closure (W7-11 — see the arm header).
                    let (k, v) = {
                        let g = core.v.read().unwrap();
                        let (kw, vw) = match &*g {
                            WireValue::Map { entries, .. } => {
                                if i >= entries.len() {
                                    break;
                                }
                                (entries[i].1.clone(), entries[i].2.clone())
                            }
                            _ => break,
                        };
                        // TICKET-154: ONE map per ENTRY (see `get_key`).
                        let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                        let k = self.from_wire_piece(&g, kw, &mut rb);
                        // Root the reconstructed key while building the value (both alloc; `alloc`
                        // never collects, but rooting matches the receiver-rooting precedent).
                        self.push(k);
                        let v = self.from_wire_piece(&g, vw, &mut rb);
                        (self.pop(), v)
                    };
                    self.guarded(|vm| vm.invoke_value(f, vec![k, v], span))?;
                }
                self.pop();
                Ok(Value::nil())
            }
            // Map[K,V].fold_entries(init, f: fn(R, K, V) -> R) -> R — per-entry reduce (3-arg closure).
            "fold_entries" => {
                self.arity_err("fold_entries", args, 2, span)?;
                let init = args[0];
                let f = args[1];
                let core = self.rwshared_core(h);
                let n = match &*core.v.read().unwrap() {
                    WireValue::Map { entries, .. } => entries.len(),
                    _ => {
                        return Err(
                            self.err("RwShared.fold_entries requires a map element".into(), span)
                        );
                    }
                };
                self.push(Value::obj(h)); // root the receiver
                self.push(init); // root the accumulator
                let acc_slot = self.stack.len() - 1;
                // TICKET-154: see `for_each_entry`.
                if let Some(all) = self.rwshared_snapshot_pieces(&core) {
                    self.push(Value::obj(all));
                    let mut i = 0;
                    while let (Some(k), Some(v)) =
                        (self.snapshot_piece(all, i), self.snapshot_piece(all, i + 1))
                    {
                        let acc = self.stack[acc_slot];
                        let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, k, v], span))?;
                        self.stack[acc_slot] = new;
                        i += 2;
                    }
                    self.pop(); // unroot the snapshot
                    let acc = self.pop();
                    self.pop();
                    return Ok(acc);
                }
                for i in 0..n {
                    // One guard for the clone AND both rebuilds, dropped before the closure (W7-11).
                    let (k, v) = {
                        let g = core.v.read().unwrap();
                        let (kw, vw) = match &*g {
                            WireValue::Map { entries, .. } => {
                                if i >= entries.len() {
                                    break;
                                }
                                (entries[i].1.clone(), entries[i].2.clone())
                            }
                            _ => break,
                        };
                        // TICKET-154: ONE map per ENTRY (see `get_key`).
                        let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
                        let k = self.from_wire_piece(&g, kw, &mut rb);
                        self.push(k); // root key while reconstructing value
                        let v = self.from_wire_piece(&g, vw, &mut rb);
                        (self.pop(), v)
                    };
                    let acc = self.stack[acc_slot];
                    let new = self.guarded(|vm| vm.invoke_value(f, vec![acc, k, v], span))?;
                    self.stack[acc_slot] = new;
                }
                let acc = self.pop(); // unroot accumulator
                self.pop(); // unroot receiver
                Ok(acc)
            }
            _ => Err(self.err(format!("type RwShared has no method '{method}'"), span)),
        }
    }

    /// TICKET-154 (W11-15) -- the ONE-map snapshot behind `for_each`, `fold`, `for_each_entry` and
    /// `fold_entries`. Returns `None` when every depth-1 piece of the stored wire stands alone
    /// ([`Vm::wire_pieces_are_self_contained`]); the caller then keeps its per-element walk, which
    /// stays flat in memory. Otherwise the stored wire holds an ALIAS (a container or cell reached
    /// twice, or a cycle through the root), so it rebuilds the WHOLE wire once into ONE map and
    /// materializes every piece from that map into a fresh list (a `Map` flattens key then value per
    /// entry). Two elements that alias one node then land on ONE object, which is what `get()` and
    /// CPython's `copy.deepcopy` give.
    ///
    /// Two invariants, both load-bearing:
    /// 1. ONE read guard spans the decision AND every materialization. The looping arms re-acquire the
    ///    guard per element, so a map carried across two acquisitions would resolve an id against a
    ///    serialization a concurrent `set` had already replaced (the torn read of W7-4 round 2).
    /// 2. The result list is rooted on the operand stack across every piece, matching `slice`; it is
    ///    popped before return, so the CALLER must root it before anything allocates or collects.
    ///
    /// Not used by `at`/`get_key`: materializing a whole container to read one element is strictly
    /// worse, and two separate calls are two crossings (CPython: two separate deep copies).
    fn rwshared_snapshot_pieces(&mut self, core: &Arc<RwSharedCore>) -> Option<GcRef> {
        let g = core.v.read().unwrap();
        if !matches!(
            &*g,
            WireValue::List { .. } | WireValue::Set { .. } | WireValue::Map { .. }
        ) || Vm::wire_pieces_are_self_contained(&g)
        {
            return None;
        }
        let mut rb = super::fxhash::FxHashMap::<u32, GcRef>::default();
        let _whole = self.from_wire_memo((*g).clone(), &mut rb);
        let res_h = self.heap.alloc(Obj::List(Vec::new()));
        self.push(Value::obj(res_h));
        let n = match &*g {
            WireValue::List { items, .. } => items.len(),
            WireValue::Set { entries, .. } => entries.len(),
            WireValue::Map { entries, .. } => entries.len(),
            _ => unreachable!(),
        };
        for i in 0..n {
            let (first, second) = match &*g {
                WireValue::List { items, .. } => (items[i].clone(), None),
                WireValue::Set { entries, .. } => (entries[i].1.clone(), None),
                WireValue::Map { entries, .. } => {
                    (entries[i].1.clone(), Some(entries[i].2.clone()))
                }
                _ => unreachable!(),
            };
            let a = self.from_wire_piece(&g, first, &mut rb);
            let b = second.map(|w| self.from_wire_piece(&g, w, &mut rb));
            if let Obj::List(items) = self.heap.get_mut(res_h) {
                items.push(a);
                items.extend(b);
            }
        }
        self.pop();
        Some(res_h)
    }

    /// Element `i` of a [`rwshared_snapshot_pieces`](Vm::rwshared_snapshot_pieces) list, or `None`
    /// once `i` runs past it.
    fn snapshot_piece(&self, all: GcRef, i: usize) -> Option<Value> {
        match self.heap.get(all) {
            Obj::List(items) => items.get(i).copied(),
            _ => None,
        }
    }

    /// `Atomic[T]` methods: `load` (copy out), `store` (copy in), `exchange` (swap, returns old),
    /// `cas(expected, new) -> bool` (swap iff the box equals `expected`), `add`/`sub` (numeric RMW,
    /// returns the new value). Each is a single lock-op-unlock, so the RMW is atomic across threads —
    /// no user closure runs under the lock (unlike `Shared.update`), so no `update_lock` is needed.
    /// `add`/`sub` use the language's `checked_add`/`checked_sub`
    /// (int overflow faults, like the `+`/`-` operators) and plain float arithmetic.
    pub(super) fn atomic_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "load" => {
                self.arity_err("load", args, 0, span)?;
                let w = self.atomic_core(h).v.lock().unwrap().clone();
                Ok(self.from_wire(w))
            }
            "store" => {
                self.arity_err("store", args, 1, span)?;
                let w = self.to_wire_crossable(args[0], span)?;
                self.atomic_core(h).store(w);
                Ok(Value::nil())
            }
            "exchange" => {
                self.arity_err("exchange", args, 1, span)?;
                let new_w = self.to_wire_crossable(args[0], span)?;
                // Summarise BEFORE taking the value lock (the walk is O(payload)) — see
                // `SharedCore::store`.
                let sum = crate::vm::core::wire_summary(&new_w);
                let core = self.atomic_core(h);
                let old = {
                    let mut g = core.v.lock().unwrap();
                    core.store_guarded(&mut g, new_w, sum)
                };
                Ok(self.from_wire(old))
            }
            "cas" => {
                self.arity_err("cas", args, 2, span)?;
                let core = self.atomic_core(h);
                // Hold the value lock across compare+swap so the CAS is atomic. `from_wire`/`to_wire`/
                // `values_equal` borrow `self`, not the guard (which borrows the cloned `Arc`), so the
                // lock can stay held while they run.
                let mut g = core.v.lock().unwrap();
                let cur = self.from_wire(g.clone());
                // TICKET-144 / TICKET-177: a payload whose `==` is slot identity (a fn value, an
                // iterator) can never compare equal to a fresh `load()`, so `false` would spin the CAS
                // retry loop forever — Go's `atomic.Value.CompareAndSwap` panics on the same shape.
                // `Obj::identity` is the one classification; a handle payload compares by core and is
                // accepted. (`g` drops on the early return, leaving the box unchanged.)
                if let Some(what) = self.slot_identity_in(cur) {
                    return Err(self.err(
                        format!("Atomic.cas: the payload holds {what}, which cas cannot compare"),
                        span,
                    ));
                }
                // Propagate a cyclic-operand depth fault (`?`) instead of swallowing it — consistent
                // with `==` and every container membership site. The `?` runs BEFORE the store, so a
                // fault leaves the box unchanged (the lock guard `g` drops on the early return).
                //
                // This is the ONE equality site that stays STRUCTURAL: the compare runs under
                // `core.v.lock()` so the compare-and-swap is atomic, and a user `eq` re-entering the
                // VM here could touch the same `Atomic` and deadlock a non-reentrant mutex. The
                // checker rejects a payload type that REACHES a user `eq` (`reject_eq_atomic_payload`),
                // but that walk cannot see through a `Protocol` existential or an unresolved type
                // param — so the property is ENFORCED here, by turning the hook off for the window,
                // instead of being asserted from the checker's exhaustiveness. Cleared on the next
                // statement (no `?` in between), so an `Err` compare cannot leave it stuck on.
                // ponytail: eq-under-lock ceiling — if `Atomic[T]` ever admits a user-`eq` payload,
                // read under the lock → drop it → eq → re-acquire → verify the value is unchanged via
                // `wire_summary` → swap.
                self.eq_hook_off = true;
                let cmp = self.values_equal_guarded(cur, args[0], 0, span);
                self.eq_hook_off = false;
                let swapped = cmp?;
                if swapped {
                    // Reject a non-crossable store BEFORE the assignment — a failed store leaves the
                    // box unchanged (recoverable, no partial write). `ensure_crossable` borrows `&self`
                    // not the guard `g`, so it is safe to call under the value lock.
                    let next = self.to_wire_crossable(args[1], span)?;
                    let sum = crate::vm::core::wire_summary(&next);
                    core.store_guarded(&mut g, next, sum);
                }
                Ok(Value::bool(swapped))
            }
            "add" | "sub" => {
                self.arity_err(method, args, 1, span)?;
                // Uniformity: route through the guard like every other store site. The checker gates
                // `add`/`sub` to numeric deltas, so the handle-reject arm is dead-but-harmless here —
                // it just means a future non-numeric delta path can't forget the guard.
                let delta = self.to_wire_crossable(args[0], span)?;
                let core = self.atomic_core(h);
                let mut g = core.v.lock().unwrap();
                let new = match (&*g, &delta) {
                    (WireValue::Int(a), WireValue::Int(b)) => {
                        let (r, label) = if method == "add" {
                            (a.checked_add(*b), "Add")
                        } else {
                            (a.checked_sub(*b), "Sub")
                        };
                        WireValue::Int(r.ok_or_else(|| {
                            self.err(format!("integer overflow in {label}"), span)
                        })?)
                    }
                    (WireValue::Float(a), WireValue::Float(b)) => {
                        WireValue::Float(if method == "add" { a + b } else { a - b })
                    }
                    // The checker gates `add`/`sub` to numeric element types, so this is unreachable.
                    _ => {
                        return Err(self.err(format!("type Atomic has no method '{method}'"), span));
                    }
                };
                let sum = crate::vm::core::wire_summary(&new);
                core.store_guarded(&mut g, new.clone(), sum);
                drop(g);
                Ok(self.from_wire(new))
            }
            _ => Err(self.err(format!("type Atomic has no method '{method}'"), span)),
        }
    }

    /// `AtomicInt` methods: `load`, `store`, `exchange`, `cas(expected, new) -> bool`, `add`/`sub`
    /// (returns the NEW value). Backed by a raw lock-free `AtomicI64` — every op uses `SeqCst` ordering
    /// (matches the sequential consistency `Atomic`'s Mutex gave — every op still appears to happen
    /// in some single global order).
    /// `add`/`sub` KEEP the i64-overflow fault via a CHECKED `compare_exchange` CAS-loop (NOT raw
    /// `fetch_add`/`fetch_sub`, which wrap silently) — error string byte-identical to `atomic_method`'s.
    pub(super) fn atomic_int_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        use std::sync::atomic::Ordering::SeqCst;
        match method {
            "load" => {
                self.arity_err("load", args, 0, span)?;
                let cur = self.atomic_int_core(h).v.load(SeqCst);
                Ok(self.make_int(cur))
            }
            "store" => {
                self.arity_err("store", args, 1, span)?;
                let n = self.int_of(args[0]);
                self.atomic_int_core(h).v.store(n, SeqCst);
                Ok(Value::nil())
            }
            "exchange" => {
                self.arity_err("exchange", args, 1, span)?;
                let n = self.int_of(args[0]);
                let old = self.atomic_int_core(h).v.swap(n, SeqCst);
                Ok(self.make_int(old))
            }
            "cas" => {
                self.arity_err("cas", args, 2, span)?;
                let expected = self.int_of(args[0]);
                let new = self.int_of(args[1]);
                let swapped = self
                    .atomic_int_core(h)
                    .v
                    .compare_exchange(expected, new, SeqCst, SeqCst)
                    .is_ok();
                Ok(Value::bool(swapped))
            }
            "add" | "sub" => {
                self.arity_err(method, args, 1, span)?;
                let delta = self.int_of(args[0]);
                let core = self.atomic_int_core(h);
                let label = if method == "add" { "Add" } else { "Sub" };
                // Checked compare_exchange CAS-loop: raw fetch_add/fetch_sub wrap silently (behavior
                // regression vs Atomic's Mutex + checked_add). Retry on a racing writer.
                loop {
                    let cur = core.v.load(SeqCst);
                    let r = if method == "add" {
                        cur.checked_add(delta)
                    } else {
                        cur.checked_sub(delta)
                    };
                    let new =
                        r.ok_or_else(|| self.err(format!("integer overflow in {label}"), span))?;
                    if core.v.compare_exchange(cur, new, SeqCst, SeqCst).is_ok() {
                        return Ok(self.make_int(new));
                    }
                }
            }
            _ => Err(self.err(format!("type AtomicInt has no method '{method}'"), span)),
        }
    }

    /// `Executor` methods (C5/escape hatch): `submit` (enqueue a detached task closure, rejected once
    /// shut), `shutdown` (graceful — drain FIFO via the re-entrant call path), `shutdown_now` (discard
    /// pending). The executor handle is re-rooted on the operand stack across the drain, and each popped task is
    /// rooted across its nested call (the receiver was popped in `do_method_call`).
    pub(super) fn executor_method(
        &mut self,
        h: GcRef,
        method: &str,
        args: &[Value],
        span: Span,
    ) -> Result<Value, RuntimeError> {
        match method {
            "submit" => {
                self.arity_err("submit", args, 1, span)?;
                let core = self.executor_core(h);
                // Cheap early reject so a shut executor costs no wiring work. Re-checked below under
                // the lock — this one is advisory, the one that decides is the atomic one.
                if core.inner.lock().unwrap().shut {
                    return Err(self.err(
                        "submit on a shut-down Executor (it no longer accepts work)".to_string(),
                        span,
                    ));
                }
                // W7-39 follow-up — the inherited chain (`creator_cancel`, captured at
                // `Op::NewExecutor`) is STICKY: nothing ever resets it, matching Go's derived context
                // (a cancelled parent stays cancelled). So once the creating job's executor has been
                // `shutdown_now`-ed, every job this core dispatches starts already-cancelled and dies
                // at its first checkpoint. Silently: the handle crosses the airlock by `Arc`, so the
                // submitter may be `main` holding the only reference, and its own GRACEFUL
                // `shutdown()` — which promises to wait for its work — returned having run nothing.
                // Keep the stickiness, drop the silence: this is a `submit` the executor cannot
                // honour, exactly like a `submit` after `shutdown()`, so it faults the same way.
                //
                // Read-only after construction (no lock), and EMPTY for an executor created by `main`
                // or by a `parallel:`/`spawn` fiber — those are untouched. The core's OWN `cancel` is
                // deliberately NOT checked here: `shutdown_now` sets `shut` first, so the check above
                // already owns that case and adding it would double-report.
                if core
                    .creator_cancel
                    .iter()
                    .any(|c| c.load(std::sync::atomic::Ordering::Relaxed))
                {
                    return Err(self.err(
                        "submit on an Executor whose creating job was cancelled (it no longer \
                         accepts work)"
                            .to_string(),
                        span,
                    ));
                }
                // The task closure crosses the airlock **by value**
                // (`wire_callable` → `to_wire`: proto + deep-copied captures + home index), exactly
                // like plain `spawn` (`cross_spawn_callee`). Routing every submit through this SAME
                // wire path runs the generator airlock enforcement uniformly, and isolates captures at
                // submit time, for every submitted closure. (Before 2026-08-16 the since-removed
                // engine instead queued the callable's own `Handle` — captures shared by reference,
                // bypassing `to_wire` — to mirror the also-removed tree-walk `interp` oracle; both are
                // gone, so the by-handle preservation was pure divergence and was retired with them.)
                // A pool thread runs the closure the moment it is submitted — the queue never holds a
                // pending closure to drain later. Queued captures stay rooted via the
                // executor handle's `children()` (the `Closure` arm of `collect_core_gcrefs`) — which
                // on M:N now roots nothing, since nothing sits in the queue.
                let w = self.wire_callable(args[0], span)?;
                // EAGER (D1/D2): start the job NOW on the shared pool. The queue stays empty on
                // this engine — the work lives in the core's `eager` slots instead.
                //
                // Building the worker happens with **NO executor lock held**, deliberately.
                // `prepare_worker_from_wire` rebuilds the closure into the worker's heap, and a
                // closure that captured this executor puts an `Obj::Executor` over THIS core into
                // that heap — so a GC there marks the core, and `Heap`'s mark arm takes
                // `core.inner.lock()`. `std::sync::Mutex` is not reentrant, so holding it across
                // the rebuild would self-deadlock on `ex.submit(fn(): ex.…)` under GC pressure.
                // Faulting here (`ensure_snapshot` on a frame-holding generator global) must also
                // happen BEFORE a slot is reserved, or the reservation would leave `outstanding`
                // permanently short and hang `shutdown` forever.
                let rw = self.prepare_eager_job(&core, w, span)?;
                // W7-26r sibling — measure the worker heap this submit just built, so its bytes
                // are OWNED by this submitter while the job waits in the pool queue (see
                // `ExecutorCore::pending`). Under a live cap only: the walk is O(this worker's
                // slots), and with no cap there is nothing to compare it against.
                //
                // `own_bytes`, NOT `live_bytes`, and OUTSIDE the `inner` lock below — both were
                // adversarial-review findings, each a real failure: the full walk charges
                // `Arc`-SHARED core payloads the submitter already counts (60 jobs capturing one
                // 1 MB `Shared` reported 60 MB against a true 3.8 MB → false OVER-MEMORY), and it
                // re-takes `core.inner`, which a job capturing its own executor turned into a
                // self-deadlock (hang, rc=124) — precisely the hazard the comment above names.
                let pending = if self.heap.mem_cap() != 0 {
                    rw.worker.heap.own_bytes()
                } else {
                    0
                };
                // Now the atomic part: re-check `shut` and reserve the submission slot under ONE
                // lock, so a job racing an `ex.shutdown()` is either rejected or is counted by
                // that shutdown's join — never dispatched into a shut executor nobody waits for.
                // Lock order is inner → eager; a finishing job takes only `eager`, so it can never
                // contend here.
                let g = core.inner.lock().unwrap();
                if g.shut {
                    return Err(self.err(
                        "submit on a shut-down Executor (it no longer accepts work)".to_string(),
                        span,
                    ));
                }
                crate::vm::sched::dispatch_eager_job(
                    &core,
                    rw,
                    self.heap.mem_cap(),
                    pending,
                    &self.sched_registry,
                );
                drop(g);
                // W7-26 (the SAMPLING half) — charge the results this executor has ACCUMULATED
                // against this heap's GC pacing counter, so a live `--max-heap` actually gets
                // sampled. `wire_callable` above charges only what the submit itself sends; a
                // job that builds its own payload wires ~nothing, so the parent's loop would
                // never sweep and the cap would fail OPEN with the results counted but never
                // looked at. Gated on a live cap, exactly like `to_wire_crossable`'s charge:
                // cap-off pays one `!= 0` load and does not take the `eager` lock at all.
                if self.heap.mem_cap() != 0 {
                    let grown = core
                        .eager
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .take_charge();
                    // W7-26r sibling — and the queued job just handed off, for exactly the same
                    // reason: counting it in `live_bytes` is worthless if nothing looks. A loop
                    // submitting slow jobs finishes none of them, so `take_charge` stays 0 and
                    // the parent (which allocates ~nothing per submit) would never sweep —
                    // measured PASS at 666 MB against an 8 MB cap with the accounting alone.
                    self.heap.charge_bytes(grown + pending);
                }
                Ok(Value::nil())
            }
            "shutdown" => {
                self.arity_err("shutdown", args, 0, span)?;
                let core = self.executor_core(h);
                // Mark shut first so a task that re-enters this executor (submit/shutdown) sees it.
                core.inner.lock().unwrap().shut = true;
                // EAGER (D1/D4): every job started at its `submit`, so there is no queue to
                // drain — `shutdown` is purely the JOIN. Wait for in-flight work, then reduce the
                // submission-ordered slots: output flushes in submission order and the
                // lowest-index fault propagates, exactly as the drain did.
                //
                // The join itself registers this thread as a blocked party (`join_eager_jobs`),
                // which is what lets a job blocked on a channel only THIS caller could have filled
                // fault instead of hanging both of us forever. Nothing to arm here any more: the
                // verdict is process-wide, so it no longer matters WHICH join a thread is in.
                self.join_eager_jobs(&core, span)?;
                self.cancel_at_join(span)?; // TICKET-147 (W14-15): the join is a cancellation point
                Ok(Value::nil())
            }
            "shutdown_now" => {
                self.arity_err("shutdown_now", args, 0, span)?;
                let core = self.executor_core(h);
                {
                    let mut g = core.inner.lock().unwrap();
                    g.shut = true;
                    g.clear(); // always empty (eager submit never fills it); cleared defensively
                }
                // D4 — "attempts to stop", COOPERATIVE not preemptive: trip the per-core cancel
                // flag so a job already running dies at its next back-edge, and one the pool has
                // not started yet observes it in its prologue. A job with no cancellation point
                // still runs to completion; that is Java's contract too.
                crate::vm::trip_cancel_flag(&core.cancel);
                // TICKET-118 (W13-8) — a cancel is a wake source too: without this poke, a worker
                // deciding under its own core lock could read the flag on both sides of the trip and
                // park untimed, leaving a job's cancelled nursery child un-drained until some later
                // unrelated wake reached it.
                crate::vm::sched::poke_live_scheds(&self.sched_registry);
                // …then JOIN, exactly like `shutdown`. Java's `shutdownNow` returns without
                // waiting because it hands back the never-started tasks and you follow up with
                // `awaitTermination`; Chezzi has no such follow-up call, so not waiting here
                // would leave detached jobs running past a `shut` executor with the program
                // still exiting mid-job. (`drain_live_executors` no longer treats `shut` alone as
                // "already handled" — see `ExecutorCore::unreduced` — but that only closes the
                // self-join hand-off; it still does not WAIT for a job this call has not joined
                // itself, so skipping the join here would still be an exit-mid-job hazard.)
                // A cancelled job is swallowed by `reduce_task_slots` (its output still flushes at
                // its slot), except that its own `defer` fault is a real fault (TICKET-147, W14-12):
                // it raises here, ranked below any ordinary fault. So this raises a fault if a job
                // genuinely faulted, or a cancelled job's cleanup did.
                self.join_eager_jobs(&core, span)?;
                self.cancel_at_join(span)?; // TICKET-147 (W14-15): the join is a cancellation point
                Ok(Value::nil())
            }
            _ => Err(self.err(format!("type Executor has no method '{method}'"), span)),
        }
    }

    /// C5 / A2 — at a clean program end, gracefully
    /// drain every `Executor` created but never explicitly `shutdown`/`shutdown_now`-ed, in creation
    /// order, reusing the shipped `shutdown` path (FIFO, run-all — every queued job runs and the
    /// lowest-submission-index fault propagates, W7-5). A hard `std.os.exit` is not drained (the
    /// caller gates on `pending_exit`); a task that calls `os.exit` mid-drain stops the remaining
    /// drain.
    pub(super) fn drain_live_executors(&mut self) -> Result<(), RuntimeError> {
        self.drain_live_executors_from(0)
    }

    /// TICKET-195 — THE end of a run, for every driver: `r` is what the run returned. A clean
    /// finish drains the live executors and reports the drain's result. A run that ended in a
    /// deadlock verdict drains them too, so a finished job's buffered output still reaches the sink
    /// (`run_file`'s in-process sink holds it only in the job's slot); the drain's own result is
    /// discarded, and the verdict and its trace stand. Any other fault, and a pending exit, skip the
    /// drain as before.
    pub(crate) fn finish_run(&mut self, r: Result<(), RuntimeError>) -> Result<(), RuntimeError> {
        match self.rank_end(0, r) {
            Ok(()) => self.drain_live_executors(),
            Err(e) if e.is_deadlock && self.pending_exit.is_none() => {
                let trace = (self.fault_trace.take(), self.fault_trace_depth);
                let _ = self.drain_live_executors();
                (self.fault_trace, self.fault_trace_depth) = trace;
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    /// Like [`Vm::drain_live_executors`], but skips the first `from` registry entries. `from` is a
    /// registry index taken earlier (e.g. [`Vm::exec_registry_mark`]) and is safe to reuse across
    /// this whole drain: `exec_registry`'s one mutation is an append (`push`), so indices never shift.
    /// This is what lets a per-test drain join only the executors ITS test created, leaving one built
    /// at module top level or in `before_all` alive for the tests that follow.
    pub(super) fn drain_live_executors_from(&mut self, from: usize) -> Result<(), RuntimeError> {
        if self.pending_exit.is_some() {
            return Ok(());
        }
        // EAGER (D1) — the executor is DETACHED: its work is already running, and this is where
        // the program waits for it. Walk the heap-independent registry, not `self.executors`:
        // that list is heap-keyed, so an executor created inside a task never reached it and its
        // work was silently lost (W7-5b). Creation order.
        //
        // Re-scan from the top each round rather than snapshotting: a job joined here can itself
        // construct and submit to a NEW executor, which must also be joined.
        //
        // `shut` alone is NOT "already handled", and reading it as such lost work: a job that shuts
        // down the executor it runs under marks it `shut` while reducing NOTHING, so with no
        // enclosing `shutdown()` this drain used to skip the core and drop every sibling's buffered
        // output and every sibling's fault. `ExecutorCore::unreduced` is that job's hand-off — see
        // its doc for why it is a flag and not "the slot vector is non-empty".
        //
        // Termination is the same one-way step as before: a picked core is marked `shut` AND joined,
        // and a join on this thread always has `slack == 0` (a top-level/`main` `Vm` is never one of
        // the core's eager jobs — see the caller table on `join_eager_jobs`), so it ends in
        // `take_slots`, which clears `unreduced`. Each core is therefore picked at most twice: once
        // while live, once to collect what a self-join left behind.
        loop {
            let next = self
                .exec_registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .skip(from)
                .find(|c| {
                    !c.inner.lock().unwrap_or_else(|e| e.into_inner()).shut
                        || c.unreduced.load(std::sync::atomic::Ordering::Acquire)
                })
                .map(Arc::clone);
            let Some(core) = next else { break };
            core.inner.lock().unwrap_or_else(|e| e.into_inner()).shut = true;
            // TICKET-048 — the program-exit drain has no call site of its own, so it names where
            // this Executor was created.
            self.join_eager_jobs(&core, core.created_at)?;
            if self.pending_exit.is_some() {
                break; // a joined job called os.exit — hard halt, stop joining
            }
        }
        Ok(())
    }
}

/// TICKET-192 — one single-entry write of a `RwShared[Map[K, V]]` (see
/// [`Vm::rwshared_map_write_entry`]).
#[derive(Debug, Clone, Copy)]
pub(super) enum EntryWrite {
    Put(Value, Value),
    Remove(Value),
    GetOrInsert(Value, Value),
}
