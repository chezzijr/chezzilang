//! B3.1 — the shared cores for `Channel` / `Shared` / `Executor`, lifted OUT of the GC heap.
//!
//! Before B3.1 a `Channel`'s queue (etc.) lived *inside* a heap [`Obj`](super::heap::Obj) and held
//! [`Value`](super::value::Value)s (i.e. `GcRef`s into that one heap), so it could never be shared
//! across threads. B3.1 moves the data into an `Arc<…Core>` that lives outside every heap and holds
//! [`WireValue`](super::wire::WireValue) (the serialized airlock form). The heap keeps only a
//! *handle* — `Obj::Channel(Arc<ChannelCore>)` — and two handles (e.g. one per task) can point at the
//! same core. This is the structural move that lets B3.3 share a core across real OS threads; at B3.1
//! everything is still single-thread and cooperative, so the `Mutex` never actually contends.
//!
//! A `Condvar` (for real blocking `recv`) was added at B3.3; a `closed` flag (for `Channel.close()`)
//! lives alongside the queue under [`ChannelCore::q`]'s lock (see [`ChanState`]). Cooperative `recv`
//! parks the *fiber* (it does not block on a primitive), so the condvar is dead on that engine.

use super::value::GcRef;
use super::wire::{WireGenState, WireValue};
use crate::lexer::Span;
use std::collections::{HashMap, VecDeque};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};
use std::time::Duration;

/// TICKET-016 (W8-3) — the process-global wait-for graph behind every `Shared`/`RwShared` update
/// guard. `owner` maps a box's stable identity (`Arc::as_ptr(..) as usize`) to the task token
/// currently holding its guard; `waiting` maps a blocked task's token to the key it wants. A guard
/// acquire walks `waiting`/`owner` alternately from the requester: a walk that returns to the
/// requester is a wait-for cycle (self-held is the length-1 case), which faults instead of blocking.
#[derive(Default)]
struct GuardGraph {
    owner: HashMap<usize, u64>,
    waiting: HashMap<u64, usize>,
}

fn guard_registry() -> &'static (Mutex<GuardGraph>, Condvar) {
    static REGISTRY: OnceLock<(Mutex<GuardGraph>, Condvar)> = OnceLock::new();
    REGISTRY.get_or_init(|| (Mutex::new(GuardGraph::default()), Condvar::new()))
}

/// Why a guard acquire refused to block: a wait-for cycle exists. `SelfHeld` is the length-1 case
/// (the requester already owns `key`, e.g. a nested `set`/`update`/`write` on the same box inside its
/// own closure); `Cycle` is a longer cross-task/cross-box cycle (e.g. AB-BA).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardCycle {
    SelfHeld,
    Cycle,
}

/// RAII handle for a held update guard (TICKET-016 / W8-3): dropping it releases the box's guard and
/// wakes every task waiting on the registry, so a cycle formed after registration is still found.
pub struct UpdateGuard {
    key: usize,
}

impl Drop for UpdateGuard {
    fn drop(&mut self) {
        release_update_guard(self.key);
    }
}

/// TICKET-016 — `GUARD_DEMOTE_BUDGET` is how long a guard acquire waits IN PLACE on its worker before
/// paying for a replacement OS thread. `Vm::block_enter` spawns one OS thread per demoting worker
/// shell, so an unconditional demote on every guarded `set`/`update`/`write` made 50 000 one-`update`
/// fibers exhaust a 32 768-task ceiling and never finish. Measured budgets on that test
/// (`TasksMax=32768`): 0 ms still peaks at 1091 threads, 1 ms at 35, 5 ms at 19.
pub const GUARD_DEMOTE_BUDGET: Duration = Duration::from_millis(5);

/// Take the update guard for the `Shared`/`RwShared` box identified by `key`, as task `me`. Blocks
/// while the box is held by a healthy other task; returns `Err` immediately when the wait-for walk
/// from `me` finds a cycle (never blocks into a cycle it can already see).
pub fn acquire_update_guard(key: usize, me: u64) -> Result<UpdateGuard, GuardCycle> {
    match acquire_update_guard_within(key, me, None) {
        Ok(Some(g)) => Ok(g),
        Ok(None) => unreachable!("an unbounded update-guard acquire cannot time out"),
        Err(cycle) => Err(cycle),
    }
}

/// Bounded/unbounded acquire of the update guard for `key`, as task `me`. With `budget: None` this
/// blocks exactly like [`acquire_update_guard`]. With `budget: Some(d)`, once `d` has elapsed without
/// acquiring the guard it returns `Ok(None)` instead of continuing to block, so the caller can pay for
/// a worker demotion only when the wait is actually long.
pub fn acquire_update_guard_within(
    key: usize,
    me: u64,
    budget: Option<Duration>,
) -> Result<Option<UpdateGuard>, GuardCycle> {
    match wait_update_guard(key, me, budget, true)? {
        GuardWait::Taken(g) => Ok(Some(g)),
        GuardWait::TimedOut => Ok(None),
        GuardWait::Free => unreachable!("a taking wait never reports Free"),
    }
}

/// TICKET-193 — what an acquire of `key` by task `me` would do right now. The ONE decider: the wait
/// loop ([`wait_update_guard`]) and the deadlock verdict ([`guard_wait_satisfiable`]) both read it.
/// The cycle walk starts at `waiting[me]`, so a caller that wants the cycle answer registers first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GuardState {
    Free,
    SelfHeld,
    Cycle,
    Busy,
}

fn guard_state(g: &GuardGraph, key: usize, me: u64) -> GuardState {
    match g.owner.get(&key) {
        None => GuardState::Free,
        Some(&o) if o == me => GuardState::SelfHeld,
        Some(_) if wait_for_cycle(g, me) => GuardState::Cycle,
        Some(_) => GuardState::Busy,
    }
}

/// How a [`wait_update_guard`] ended without a cycle.
enum GuardWait {
    Taken(UpdateGuard),
    Free,
    TimedOut,
}

/// The one guard wait loop. `take` = take the guard once free; else report `Free` and leave it.
/// Clears `waiting[me]` on every return (DEC-016).
fn wait_update_guard(
    key: usize,
    me: u64,
    budget: Option<Duration>,
    take: bool,
) -> Result<GuardWait, GuardCycle> {
    let deadline = budget.map(|d| std::time::Instant::now() + d);
    let (mtx, cv) = guard_registry();
    let mut g = mtx.lock().unwrap();
    loop {
        g.waiting.insert(me, key);
        let state = guard_state(&g, key, me);
        if state != GuardState::Busy {
            g.waiting.remove(&me);
        }
        let left = match state {
            GuardState::Free if take => {
                g.owner.insert(key, me);
                return Ok(GuardWait::Taken(UpdateGuard { key }));
            }
            GuardState::Free => return Ok(GuardWait::Free),
            GuardState::SelfHeld => return Err(GuardCycle::SelfHeld),
            GuardState::Cycle => return Err(GuardCycle::Cycle),
            GuardState::Busy => match deadline {
                None => Duration::from_millis(50),
                Some(d) => match d.checked_duration_since(std::time::Instant::now()) {
                    None => {
                        g.waiting.remove(&me);
                        return Ok(GuardWait::TimedOut);
                    }
                    Some(l) => l.min(Duration::from_millis(50)),
                },
            },
        };
        g = cv.wait_timeout(g, left).unwrap().0;
    }
}

/// TICKET-193 — wait until `key` is free WITHOUT taking it: `Ok(true)` free, `Ok(false)` budget spent,
/// `Err` a cycle. `Vm::guard_wait_block` waits here holding no width permit, then takes the permit,
/// then the guard (permit before guard; DEC-141).
pub fn await_update_guard_free(
    key: usize,
    me: u64,
    budget: Option<Duration>,
) -> Result<bool, GuardCycle> {
    match wait_update_guard(key, me, budget, false)? {
        GuardWait::Free => Ok(true),
        GuardWait::TimedOut => Ok(false),
        GuardWait::Taken(_) => unreachable!("a non-taking wait never takes the guard"),
    }
}

/// TICKET-063 — would an acquire of `key` by `me`, issued now, NOT block? Reads [`guard_state`].
///
/// The cycle arm answers `false` between polls, not `true`: `acquire_update_guard_within` removes
/// `me` from `waiting` before it returns `Ok(None)` (see its deadline arm above), so once a bounded
/// acquire has given up, `wait_for_cycle` finds no entry for `me` and this fn reports "not
/// satisfiable" even on a real cycle. That is the safe direction — the nursery deadlock judge then
/// reports `deadlock` where a `GuardCycle` fault was one poll away, never the reverse.
pub fn guard_wait_satisfiable(key: usize, me: u64) -> bool {
    let (mtx, _cv) = guard_registry();
    let g = mtx.lock().unwrap();
    guard_state(&g, key, me) != GuardState::Busy
}

/// Walk `waiting` -> `owner` -> `waiting` -> ... from `start`, bounded by the number of owned boxes
/// plus one hop: a walk that revisits `start` is a wait-for cycle.
fn wait_for_cycle(g: &GuardGraph, start: u64) -> bool {
    let mut cur = start;
    let max_hops = g.owner.len() + 1;
    for _ in 0..max_hops {
        let Some(&want) = g.waiting.get(&cur) else {
            return false;
        };
        let Some(&owner) = g.owner.get(&want) else {
            return false;
        };
        if owner == start {
            return true;
        }
        cur = owner;
    }
    false
}

/// Release the update guard held on `key` and wake every waiter, so a cycle that formed after this
/// registration (or the newly-freed key) is re-checked.
pub fn release_update_guard(key: usize) {
    let (mtx, cv) = guard_registry();
    let mut g = mtx.lock().unwrap();
    g.owner.remove(&key);
    drop(g);
    cv.notify_all();
}

/// `Channel[T]` core (B3.1): the shared mailbox, a FIFO of wire-form messages. `send` locks +
/// `push_back`; `recv`/`try_recv` lock + `pop_front`; `len` locks + len. `cap` is `None` for an
/// unbounded `Channel[T]()` (the default — `send` never blocks) and `Some(n)` for a bounded
/// `Channel[T](n)`: once `n` messages are queued a `send` BLOCKS/parks until a `recv` frees a slot
/// (backpressure), and `try_send` returns `false`. A freed slot wakes parked senders exactly as a
/// `send` wakes parked receivers (the not-full waiter set mirrors the not-empty one).
///
/// B3.3-threads: `cv` is the real-OS-thread blocking primitive. A `recv` on an empty queue waits on
/// `cv` (paired with `q`'s `Mutex`); a `send` `notify_all`s it after pushing. An ordinary M:N `recv`
/// snapshot-parks the *fiber* instead and never touches `cv`. `cv` IS used by every party that
/// blocks in place on its own thread: a party with no scheduler at all (top-level `main`, an eager
/// `Executor` job — [`Vm::block_recv`]), and **D5 owe #3 Path C**, where a `recv` reached inside an
/// M:N native callback can't snapshot-park, so the worker thread DEMOTES — it blocks in place on
/// `cv` and resumes when a sibling `send` `notify_all`s it (`MnSched::send_wake` + the non-mn
/// `send`). The wait loop re-checks the queue / cancel / terminate on every wake
/// (spurious-wakeup-safe; bounded poll).
#[derive(Debug, Default)]
pub struct ChannelCore {
    pub q: Mutex<ChanState>,
    pub cv: Condvar,
    /// Bounded-channel capacity: `None` = unbounded (`send` never blocks); `Some(0)` = rendezvous
    /// (TICKET-028) — a send moves only into a parked receiver's slot (TICKET-185,
    /// [`ChanState::send`]), because a cap-0 channel has no buffer; `Some(n>0)` = a bounded FIFO whose `send` parks
    /// the fiber once `n` messages are queued and whose `try_send` returns `false` when full.
    /// Immutable after construction (set once by `Op::NewChannel`).
    pub cap: Option<usize>,
    /// `timer(ms)` timeout channel: `Some(deadline)` iff this channel was built by `timer`. It is
    /// **level-triggered** — `recv` yields `true` on any call at/after the deadline (the typical use
    /// recvs it once, in a `wait` arm). Delivery is handled at `recv` time in the receiver's own
    /// scheduler ([`Vm::chan_recv_step`]): an M:N worker outside a native callback
    /// (`mn.is_some() && native_reentry == 0`) schedules a background `send(true)` + parks; otherwise
    /// (`mn.is_none()` — the top-level VM, the inline outermost-`parallel:` builder VM, or an eager
    /// `Executor` job's `Vm` — or inside a native callback, `native_reentry > 0`) it inline-sleeps to
    /// the deadline and synthesises `true`.
    /// `None` for an ordinary `Channel[T]`.
    pub timer: Option<std::time::Instant>,
    /// `wait`-arm timed-park latch: set once (CAS false→true) when a `--parallel` `wait` arms the
    /// background `send_wake(true)` for this timer channel, so a re-park of the SAME wait (woken with
    /// no consumable value, e.g. a sibling `close` on another arm) does NOT arm a redundant second
    /// job. A fresh `timer(ms)` builds a fresh core (`armed=false`), so no reset is needed; a reused
    /// timer handle is still served by its single job (it wakes whatever token sits in this bucket at
    /// the deadline). Only the snapshot-park path arms a job; the single-`recv` and demote paths don't.
    pub timer_armed: AtomicBool,
    /// `trip()` manual level-trigger latch (the primitive behind `std.cancel`'s `done()`). Once set
    /// true it is permanent: `recv`/`try_recv`/`wait` report ready (`true`) on every call thereafter,
    /// for any number of receivers — exactly like a passed `timer` deadline, but flipped on demand
    /// instead of by the clock. `false` for an ordinary `Channel[T]`. A `trip()` reuses `close()`'s
    /// wake fan-out (minus the `closed` flag) so a parked `recv`/`wait` re-runs and observes it.
    pub done_latch: AtomicBool,
    /// TICKET-205 — gated threads waiting in place on this channel; a wake queues each for the permit.
    pub(super) gated: Mutex<Vec<GatedWaiter>>,
}

/// TICKET-205 — one gated in-place waiter of a channel: its permit slot, and the channel whose
/// condvar it sleeps on when that is not this one (a `wait:` sleeps on its first arm).
pub(super) type GatedWaiter = (Arc<super::width::Slot>, Option<Arc<ChannelCore>>);

/// The locked interior of a [`ChannelCore`]: the message FIFO plus a `closed` flag. Folding `closed`
/// into the *same* mutex as the queue is deliberate — every park decision ([`super::Vm::park`],
/// `send_wake`, the recv arm, the demote loop) re-checks the queue under this lock, so "a value is
/// waiting OR the channel is closed" is one atomic observation. A separate `AtomicBool` would leave a
/// TOCTOU gap (check empty, then close happens, then park) that could strand a parked fiber. Once
/// `closed`: `send`/`try_send` are rejected, `recv` drains then faults, and `for v in ch:` ends once
/// drained. `close()` wakes every parked/demoted receiver via `cv` + the scheduler.
/// TICKET-185 — a blocked party's commit record (Go's `sudog` + `selectDone`). One `Pending` per
/// blocked `send`, `recv` or `wait:`; a `wait:` shares ONE across all its arms, so the CAS that
/// takes a value also decides which arm fired. Every commit is a CAS from [`PENDING_QUEUED`] made
/// under the channel lock `core.q` of the entry it commits ([`ChanState::give`] fills a slot,
/// [`ChanState::pop`] takes an offer). Nothing else decides "delivered".
#[derive(Debug, Default)]
pub struct Pending(AtomicU32);

/// Nobody has committed this party yet: its offers may be taken and its slots filled.
pub const PENDING_QUEUED: u32 = 0;
/// The party withdrew (it re-polls, or it unwound); none of its entries can commit any more.
pub const PENDING_CANCELLED: u32 = 1;
/// `close()` closed one of this party's offers: its `send` faults `send on a closed channel`.
pub const PENDING_CLOSED: u32 = 2;
/// `PENDING_DONE + arm`: the value moved on that arm (an offer was taken or a slot was filled).
pub const PENDING_DONE: u32 = 3;

impl Pending {
    pub fn new() -> Arc<Pending> {
        Arc::new(Pending(AtomicU32::new(PENDING_QUEUED)))
    }

    pub fn state(&self) -> u32 {
        self.0.load(Ordering::Acquire)
    }

    pub fn is_queued(&self) -> bool {
        self.state() == PENDING_QUEUED
    }

    /// Commit `arm`: the one CAS every hand-off goes through.
    pub fn try_commit(&self, arm: u32) -> bool {
        self.0
            .compare_exchange(
                PENDING_QUEUED,
                PENDING_DONE + arm,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// Withdraw; `Err(state)` when somebody settled the party first.
    pub fn try_cancel(&self) -> Result<(), u32> {
        self.0
            .compare_exchange(
                PENDING_QUEUED,
                PENDING_CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ())
    }

    pub fn try_close(&self) -> bool {
        self.0
            .compare_exchange(
                PENDING_QUEUED,
                PENDING_CLOSED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

fn is_me(p: &Arc<Pending>, me: Option<&Arc<Pending>>) -> bool {
    me.is_some_and(|m| Arc::ptr_eq(m, p))
}

/// A blocked sender's value, published so a receiver can take it (TICKET-185). Lives in
/// `ChanState::sendq`, never in the buffer, so it never counts toward `len()` or `cap`.
#[derive(Debug)]
struct Offer {
    p: Arc<Pending>,
    arm: u32,
    sum: usize,
    w: WireValue,
}

/// A blocked rendezvous receiver's place (TICKET-185): a sender that finds it commits it and
/// stores the value in `got`, where the receiver takes it on its next run.
#[derive(Debug)]
struct Slot {
    p: Arc<Pending>,
    arm: u32,
    got: Option<(usize, WireValue)>,
}

/// TICKET-194 — what a `recv` on a channel gets WITHOUT waiting ([`ChannelCore::recv_ready`]).
pub enum RecvReady {
    /// A buffered value or a live sender's offer, taken.
    Value(WireValue),
    /// A tripped `done()` latch or a fired `timer(ms)`: the receive yields `true`.
    Fired,
    /// Closed and drained.
    Closed,
    /// Nothing is ready: the receive would wait (an unfired timer included).
    Wait,
}

impl ChannelCore {
    /// TICKET-205 — THE wake of every thread waiting in place on this channel: the waker queues each
    /// gated waiter for the runner permit before it notifies, so the hand-over order is the waker's.
    pub(super) fn wake_all(&self) {
        for (s, sleeps_on) in self.gated.lock().unwrap_or_else(|e| e.into_inner()).iter() {
            super::width::reserve(s);
            if let Some(other) = sleeps_on {
                other.cv.notify_all();
            }
        }
        self.cv.notify_all();
    }

    /// TICKET-194 — THE ready decision of a channel receive, made in the caller's `core.q` hold `q`:
    /// a value > a tripped latch > a fired timer > closed.
    pub fn recv_ready(&self, q: &mut ChanState) -> RecvReady {
        if let Some(w) = q.pop() {
            return RecvReady::Value(w);
        }
        if self.done_latch.load(Ordering::Relaxed)
            || self.timer.is_some_and(|d| std::time::Instant::now() >= d)
        {
            return RecvReady::Fired;
        }
        if q.closed {
            return RecvReady::Closed;
        }
        RecvReady::Wait
    }
}

/// What one `send` attempt did ([`ChanState::send`]).
pub enum SendOutcome {
    /// The value moved: into the buffer, or into a receiver's slot.
    Sent,
    /// The value waits in `sendq` as the caller's offer.
    Offered,
    /// Nothing can take the value now and the caller published no offer.
    Full,
    Closed,
}

#[derive(Debug, Default)]
pub struct ChanState {
    /// PRIVATE on purpose (W6-7/W6-10): every mutation must go through the methods below so the
    /// cached GC summary (`bytes`/`dirty`) can never go stale. A stale `dirty == false` would stop
    /// the GC tracing a live handle queued in this channel — a use-after-free. Rust module privacy
    /// is what makes "did I catch every push site?" a compile error instead of a code review.
    ///
    /// Each message carries its own [`wire_summary`] byte count so `pop` is O(1): these queues are
    /// popped under the GLOBAL `MnSched` lock (`sched.rs` demote paths), and re-deriving the count
    /// on removal would put an O(payload) walk in that critical section.
    queue: VecDeque<(usize, WireValue)>,
    /// TICKET-185 — offers of blocked senders (a cap-0 channel, or a FULL bounded one).
    sendq: VecDeque<Offer>,
    /// TICKET-185 — slots of blocked cap-0 receivers.
    recvq: VecDeque<Slot>,
    /// Approximate owned bytes of every value held here (buffer, offers, filled slots — see
    /// [`wire_summary`]) — the off-heap storage `Heap::live_bytes` could not see before W6-10.
    bytes: usize,
    /// True while ANY held value can root a heap object (a `Handle` or a nested core). Cleared
    /// only when all three queues empty, so it is conservative (over-walk = safe) and self-healing.
    dirty: bool,
    pub closed: bool,
}

impl ChanState {
    fn add(&mut self, sum: (usize, bool)) {
        self.bytes += sum.0;
        self.dirty |= sum.1;
    }

    fn sub(&mut self, b: usize) {
        self.bytes = self.bytes.saturating_sub(b);
        if self.queue.is_empty() && self.sendq.is_empty() && self.recvq.is_empty() {
            self.bytes = 0;
            self.dirty = false;
        }
    }

    /// Buffer a message with its PRE-COMPUTED [`wire_summary`].
    ///
    /// The summary MUST be computed by the caller **before taking any lock**: `send_commit` holds
    /// `MnSched::core` — the process-wide lock that serializes every fiber's park/wake/finish —
    /// across this call, and `wire_summary` is O(payload). Global-lock hold time must not scale
    /// with user payload size.
    pub fn push(&mut self, sum: (usize, bool), w: WireValue) {
        self.add(sum);
        self.queue.push_back((sum.0, w));
    }

    /// Publish a blocked sender's value as `p`'s offer on `arm`.
    pub fn offer(&mut self, p: &Arc<Pending>, arm: u32, sum: (usize, bool), w: WireValue) {
        self.add(sum);
        self.sendq.push_back(Offer {
            p: Arc::clone(p),
            arm,
            sum: sum.0,
            w,
        });
    }

    /// Publish a blocked rendezvous receiver's slot for `p` on `arm`.
    pub fn slot(&mut self, p: &Arc<Pending>, arm: u32) {
        self.recvq.push_back(Slot {
            p: Arc::clone(p),
            arm,
            got: None,
        });
    }

    /// Hand `w` to the first live, unfilled slot that is not `me`'s, committing it. Dead slots are
    /// dropped on the way. `Err(w)` when no such slot exists.
    pub fn give(
        &mut self,
        sum: (usize, bool),
        w: WireValue,
        me: Option<&Arc<Pending>>,
    ) -> Result<(), WireValue> {
        let mut i = 0;
        while i < self.recvq.len() {
            let s = &self.recvq[i];
            if s.got.is_some() || is_me(&s.p, me) {
                i += 1;
                continue;
            }
            if s.p.try_commit(s.arm) {
                self.recvq[i].got = Some((sum.0, w));
                self.add(sum);
                return Ok(());
            }
            self.recvq.remove(i);
        }
        Err(w)
    }

    /// THE send decision (TICKET-185), made in one `core.q` hold. In order: closed; unbounded
    /// buffers; rendezvous gives to a slot; bounded buffers while there is room; otherwise the
    /// value becomes `offer`'s offer, or the send is [`SendOutcome::Full`].
    pub fn send(
        &mut self,
        cap: Option<usize>,
        sum: (usize, bool),
        w: WireValue,
        offer: Option<(&Arc<Pending>, u32)>,
    ) -> SendOutcome {
        if self.closed {
            return SendOutcome::Closed;
        }
        let w = match cap {
            None => {
                self.push(sum, w);
                return SendOutcome::Sent;
            }
            Some(0) => match self.give(sum, w, offer.map(|(p, _)| p)) {
                Ok(()) => return SendOutcome::Sent,
                Err(w) => w,
            },
            Some(n) if self.queue.len() < n => {
                self.push(sum, w);
                return SendOutcome::Sent;
            }
            Some(_) => w,
        };
        match offer {
            Some((p, arm)) => {
                self.offer(p, arm, sum, w);
                SendOutcome::Offered
            }
            None => SendOutcome::Full,
        }
    }

    /// Take the next value: the buffer front (then move the first live offer into the freed
    /// place — Go's recv refill), else the first live offer that is not `me`'s. Each offer taken
    /// is committed by CAS; dead offers are dropped on the way.
    pub fn pop_for(&mut self, me: Option<&Arc<Pending>>) -> Option<WireValue> {
        if let Some((b, w)) = self.queue.pop_front() {
            self.bytes = self.bytes.saturating_sub(b);
            if let Some((sum, ow)) = self.take_offer(me) {
                self.queue.push_back((sum, ow));
            }
            self.sub(0);
            return Some(w);
        }
        let (sum, w) = self.take_offer(me)?;
        self.sub(sum);
        Some(w)
    }

    /// Commit and remove the first live offer that is not `me`'s; dead offers are dropped.
    fn take_offer(&mut self, me: Option<&Arc<Pending>>) -> Option<(usize, WireValue)> {
        let mut i = 0;
        while i < self.sendq.len() {
            if is_me(&self.sendq[i].p, me) {
                i += 1;
                continue;
            }
            let o = self.sendq.remove(i).unwrap();
            if o.p.try_commit(o.arm) {
                return Some((o.sum, o.w));
            }
            self.bytes = self.bytes.saturating_sub(o.sum);
        }
        None
    }

    pub fn pop(&mut self) -> Option<WireValue> {
        self.pop_for(None)
    }

    /// Remove `p`'s filled slot and return its value.
    pub fn take(&mut self, p: &Arc<Pending>) -> Option<WireValue> {
        let i = self
            .recvq
            .iter()
            .position(|s| s.got.is_some() && Arc::ptr_eq(&s.p, p))?;
        let (b, w) = self.recvq.remove(i)?.got?;
        self.sub(b);
        Some(w)
    }

    /// Remove every offer and slot of `p`.
    pub fn withdraw(&mut self, p: &Arc<Pending>) {
        let mut freed = 0;
        self.sendq.retain(|o| {
            let mine = Arc::ptr_eq(&o.p, p);
            if mine {
                freed += o.sum;
            }
            !mine
        });
        self.recvq.retain(|s| {
            let mine = Arc::ptr_eq(&s.p, p);
            if mine && let Some((b, _)) = &s.got {
                freed += b;
            }
            !mine
        });
        self.sub(freed);
    }

    /// `close()` (TICKET-185): in this one hold, set `closed`, close every offer (each such sender
    /// faults `send on a closed channel`) and drop the unfilled slots. A filled slot stays: its
    /// value was delivered before the close.
    pub fn close(&mut self) {
        self.closed = true;
        let mut freed = 0;
        for o in self.sendq.drain(..) {
            o.p.try_close();
            freed += o.sum;
        }
        self.recvq.retain(|s| s.got.is_some());
        self.sub(freed);
    }

    pub fn clear(&mut self) {
        self.queue.clear();
        self.sendq.clear();
        self.recvq.clear();
        self.bytes = 0;
        self.dirty = false;
    }

    /// The buffered message count — Go's `len`: offers and slots never count.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// The buffer is empty — like [`len`](Self::len), offers and slots never count.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Every value held here — buffer, offers and filled slots — for the GC trace.
    pub fn iter(&self) -> impl Iterator<Item = &WireValue> {
        self.queue
            .iter()
            .map(|(_, w)| w)
            .chain(self.sendq.iter().map(|o| &o.w))
            .chain(
                self.recvq
                    .iter()
                    .filter_map(|s| s.got.as_ref().map(|(_, w)| w)),
            )
    }

    /// Cached GC summary of the held values: `(approximate owned bytes, can-root-a-heap-object)`.
    pub fn summary(&self) -> (usize, bool) {
        (self.bytes, self.dirty)
    }

    /// Would a `recv` by `me` take a value now? A buffered value, or a live offer of another party.
    pub fn recv_ready_for(&self, me: Option<&Arc<Pending>>) -> bool {
        !self.queue.is_empty()
            || self
                .sendq
                .iter()
                .any(|o| o.p.is_queued() && !is_me(&o.p, me))
    }

    /// Would a `send` by `me` move its value now? Unbounded: always. Rendezvous: a live unfilled
    /// slot of another party. Bounded: room in the buffer.
    pub fn send_ready_for(&self, cap: Option<usize>, me: Option<&Arc<Pending>>) -> bool {
        match cap {
            None => true,
            Some(0) => self
                .recvq
                .iter()
                .any(|s| s.got.is_none() && s.p.is_queued() && !is_me(&s.p, me)),
            Some(n) => self.queue.len() < n,
        }
    }
}

/// What a blocked party's [`PendingOp`] settled to.
#[derive(Debug)]
pub enum Settled {
    /// Nobody committed it; every entry is withdrawn and the party polls again.
    Cancelled,
    /// `close()` closed one of its offers.
    Closed,
    /// Its offer on this arm was taken.
    Sent(u32),
    /// Its slot on this arm was filled with this value.
    Got(u32, WireValue),
}

/// TICKET-185 — a blocked party's [`Pending`] and every channel it has entries on, as
/// `(core, arm, is_send)`. Held by the running `Vm` or its parked `Fiber` until
/// [`settle`](Self::settle). Dropping it settles it, so a fault that unwinds never leaves a live
/// offer or slot behind. Never drop one while holding a `core.q` guard: settling takes that lock.
#[derive(Debug)]
pub struct PendingOp {
    pub p: Arc<Pending>,
    pub at: Vec<(Arc<ChannelCore>, u32, bool)>,
}

impl PendingOp {
    pub fn new(p: Arc<Pending>, at: Vec<(Arc<ChannelCore>, u32, bool)>) -> Self {
        PendingOp { p, at }
    }

    /// Cancel if still queued, take a filled slot's value, and withdraw every remaining entry.
    pub fn settle(mut self) -> Settled {
        self.settle_mut()
    }

    fn settle_mut(&mut self) -> Settled {
        let at = std::mem::take(&mut self.at);
        let r = match self.p.try_cancel() {
            Ok(()) | Err(PENDING_CANCELLED) => Settled::Cancelled,
            Err(PENDING_CLOSED) => Settled::Closed,
            Err(s) => {
                let arm = s - PENDING_DONE;
                match at.iter().find(|(_, a, _)| *a == arm) {
                    Some((_, _, true)) => Settled::Sent(arm),
                    Some((core, _, false)) => {
                        let w = core
                            .q
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .take(&self.p);
                        match w {
                            Some(w) => Settled::Got(arm, w),
                            None => Settled::Cancelled,
                        }
                    }
                    None => Settled::Cancelled,
                }
            }
        };
        for (core, _, _) in &at {
            core.q
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .withdraw(&self.p);
        }
        r
    }
}

impl Drop for PendingOp {
    fn drop(&mut self) {
        if !self.at.is_empty() {
            let _ = self.settle_mut();
        }
    }
}

/// `Shared[T]` core (B3.1): the one box every task reaches. `get` locks + clones out; `set` and
/// `update` each take the box's process-global update guard (TICKET-016 / W8-3,
/// [`super::core::acquire_update_guard`]) before touching `v`.
///
/// B3.3-threads: `update`'s read-modify-write must be **atomic across threads** — that is the entire
/// promise of `Shared[T]` ("the single owner serialises writes, so the torn-write race is
/// unrepresentable"), and `set` must not be able to silently clobber a write an in-flight `update`'s
/// closure made — so `set` takes the SAME guard `update` does. The value lock `v` cannot be held
/// across the user closure (it would deadlock a closure that re-enters `get` on the same box —
/// `Mutex` is not reentrant), so the update guard — not `v` — is what serialises the whole RMW: held
/// for the entire operation, while `v` is still locked only for the brief read and the brief
/// write-back. A same-box `set`/`update` re-entry inside `update`'s own closure is the guard
/// registry's length-1 wait-for cycle and FAULTS; a cross-task contender BLOCKS until the guard
/// frees; a genuine cross-box wait-for cycle (AB-BA) FAULTS instead of hanging.
#[derive(Debug, Default)]
pub struct SharedCore {
    pub v: Mutex<WireValue>,
    /// W6-7/W6-10 — cached GC summary of `v`. MUST be re-`set` under `v`'s lock by every store.
    pub summary: WireSummary,
}

impl SharedCore {
    /// Replace the payload AND refresh the cached GC summary **under the same lock** (W6-7/W6-10).
    /// Every write path (`set`, `update`'s write-back) must go through here: a stale `WS_CLEAN`
    /// would stop the GC tracing a handle stored into this box — a use-after-free.
    ///
    /// The O(payload) [`wire_summary`] walk runs BEFORE the lock is taken (`w` is caller-owned at
    /// that point, so the result is exact); only the two atomic stores + the move happen inside.
    /// Same rule as [`ChanState::push`]: lock hold time must not scale with user payload size —
    /// here it is a *reader* stall, since `RwShared`'s whole contract is many concurrent readers.
    pub fn store(&self, w: WireValue) {
        let sum = wire_summary(&w);
        let mut g = self.v.lock().unwrap();
        self.summary.store(sum.0, sum.1);
        *g = w;
    }
}

/// `RwShared[T]` core: the read-write counterpart to [`SharedCore`]. The value lives behind a
/// `RwLock` instead of a `Mutex`, so MANY concurrent `read` guards (or ONE exclusive `write` guard)
/// can be held at once — the point of the type is that read-heavy workloads scale. `read(f)` takes a
/// SHARED read guard, clones the value out, drops the guard, then runs `f` (no write-back) — `read`
/// never takes the update guard. `write(f)` and `set` each take the box's process-global update guard
/// (TICKET-016 / W8-3, [`super::core::acquire_update_guard`]) before touching `v`.
///
/// `write`'s read-modify-write must be **atomic across threads** (the box's contract, exactly like
/// `Shared.update`), and `set` must not be able to silently clobber an in-flight `write`'s closure —
/// so `set` takes the SAME guard `write` does. The value lock `v` cannot be held across the user
/// closure (a `RwLock` write guard is not reentrant — it would deadlock a closure that re-enters
/// `get`/`read` on the same box), so the update guard — not `v` — serialises the whole RMW: held for
/// the entire operation, while `v` is taken only for the brief read-out and the brief write-back. A
/// same-box `set`/`write` re-entry inside `write`'s own closure FAULTS (the guard registry's
/// length-1 wait-for cycle); `write` nested in `read` still persists (`read` never takes the guard); a
/// genuine cross-box wait-for cycle FAULTS instead of hanging.
#[derive(Debug, Default)]
pub struct RwSharedCore {
    pub v: RwLock<WireValue>,
    /// W6-7/W6-10 — cached GC summary of `v`. MUST be re-`set` under `v`'s write lock by every store.
    pub summary: WireSummary,
    /// TICKET-192 — write generation of `v`, bumped under `v`'s write lock by every mutation. A probe
    /// that drops the read guard to run a user `eq` re-reads it under the next guard and restarts when
    /// it moved, so a concurrent remove that shifts positions cannot make a present key read as
    /// missing. It lives on the core, not the table: a whole `set` replaces the table, and a counter
    /// restarting with it could return to an old value.
    pub generation: AtomicU64,
}

impl RwSharedCore {
    /// Replace the payload AND refresh the cached GC summary under the same write lock — see
    /// [`SharedCore::store`] (the walk is hoisted OFF the exclusive lock for the same reason).
    pub fn store(&self, w: WireValue) {
        let sum = wire_summary(&w);
        let mut g = self.v.write().unwrap();
        self.summary.store(sum.0, sum.1);
        self.generation.fetch_add(1, Ordering::Relaxed);
        *g = w;
    }

    /// TICKET-192 — run `f` on the stored map under the write lock, bumping the write generation
    /// (see [`generation`](Self::generation)). `None` when the stored value is not a `Map`. `f` must
    /// keep `summary` in step itself ([`WireSummary::adjust`]); it runs under the lock, so it may.
    pub fn with_map_mut<R>(&self, f: impl FnOnce(&mut super::wire::WireMap) -> R) -> Option<R> {
        let mut g = self.v.write().unwrap();
        match &mut *g {
            WireValue::Map { entries, .. } => {
                self.generation.fetch_add(1, Ordering::Relaxed);
                Some(f(entries))
            }
            _ => None,
        }
    }
}

/// `Atomic[T]` core: the cross-task atomic box. Like [`SharedCore`] (one boxed wire value behind a
/// `Mutex`, reachable across threads via the `Arc` handle), but presents atomic-operation methods —
/// `load`/`store`/`exchange`/`cas` and (numeric `T`) `add`/`sub`. Each method is a single
/// lock-op-unlock, so the read-modify-write of `add`/`sub`/`exchange`/`cas` is atomic across threads
/// without a separate `update_lock` (no user closure runs under the lock, unlike `Shared.update`).
#[derive(Debug, Default)]
pub struct AtomicCore {
    pub v: Mutex<WireValue>,
    /// W6-7/W6-10 — cached GC summary of `v`. MUST be re-`set` under `v`'s lock by every store.
    pub summary: WireSummary,
}

impl AtomicCore {
    /// Replace the payload AND refresh the cached GC summary under the same lock — see
    /// [`SharedCore::store`] (the walk is hoisted OFF the lock for the same reason).
    pub fn store(&self, w: WireValue) {
        let sum = wire_summary(&w);
        let mut g = self.v.lock().unwrap();
        self.summary.store(sum.0, sum.1);
        *g = w;
    }

    /// Replace the payload through an ALREADY-held guard (the `exchange` / `cas` / `add`|`sub`
    /// read-modify-write paths, which must not drop the lock between compare and swap), returning
    /// the previous value. Refreshes the summary in the same critical section.
    ///
    /// `sum` is the caller's PRE-COMPUTED [`wire_summary`] of `w` — passed in, not derived here, so
    /// the O(payload) walk can sit OUTSIDE the lock wherever the new value is known before it is
    /// taken (`exchange`). The two RMW paths that genuinely build their value under the lock
    /// (`cas`'s `to_wire` is already O(payload) under it; `add`/`sub` are scalars) compute it inline.
    pub fn store_guarded(&self, g: &mut WireValue, w: WireValue, sum: (usize, bool)) -> WireValue {
        self.summary.store(sum.0, sum.1);
        std::mem::replace(g, w)
    }
}

/// `AtomicInt` core: the monomorphic, LOCK-FREE int atomic (Rust `AtomicI64` / Java `AtomicInteger` /
/// Go `atomic.Int64` style). Unlike [`AtomicCore`] (a `Mutex<WireValue>` holding an arbitrary sendable
/// value), the value is statically int, so it can be a raw `std::sync::atomic::AtomicI64` — no lock, no
/// runtime type-sniffing, no wider-T hole. `SeqCst` on every op preserves the sequential consistency the
/// Mutex gave — every op still appears to happen in some single global order. `add`/`sub` use a
/// CHECKED compare_exchange CAS-loop
/// (not raw `fetch_add`/`fetch_sub`, which wrap silently) to KEEP the i64-overflow fault.
#[derive(Debug, Default)]
pub struct AtomicIntCore {
    pub v: std::sync::atomic::AtomicI64,
}

/// D6 — a monotonic, process-wide poll key. The netpoller (`super::poller`) keys an fd registration
/// by an arbitrary `usize` we choose, NOT the raw fd: a closed-then-reopened fd reuses its integer,
/// which would alias a stale registration (an ABA hazard); a fresh key per socket avoids that. It is
/// also the registry key the poller files a parked fiber under. `0` is reserved (never a real key).
static NEXT_POLL_KEY: AtomicUsize = AtomicUsize::new(1);

/// Allocate the next unique poll key (see [`NEXT_POLL_KEY`]).
pub fn next_poll_key() -> usize {
    NEXT_POLL_KEY.fetch_add(1, Ordering::Relaxed)
}

/// D6 — `Socket` core: a non-blocking connected TCP stream, the shared half of an `Obj::Socket`
/// handle (structurally like [`ChannelCore`] — an `Arc`'d core outside every heap, so two fibers can
/// alias one fd). `Option` so `close()` can take + drop the stream (closing the fd) while aliasing
/// handles observe `None` — a use-after-close is then a clean fault, never a dangling-fd panic. The
/// `std::net::TcpStream` is the RAII fd owner: the last `Arc<SocketCore>` drop closes the fd
/// automatically (no manual `Drop` needed). `key` is the stable poll-registration identity.
#[derive(Debug)]
pub struct SocketCore {
    pub stream: Mutex<Option<TcpStream>>,
    pub key: usize,
    /// D6 — `true` exactly while a would-block op on this socket sits parked in the netpoller. Set by
    /// `park_on_fd` before parking, cleared by the poller when it injects the fiber back (or on
    /// `deregister`). Because oneshot epoll/kqueue allows ONE registration per fd, a SECOND fiber that
    /// shares this socket (`Arc`) and reaches a would-block op while the first is parked is rejected
    /// with a clean fault — without it the duplicate `Poller::add` would `EEXIST`-panic the poll thread
    /// and the duplicate registry insert would drop the first fiber (an `inflight` leak + hang). Shared
    /// (`Arc`) so the poller can clear it without holding the type-erased core.
    pub in_flight: Arc<AtomicBool>,
    /// W15-1 — set once by `close()` before it deregisters; `poller::register` refuses a park while it
    /// is set, so a would-block op racing `close` never arms a closed or reused fd.
    pub closed: Arc<AtomicBool>,
    /// B1 — the incomplete-UTF-8 tail (≤3 bytes) of the previous `read`: a multibyte codepoint that
    /// straddled the `read(n)` chunk boundary. `Socket.read -> Result[str]` is a str-only seam, so a
    /// chunk that ends mid-codepoint is NOT decodable on its own — the tail is retained HERE and
    /// prepended to the next read (never lossily decoded, never dropped). It lives on the `Arc`'d core,
    /// not on the frame, because a would-block park REWINDS `ip` and re-executes the whole read op (see
    /// [`Vm::park_on_fd`]) and because two fibers may alias one socket.
    ///
    /// LOCK ORDER — `carry` is the OUTER lock: a reader takes `carry`, then `stream`, does the fd read,
    /// updates the carry, and drops both. The fd read and the carry update MUST be one critical section:
    /// with two fibers aliasing one socket, splitting them lets fiber B take the continuation bytes off
    /// the fd and decode them BEFORE fiber A stores the lead byte it took — valid text then errors as
    /// "invalid utf-8" and A's carry poisons the next read. Nothing may take `stream` then `carry`.
    pub carry: Mutex<Vec<u8>>,
}

/// D6 — `Listener` core: a non-blocking accepting socket. Same handle/core split + fd-lifecycle as
/// [`SocketCore`]; `accept` on it (when ready) yields a fresh `SocketCore`.
#[derive(Debug)]
pub struct ListenerCore {
    pub listener: Mutex<Option<TcpListener>>,
    pub key: usize,
    /// D6 — see [`SocketCore::in_flight`].
    pub in_flight: Arc<AtomicBool>,
    /// W15-1 — see [`SocketCore::closed`].
    pub closed: Arc<AtomicBool>,
}

/// D6 — a fresh, not-yet-parked in-flight flag for a new socket/listener core.
pub fn new_in_flight() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// W15-1 — a fresh, not-yet-closed flag for a new socket/listener core.
pub fn new_closed() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// R2b — `Reader` core: a read-only file handle, the shared half of an `Obj::Reader` — the input twin
/// of [`WriterCore`]. Same handle/core split as [`SocketCore`]: an `Arc`'d `BufReader<File>` outside
/// every heap, so a `spawn`ed fiber can alias the handle (cross-task read ORDERING against one shared
/// fd is unspecified — two tasks race the file offset, Go's `bufio`-not-goroutine-safe rule — but each
/// read is one Mutex critical section). `Option` so `close()` can take + drop the reader (closing the
/// fd) while aliasing handles observe `None` — a use-after-close is then a clean fault, never a panic.
/// `key` is a stable identity (like `SocketCore.key`); NO netpoller registration + NO `Drop` (reads are
/// flush-free — the fd closes on the `BufReader` drop). File-only (stdin is a separate shared source).
#[derive(Debug)]
pub struct ReaderCore {
    pub inner: Mutex<Option<std::io::BufReader<std::fs::File>>>,
    pub key: usize,
    /// W7-9 — the RAW bytes of a line `read_line` pulled off the fd but could not decode as UTF-8
    /// (terminator INCLUDED). `read_line -> Option[str]` is a str-only seam, so an undecodable line
    /// is not returnable — but it was already taken off the `BufReader`, and dropping it is the
    /// silent data loss B1/R1 exist to kill (the fault's own message recommends `read_bytes`, which
    /// used to hand back the NEXT line). Retained HERE so `read_bytes` gives them back byte-exactly,
    /// exactly like [`SocketCore::carry`]. Consequences, both deliberate:
    ///   * STICKY — while the carry is non-empty `read_line` re-decodes it and re-faults instead of
    ///     advancing; skipping would be the same loss one call later.
    ///   * SELF-HEALING — once a partial `read_bytes` drains the invalid prefix, the remaining carry
    ///     decodes and is returned as the line.
    ///
    /// An IO error mid-line carries too ([`ReaderCarry::io_err`]) — `read_until` leaves everything it
    /// read before the error in the buffer, and those bytes are already off the `BufReader`. That
    /// carry is NOT self-healing: an interrupted line is a TRUNCATED one, and handing it back as a
    /// whole line would trade the old silent loss for a silent lie. It re-faults until drained.
    ///
    /// `close()` discards it (closed is closed), and every read arm checks `inner.is_none()` BEFORE
    /// serving the carry, so it can neither leak past close nor resurrect after EOF.
    ///
    /// LOCK ORDER — `carry` is the OUTER lock, same rule as [`SocketCore::carry`]: take `carry`,
    /// then `inner`, do the fd read AND the carry update in ONE critical section, drop both. Two
    /// fibers may alias one `Reader`; splitting the two would let B take bytes off the fd before A
    /// stores the line it refused. Nothing may take `inner` then `carry`.
    ///
    /// A `VecDeque`, NOT a `Vec`, unlike [`SocketCore::carry`]: that one is bounded (<= 3 bytes off
    /// the happy path, one `MAX_SOCKET_READ` chunk at worst), this one is bounded only by the
    /// distance to the next `\n`, i.e. the whole file. A `Vec` front-drain memmoves the remainder on
    /// every call, so the chunked `read_bytes` recovery the fault message prescribes would be
    /// O(n^2) in the refused line (measured pre-fix: 64 MB -> 19.5s). Deque front-drain is O(taken).
    pub carry: Mutex<ReaderCarry>,
}

/// The [`ReaderCore::carry`] payload: the retained bytes plus, when they came from a failed READ
/// rather than a failed DECODE, the IO error that produced them.
#[derive(Debug, Default)]
pub struct ReaderCarry {
    pub bytes: std::collections::VecDeque<u8>,
    /// `Some(msg)` = these bytes are the truncated head of a line the fd failed to finish. `read_line`
    /// re-raises `msg` while it is set instead of decoding the bytes into a line that was never whole;
    /// `read_bytes` still hands them back, and clears this once the carry is empty.
    pub io_err: Option<String>,
}

impl ReaderCarry {
    /// Drop the carry AND its capacity. A refused line is bounded only by the file, so leaving a
    /// drained deque's buffer allocated pins that many bytes for the `Reader`'s whole lifetime —
    /// invisible to `Heap::live_bytes` and to `--max-heap`, since these bytes are off-heap.
    pub fn reset(&mut self) {
        self.bytes = std::collections::VecDeque::new();
        self.io_err = None;
    }
}

/// R2 — `Writer` core: a write-only file/stream handle, the shared half of an `Obj::Writer`. Same
/// handle/core split as [`SocketCore`] — an `Arc`'d core outside every heap, so a `spawn`ed fiber can
/// alias one handle. `Option` so `close()` can take + drop the backing (flushing + closing an fd)
/// while aliasing handles observe `None` — a use-after-close is then a clean fault, never a panic.
/// `key` is a stable identity (like `SocketCore.key`); there is NO netpoller registration (regular
/// files are always epoll-ready, so file writes are synchronous blocking syscalls — no park).
#[derive(Debug)]
pub struct WriterCore {
    pub inner: Mutex<Option<Backing>>,
    pub key: usize,
}

/// R2 — where a [`WriterCore`] sends bytes.
/// * `File` — a `create`/`append` file writer. The `BufWriter` gives OS-level write buffering for free
///   and **flushes on drop**, so an unclosed file writer never silently loses data.
/// * `Stdout`/`Stderr` — markers: a write ROUTES through [`Vm::emit_out`]/[`Vm::emit_err`] (the
///   captured `Vm.out` buffer / the streaming-CLI sink), NEVER a raw fd — else capture/streaming break.
/// * `Buffered` — the Go `bufio.NewWriter` escape hatch: accumulate in `buf`, drain to `inner` on
///   flush / buffer-full / close. A file-backed tail — `inner=File` **or** a nested `inner=Buffered`
///   chain that bottoms out in one — is best-effort drained on drop by [`WriterCore`]'s `Drop`; a
///   `Buffered{ inner=Stdout/Stderr }` tail CANNOT reach `&mut Vm` from `Drop`, so it is lost on drop
///   (documented ceiling — needs an explicit `flush()`/`close()`).
#[derive(Debug)]
pub enum Backing {
    File(std::io::BufWriter<std::fs::File>),
    Stdout,
    Stderr,
    Buffered {
        inner: Arc<WriterCore>,
        buf: Vec<u8>,
        cap: usize,
    },
}

impl Drop for WriterCore {
    /// R2 — best-effort drop-flush for a `Buffered` tail (the extra `Vec<u8>` std's `BufWriter` drop
    /// can't see). All four inner backings are handled: `File` → write+flush it; `Buffered` → append to
    /// the inner's own buffer, whose `Drop` cascades it one level further down (a nested
    /// `buffered(buffered(file))` chain is still file-backed, so it owes the same drop-flush);
    /// `Stdout`/`Stderr` → dropped silently, it can't reach `&mut Vm` from here (`emit_out` needs it).
    /// Must NEVER panic (a failed flush at GC/exit — ENOSPC — would abort the process): every error is
    /// swallowed.
    fn drop(&mut self) {
        let Ok(mut guard) = self.inner.lock() else {
            return;
        };
        if let Some(Backing::Buffered { inner, buf, .. }) = guard.as_mut() {
            if buf.is_empty() {
                return;
            }
            let drained = std::mem::take(buf);
            if let Ok(mut ig) = inner.inner.lock() {
                match ig.as_mut() {
                    Some(Backing::File(bw)) => {
                        use std::io::Write;
                        let _ = bw.write_all(&drained).and_then(|()| bw.flush());
                    }
                    Some(Backing::Buffered { buf: ibuf, .. }) => ibuf.extend_from_slice(&drained),
                    Some(Backing::Stdout) | Some(Backing::Stderr) | None => {}
                }
            }
        }
    }
}

/// The mutable inside of an [`ExecutorCore`]: the pending-task FIFO + the shut flag, behind one lock
/// (one `Mutex` for both, so `submit`/`shutdown` see a consistent view and to avoid a `Mutex<bool>`).
#[derive(Debug, Default)]
pub struct ExecState {
    /// PRIVATE on purpose — see [`ChanState::queue`].
    queue: VecDeque<(usize, WireValue)>,
    bytes: usize,
    dirty: bool,
    pub shut: bool,
}

impl ExecState {
    /// The eager (M:N) engine dispatches at `submit` rather than filling this queue (decision D3),
    /// so it is permanently empty — but `len`/`iter`/`summary`/`clear` below are still live: the
    /// `Executor` `Display` impl, the GC live-bytes walk and rooting pass, and `shutdown_now` all
    /// read or reset it unconditionally rather than special-case an executor that could — in a build
    /// with a queueing decision — actually hold work. `push`/`pop`/`take_all` had no such reader left
    /// once the queue-at-submit path was removed and are deleted; `is_empty` is kept as a trivial
    /// wrapper purely for `clippy::len_without_is_empty` — it has no caller of its own.
    pub fn clear(&mut self) {
        self.queue.clear();
        self.bytes = 0;
        self.dirty = false;
    }

    pub fn len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &WireValue> {
        self.queue.iter().map(|(_, w)| w)
    }

    /// Cached GC summary of the queued tasks: `(approximate owned bytes, can-root-a-heap-object)`.
    pub fn summary(&self) -> (usize, bool) {
        (self.bytes, self.dirty)
    }
}

/// W7-26 — one finished job's `(owned bytes, holds a nested core)`, the [`wire_summary`] of a
/// [`TaskOutcome`](super::TaskOutcome). Every variant owns two buffered-output `Vec<u8>`s (W7-5c
/// flushes them at the slot's task-order position, so they are retained until `shutdown`); only
/// `Done` also owns a return value.
///
/// Charged UNCONDITIONALLY, unlike the `mem_cap != 0` gates on `Vm::to_wire_crossable`'s pacing
/// charge and `live_bytes`'s nested-core recursion. Those fire per-store / per-sweep; this fires
/// once per finished job, beside a thread handoff and a condvar notify, right after that job's own
/// `O(payload)` `to_wire` — so gating it would buy nothing and would make `live_bytes` mean two
/// different things depending on a flag (both ancestors keep accounting live and the *limit*
/// separate: Go's `runtime.MemStats` vs `GOMEMLIMIT`). [`ChanState::push`] charges unconditionally
/// for the same reason.
///
/// Called OFF the `eager` lock (see [`the Executor's sched::finish`]) — the walk is O(result).
pub(super) fn outcome_summary(o: &super::TaskOutcome) -> (usize, bool) {
    use super::TaskOutcome as T;
    let (out, stderr, value) = match o {
        T::Done(r) => (&r.out, &r.stderr, Some(&r.value)),
        T::Cancelled { out, stderr }
        | T::CancelledFault { out, stderr, .. }
        | T::Exit { out, stderr, .. }
        | T::Fault { out, stderr, .. }
        | T::Deadlocked { out, stderr, .. } => (out, stderr, None),
    };
    let mut acc = (
        std::mem::size_of::<super::TaskOutcome>() + out.capacity() + stderr.capacity(),
        false,
    );
    if let Some(w) = value {
        // The "no `Heap::children` eager arm" claim is an INVARIANT, not luck: a job's return value
        // crossed via `to_wire_crossable`, whose `ensure_crossable` rejects a `Handle`. Fenced here
        // rather than merely reasoned about in a comment — if this ever fires, the eager half can
        // root a parent-heap object and `children` needs the arm `live_bytes` just gained.
        debug_assert!(
            !w.has_handle(),
            "an eager job result carries a parent-heap Handle — Heap::children needs an eager arm"
        );
        let (b, d) = wire_summary(w);
        acc.0 += b;
        acc.1 |= d;
    }
    acc
}

/// W7-26r — the `--max-heap` verdict a finished task's own thread reaches when the RETAINED backlog
/// of its join (an `Executor`'s eager slots, a nursery scope's task slots) has by itself grown past
/// the whole cap. Returns the outcome to store plus whether the cap tripped (the caller then trips
/// its scope/core cancel so siblings stop feeding the backlog).
///
/// **Why the producer decides, and not the joining parent.** `over_cap` is assigned only in
/// `Heap::sweep()`, which runs only at the parent fiber's own instruction boundary — and a parent
/// blocked inside `Executor.shutdown()`'s join or a `parallel:` join reaches none. Measured on the
/// release binary against an 8 MB cap: 300 jobs each printing ~1 MB PASSED at **622 MB** (executor)
/// and **733 MB** (nursery). Both owning ancestors put the observation on the ALLOCATOR, never on
/// the blocked consumer (measured 2026-08-06): CPython's `ThreadPoolExecutor` under a 300 MB
/// `RLIMIT_AS` raised `MemoryError` **in the worker at job 57/500** while `main` sat in
/// `ex.shutdown()`; Go 1.26 under `GOMEMLIMIT=32MiB` ran **7 GC cycles while `main` was blocked** in
/// `wg.Wait()`. So does this: the thread that produced the bytes is the one that looks.
///
/// **It cannot false-positive.** The trip needs the retained backlog ALONE to exceed the entire cap,
/// and those bytes provably exist — they are held in the slot vector until the join reduces it.
/// Nothing here estimates, samples a heap mid-native-call, or sweeps where values are unrooted
/// (which is what rules out the alternative of polling `live_bytes()` from inside the join: it
/// counts not-yet-swept garbage and would fault healthy programs).
///
/// Only `Done`/`Cancelled` are replaced: an `Exit` or an existing `Fault` already halts the join
/// with equal-or-higher precedence in [`Vm::reduce_task_slots`](super::Vm::reduce_task_slots), and
/// demoting one would lose an `os.exit` or a real fault. The replacement KEEPS the task's buffered
/// output (it flushes at its task-order slot like any fault's, W7-5c) and is the same size, so the
/// caller's already-computed [`outcome_summary`] stays accurate.
pub(super) fn halt_over_backlog(
    outcome: super::TaskOutcome,
    backlog: usize,
    cap: usize,
) -> (super::TaskOutcome, bool) {
    use super::TaskOutcome as T;
    if cap == 0 || backlog <= cap {
        return (outcome, false);
    }
    let err = super::RuntimeError {
        message: format!("test exceeded --max-heap ({cap} bytes)"),
        span: super::Span::default(),
        is_assert: false,
        // The marker is the whole point: it makes this a hard halt `recover:` cannot catch and buckets
        // the run `OVER-MEMORY`, exactly like the parent-side abort in `Vm::run_until`.
        is_over_memory: true,
        is_timed_out: false,
        is_deadlock: false,
        is_panic: false,
    };
    match outcome {
        T::Done(r) => (
            T::Fault {
                err,
                out: r.out,
                stderr: r.stderr,
                trace: Vec::new(),
            },
            true,
        ),
        T::Cancelled { out, stderr } => (
            T::Fault {
                err,
                out,
                stderr,
                trace: Vec::new(),
            },
            true,
        ),
        other => (other, false),
    }
}

/// `Executor` core (B3.1 / C5 escape hatch): the explicitly-owned work queue. `submit` runs EAGERLY
/// (the job goes straight to the pool, matching Python's `ThreadPoolExecutor` / Java's
/// `ExecutorService`) and [`ExecState::queue`] stays empty — the pending work lives in `eager`.
/// `shut` lives in the **shared** core, so any handle aliasing this core sees the same shutdown state
/// (this is what prevents a `from_wire`'d alias from being drained twice at program exit).
#[derive(Default)]
pub struct ExecutorCore {
    /// At most this many jobs run at once; zero means no cap; set once at construction.
    pub limit: usize,
    pub inner: Mutex<ExecState>,
    /// TICKET-208 — the detached scope this Executor's jobs run in: a sched built at the first
    /// `submit` and taken by the join that reduces its slots. The Executor's tasks, their results
    /// and their byte charge live in that sched and nowhere else.
    pub(super) scope: Mutex<Option<super::EagerScope>>,
    /// The cooperative cancel flag shared by every job this executor has dispatched. Per-CORE, not
    /// per-drain (the pre-eager model had no running jobs to cancel): `shutdown_now` trips it so
    /// already-started jobs die at their next back-edge (decision D4 — "attempts to stop",
    /// cooperative, not preemptive), and a hard halt inside a job trips it via `run_outcome`.
    pub cancel: Arc<AtomicBool>,
    /// The cancel-flag chain of the job that CREATED this executor (`Vm::scope_ancestors()` captured at
    /// `Op::NewExecutor`), empty for an executor created by `main` or by a `parallel:`/`spawn` fiber.
    /// Every job dispatched from this core inherits it as its `cancel_outer`, so an outer
    /// `shutdown_now()` reaches a nested executor's jobs (W7-39, `docs/concurrency.md` §Executor).
    ///
    /// **Keyed on the CREATOR, never on the submitter.** An `Executor` value crosses the airlock by
    /// `Arc`, so `submit` can be reached from a job of an entirely unrelated executor; keying on the
    /// submitter made *that* executor's `shutdown_now()` kill a job belonging to `main`'s executor, and
    /// `main`'s own graceful `shutdown()` then returned with the work silently dropped.
    ///
    /// Set once at construction and read-only afterwards — the core crosses threads by `Arc`, so a
    /// plain `Vec` (no lock) is only sound because nothing ever writes it again.
    pub creator_cancel: Vec<Arc<AtomicBool>>,
    /// TICKET-048 — the source span of the `Op::NewExecutor` that built this core. The program-exit
    /// drain (`Vm::drain_live_executors`) joins this executor from no source position of its own, so
    /// its fault names the `Executor()` call instead of claiming a false
    /// `line 1, col 1`.
    pub created_at: Span,
}

impl ExecutorCore {
    /// Jobs submitted and not yet finished. With [`held_bytes`](Self::held_bytes), the one facade
    /// over what an Executor holds outside every heap; every production reader goes through it.
    pub fn outstanding(&self) -> usize {
        self.sched().map_or(0, |s| s.outstanding_tasks())
    }

    /// This Executor's sched, when a `submit` has built it and no join has reduced it. Clones the
    /// `Arc` out and drops the `scope` lock before it returns.
    pub(super) fn sched(&self) -> Option<Arc<super::MnSched>> {
        self.scope
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|s| Arc::clone(&s.sched))
    }

    /// Test seam: attach `sched` as this Executor's scope, as `new_detached_sched` does.
    #[cfg(test)]
    pub(super) fn attach_test_sched(&self, sched: Arc<super::MnSched>) {
        *self.scope.lock().unwrap() = Some(super::EagerScope {
            sched,
            cancel: Arc::clone(&self.cancel),
            drainer: None,
            drainer_slot: None,
            scope: 0,
            fiber_owned: false,
            more_scopes: Vec::new(),
        });
    }

    /// Bytes this Executor holds outside every heap: its finished jobs' retained output plus the
    /// submit-time bytes of its unfinished jobs. Both `--max-heap` walks read this one number.
    pub fn held_bytes(&self) -> usize {
        self.sched().map_or(0, |s| s.held_bytes())
    }
}

impl std::fmt::Debug for ExecutorCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorCore").finish_non_exhaustive()
    }
}

/// Every `ExecutorCore` created during one run, in creation order — the list the program-exit join
/// walks (decision D1: an executor is detached, and the program waits for its work at exit).
///
/// **Why this exists alongside `Vm.executors`** (W7-5b). `Vm.executors` is a `Vec<GcRef>`: heap-keyed,
/// so it is swapped per fiber with its heap (`swap_ctx`) and an executor created INSIDE an M:N task
/// lands in that task's throwaway worker list, which is dropped when the task finishes. The top-level
/// join therefore never saw it, and its work was silently lost. An `ExecutorCore` lives OUTSIDE every
/// heap (B3.1), so a list of `Arc`s is heap-independent by construction: `spawn_worker` hands the SAME
/// list to every worker, and an executor created anywhere in the run is visible to the one join.
/// That is why closing W7-5b needs no change to `swap_ctx`'s heap-only gate — the change that a
/// previous attempt stopped at because it drags in GC rooting for a parked parent ctx.
///
/// Strong `Arc`s, not `Weak`: the whole point is to join work whose creating heap is already gone, so
/// the core must outlive its `Obj::Executor` handle. Entries are never pruned — same shape (and same
/// bound: one small struct per `Executor` the program constructs) as `Vm.executors`, whose
/// "reap only those alive at exit" snapshot has always been push-only too.
pub type ExecRegistry = Arc<Mutex<Vec<Arc<ExecutorCore>>>>;

/// A core's `Arc` pointer identity, for the mark walk's visited-core set. `PartialEq`/`Hash` are
/// hand-written rather than derived so the `#[cfg(test)]` probe counter in each has exactly one place
/// to live (TICKET-049) — see [`SeenCores`].
#[derive(Clone, Copy, Eq, Debug)]
pub struct CoreId(pub usize);

impl PartialEq for CoreId {
    fn eq(&self, other: &Self) -> bool {
        // TICKET-049 — charging the probe HERE, in the key type, means every dedup implementation
        // (`contains`, `iter().any()`, a hand-written loop) is charged alike; a counter bumped only
        // inside `SeenCores::insert` would stay green through a revert to a linear scan.
        #[cfg(test)]
        CORE_PROBES.with(|c| c.set(c.get() + 1));
        self.0 == other.0
    }
}

impl std::hash::Hash for CoreId {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        // TICKET-049 — see the comment on `eq`: charging here too means a hash-set dedup is charged
        // once per insert attempt, matching a linear scan's once-per-comparison charge.
        #[cfg(test)]
        CORE_PROBES.with(|c| c.set(c.get() + 1));
        self.0.hash(h);
    }
}

/// The mark walk's visited-core set. The ONE membership operation (`insert`) lives at this single
/// site so a linear-scan revert — deliberately measured at TICKET-049 step 7 — is a one-line edit
/// here, not a six-site rewrite across every `WireValue` core arm.
#[derive(Default, Debug)]
pub struct SeenCores(super::fxhash::FxHashSet<CoreId>);

impl SeenCores {
    /// Build a set already containing `ptr` (the mark walk's own core, seeded before it walks its
    /// payload).
    pub fn with(ptr: usize) -> Self {
        let mut s = Self::default();
        s.insert(ptr);
        s
    }

    /// Insert `ptr`; `true` iff it was not already present (same contract as `HashSet::insert`).
    pub fn insert(&mut self, ptr: usize) -> bool {
        self.0.insert(CoreId(ptr))
    }
}

#[cfg(test)]
thread_local! {
    /// TICKET-049 — count of `CoreId::eq`/`CoreId::hash` calls, i.e. identity-comparison probes the
    /// mark walk's dedup performs. Thread-local because `cargo test --lib` runs ~4 448 tests
    /// concurrently at `RUST_TEST_THREADS=4`, and a process-global counter would be polluted by any
    /// other test marking a core on another thread at the same time.
    static CORE_PROBES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub fn probe_reset() {
    CORE_PROBES.with(|c| c.set(0));
}

#[cfg(test)]
pub fn probes() -> u64 {
    CORE_PROBES.with(|c| c.get())
}

/// B3.1 GC support — collect every `GcRef` reachable from a core's wire contents into `out`, so the
/// heap's `children()` can keep those heap objects rooted. A core's `WireValue`s can still carry
/// `Handle(GcRef)`s into the live heap (an `Executor` queues `Closure` handles — closures can't cross
/// by value until B3.3/G1; a `Channel[str]` now queues owned bytes (B3.3a), rooting nothing).
///
/// It **recurses into nested cores** (a `Channel` stored inside a `Shared`, etc.): a nested core may
/// be reachable *only* through its parent core (its own heap handle already swept), so its embedded
/// handles would dangle if we stopped at the boundary. `seen` (core identities by `Arc` pointer)
/// breaks the `Arc` reference cycles decision E warns about — a cycle is walked once, not forever.
pub fn collect_core_gcrefs(w: &WireValue, out: &mut Vec<GcRef>, seen: &mut SeenCores) {
    // ONE core lock at a time — see the ABBA note on `collect_gcrefs_structural`. Callers that
    // already hold a core's guard must NOT use this entry point: they call
    // `collect_gcrefs_structural` under their guard, drop it, then `drain_pending_cores`.
    let mut pending: Vec<WireValue> = Vec::new();
    collect_gcrefs_structural(w, out, seen, &mut pending);
    drain_pending_cores(out, seen, &mut pending);
}

/// Drain the nested cores queued by [`collect_gcrefs_structural`], locking exactly ONE at a time.
///
/// **Call this with NO core guard held.** `Heap::children` locks a core to read its payload, so if it
/// drained while still holding that guard it would hold two core locks at once and the ABBA window
/// stays open — which is exactly what a first attempt at this fix measured: unchanged at 8/40 hangs.
///
/// **Why dropping the guard between cores cannot UNDER-ROOT.** Splitting the walk means core X and a
/// nested core Y are no longer read under one atomic pair of locks, so in principle a store could move
/// a `Handle` from Y to X after X was walked and before Y is — invisible to both halves. That cannot
/// happen for a handle belonging to the heap being marked: heaps are **per-fiber/per-worker**
/// (`Vm::heap`, swapped by `Vm::swap_ctx`) and a collection runs **synchronously on the owning worker**
/// from its own instruction loop (`Vm::collect` at `exec.rs`'s `should_collect()` check), so while this
/// heap is marking, the fiber that owns it is not running user code and cannot be mid-move. Another
/// worker concurrently moving handles is moving handles of ITS heap, which this mark does not root.
/// What the old code bought with the second lock was cross-core atomicity that this invariant already
/// provides — at the cost of the deadlock above. Probed as well as argued: a 400-node cyclic graph
/// under 60k allocations, a diamond (one node reachable by two paths), and handles moved between cores
/// under concurrent mutation all report 0 corruption on both profiles at 2/4/8 workers, with the
/// `debug_assert` in `Heap::mark_core_payload` re-deriving the verdict on every debug-build pass.
pub fn drain_pending_cores(
    out: &mut Vec<GcRef>,
    seen: &mut SeenCores,
    pending: &mut Vec<WireValue>,
) {
    while let Some(core) = pending.pop() {
        match &core {
            WireValue::Channel(c) => {
                let q = c.q.lock().unwrap();
                for w in q.iter() {
                    collect_gcrefs_structural(w, out, seen, pending);
                }
            }
            WireValue::Shared(c) => {
                let v = c.v.lock().unwrap();
                collect_gcrefs_structural(&v, out, seen, pending);
            }
            WireValue::RwShared(c) => {
                let v = c.v.read().unwrap();
                collect_gcrefs_structural(&v, out, seen, pending);
            }
            WireValue::Atomic(c) => {
                let v = c.v.lock().unwrap();
                collect_gcrefs_structural(&v, out, seen, pending);
            }
            WireValue::Executor(c) => {
                let q = c.inner.lock().unwrap();
                for w in q.iter() {
                    collect_gcrefs_structural(w, out, seen, pending);
                }
            }
            // `pending` only ever receives the five core variants above.
            _ => {}
        }
        // The guard above is dropped HERE, before the next core is locked. That is the whole fix.
    }
}

/// The lock-free half of [`collect_core_gcrefs`]: walk one already-locked `WireValue` structurally,
/// pushing `Handle`s into `out` and QUEUEING any nested core into `pending` instead of locking it.
///
/// **Never lock a core from in here.** This function runs with a core's payload guard held, and the
/// walk used to recurse straight into a nested core's lock while holding it. With a CYCLIC core graph
/// two workers marking concurrently then took the same two locks in opposite orders — a textbook ABBA
/// deadlock: every thread parked in `futex_do_wait` at 0% CPU, no deadlock report, unkillable by
/// `--timeout`. Measured on the fixture in `docs/benchmarks.md` (a `Channel` whose queued struct
/// reaches back to it): ~20% of runs at `CHEZZI_THREADS>=2`, 0% at `=1`, 0% with the cycle removed,
/// 0% with allocation (hence GC) removed.
pub fn collect_gcrefs_structural(
    w: &WireValue,
    out: &mut Vec<GcRef>,
    seen: &mut SeenCores,
    pending: &mut Vec<WireValue>,
) {
    match w {
        WireValue::Handle(g) => out.push(*g),
        WireValue::List { items: xs, .. } | WireValue::Tuple { items: xs, .. } => xs
            .iter()
            .for_each(|x| collect_gcrefs_structural(x, out, seen, pending)),
        WireValue::Map { entries, .. } => entries.iter().for_each(|(_, k, v)| {
            collect_gcrefs_structural(k, out, seen, pending);
            collect_gcrefs_structural(v, out, seen, pending);
        }),
        WireValue::Set { entries, .. } => entries
            .iter()
            .for_each(|(_, e)| collect_gcrefs_structural(e, out, seen, pending)),
        WireValue::Struct { fields, .. } => fields
            .iter()
            .for_each(|(_, v)| collect_gcrefs_structural(v, out, seen, pending)),
        WireValue::Enum { payload, .. } => payload
            .iter()
            .for_each(|x| collect_gcrefs_structural(x, out, seen, pending)),
        WireValue::NewType { inner, .. } => collect_gcrefs_structural(inner, out, seen, pending),
        // A cell queued in a channel/executor roots its inner value's handles (like `NewType`).
        WireValue::Cell { inner, .. } => collect_gcrefs_structural(inner, out, seen, pending),
        // A cursor queued in a channel/executor roots its snapshot items' handles (like `List`).
        WireValue::Iter { items, .. } => items
            .iter()
            .for_each(|x| collect_gcrefs_structural(x, out, seen, pending)),
        // F3 path C: a generator queued in a channel/executor crosses by value, but its backing
        // closure or a parked slot could still embed a `Handle` into the live heap — root them while
        // the generator sits in the queue (like `Closure`/`Iter`).
        WireValue::Generator { closure, state, .. } => {
            if let Some(c) = closure {
                collect_gcrefs_structural(c, out, seen, pending);
            }
            match state {
                WireGenState::Pending(args, _) => args
                    .iter()
                    .for_each(|x| collect_gcrefs_structural(x, out, seen, pending)),
                WireGenState::Suspended { stack, .. } => stack
                    .iter()
                    .for_each(|x| collect_gcrefs_structural(x, out, seen, pending)),
                WireGenState::Done | WireGenState::Unsendable(_) => {}
            }
        }
        // A nested core is QUEUED, never locked here — `seen` keeps each one queued at most once,
        // which is also what terminates a cyclic core graph. Cloning the variant is an `Arc` refcount
        // bump, and it is what keeps the core alive between the queue and the drain.
        WireValue::Channel(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                pending.push(w.clone());
            }
        }
        WireValue::Shared(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                pending.push(w.clone());
            }
        }
        WireValue::RwShared(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                pending.push(w.clone());
            }
        }
        WireValue::Atomic(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                pending.push(w.clone());
            }
        }
        // `AtomicInt` holds a plain i64 — no heap refs to trace (identity-only wire visit).
        WireValue::AtomicInt(_) => {}
        WireValue::Executor(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                pending.push(w.clone());
            }
        }
        // B3.6: a submitted closure queued in an `Executor` crosses by value, but its captures may
        // still embed `Handle`s into the live heap (a captured `Channel[str]`'s bytes root nothing,
        // but a captured callable would) — root them while the task sits in the queue.
        WireValue::Closure { captured, .. } => {
            captured
                .iter()
                .for_each(|(_, v)| collect_gcrefs_structural(v, out, seen, pending));
        }
        // B3.3a: `Str` crosses by value (owned bytes in the core) — it roots no heap object.
        // D6: a `Socket`/`Listener` core holds an OS fd + a poll key — no `WireValue`s, no `GcRef`s.
        // `bytes`/`bytearray` cross by value (owned raw bytes) — root no heap object.
        // An opaque `ptr` crosses by value (a raw address) — it roots no heap object.
        // A first-class builtin fn crosses by value (its name) — pure code, roots no heap object.
        // A native fn crosses by value (name + fn ptr) and a Cffi as a shared `Arc` — neither holds a
        // `GcRef`, so both root no heap object.
        // B3.3: a bare fn crosses by value (proto id + home index) — no captures, roots no heap object.
        // R2: a `Writer` core holds an fd/buffer + a key — no `WireValue`s, no `GcRef`s (like `Socket`).
        // R2b: a `Reader` core holds a BufReader<File> + a key — likewise no `WireValue`s, no `GcRef`s.
        // A back-reference roots nothing: its target (an already-walked identity-preserved node — a
        // Cell/Closure or a container) is reachable elsewhere in the same wire graph, so its handles
        // are already collected there. It also TERMINATES the walk on a now-cyclic wire graph.
        WireValue::Backref(_)
        | WireValue::Str(_)
        | WireValue::Bytes(_)
        | WireValue::ByteArray(_)
        | WireValue::Int(_)
        | WireValue::Float(_)
        | WireValue::Bool(_)
        | WireValue::Socket(_)
        | WireValue::Listener(_)
        | WireValue::Writer(_)
        | WireValue::Reader(_)
        | WireValue::Ptr(_)
        | WireValue::Builtin(_)
        | WireValue::Native { .. }
        | WireValue::Cffi(_)
        | WireValue::Func { .. }
        | WireValue::Nil => {}
    }
}

/// [`WireSummary`] state: never walked (or invalidated) — the GC must walk and then memoize. This is
/// the `Default`, so any core built without going through a store path degrades to today's behaviour
/// (a full walk) rather than to an under-rooted heap.
pub const WS_UNKNOWN: u8 = 0;
/// [`WireSummary`] state: the payload provably holds no `Handle` and no nested core — the GC skips it.
pub const WS_CLEAN: u8 = 1;
/// [`WireSummary`] state: the payload may root a heap object — walk it, every pass, never memoize.
pub const WS_DIRTY: u8 = 2;

/// W6-7/W6-10 — the cached GC summary of a single-value core's payload (`Shared`/`RwShared`/`Atomic`).
///
/// `state` answers "can the GC skip this subtree?" and `bytes` feeds `--max-heap` (an airlocked
/// `WireValue` lives in an `Arc` **outside** every [`Heap`](super::heap::Heap), so `live_bytes` used to
/// count it nowhere). Both are computed by ONE [`wire_summary`] walk at STORE time — the payload of
/// these cores is *replaced*, not mutated in place, so every write path must call [`set`](Self::set)
/// **while holding the same value lock as the write**: a stale `CLEAN` would stop the GC tracing a live
/// handle. A `debug_assert` in `Heap::children` re-verifies the memo on every debug-build GC pass.
#[derive(Debug, Default)]
pub struct WireSummary {
    state: AtomicU8,
    bytes: AtomicUsize,
}

impl WireSummary {
    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Record the summary of a payload that has just been stored (absolute, not incremental).
    pub fn set(&self, w: &WireValue) {
        let (b, dirty) = wire_summary(w);
        self.store(b, dirty);
    }

    /// Record an already-computed summary (the lazy GC fill path).
    pub fn store(&self, bytes: usize, dirty: bool) {
        self.bytes.store(bytes, Ordering::Relaxed);
        self.state
            .store(if dirty { WS_DIRTY } else { WS_CLEAN }, Ordering::Relaxed);
    }

    /// TICKET-192 — fold one in-place entry write into the summary: `add_bytes`/`add_dirty` are the
    /// new pieces' [`wire_summary`], `sub_bytes` the replaced pieces' bytes. Leaves `UNKNOWN` alone
    /// (the GC still walks and fills it), saturates bytes at zero, and never turns `DIRTY` back into
    /// `CLEAN`: dropping one dirty piece does not prove the rest clean. Only a whole store cleans.
    /// Call it under the same write lock as the entry write.
    pub fn adjust(&self, add_bytes: usize, add_dirty: bool, sub_bytes: usize) {
        if self.state() == WS_UNKNOWN {
            return;
        }
        let b = self
            .bytes()
            .saturating_add(add_bytes)
            .saturating_sub(sub_bytes);
        self.bytes.store(b, Ordering::Relaxed);
        if add_dirty {
            self.state.store(WS_DIRTY, Ordering::Relaxed);
        }
    }
}

/// W6-7/W6-10 — ONE walk of a stored wire payload yielding both GC facts: `(approximate owned bytes,
/// can-root-a-heap-object)`.
///
/// **This is NOT [`WireValue::has_handle`]**, and the two must never be merged. `has_handle` answers an
/// *airlock* question ("may this value cross?") and deliberately returns `false` for the nested
/// `Channel`/`Shared`/`RwShared`/`Atomic`/`Executor` arms — those cross by shared `Arc`. [`collect_core_gcrefs`]
/// (right above) *recurses into* them, because a nested core may be reachable only through its parent and
/// its embedded handles would dangle otherwise. So a cached `has_handle` verdict would be a use-after-free.
/// Here a nested core is therefore **always dirty**, and the walk STOPS at that boundary.
///
/// The byte half of that stop is completed by [`nested_core_bytes`] (gaps.md `W6-10r`, FIXED
/// 2026-08-06), NOT by this function: a nested core's bytes belong to that core's own summary, and
/// `Heap::live_bytes` used to reach them only through its `Obj::*` alias slot — so a nested core
/// whose last alias slot had been swept was counted NOWHERE. `live_bytes` now runs the cross-core
/// byte recursion (`Arc`-de-duped, under a live `--max-heap` only); the walk here still stops.
/// That closed `W6-10r`, not the cap in general — an `Executor`'s eager half followed in `W7-26`
/// (FIXED 2026-08-06, both here and in `nested_core_bytes`). The inline-scalar escape
/// (`future.md §1b`) and the join-window sampling residual (`W7-26r`) are FIXED too — `W7-28`
/// 2026-08-07 and `W7-26r` 2026-08-06 — as is `W6-10s` residual (a) (a task whose whole body is one
/// native call reaches no instruction boundary), fixed by `W7-29` 2026-08-07: `Vm::start_task`
/// samples the cap before dispatch, with the pending call's operands rooted on the operand stack.
///
/// Keep the arms in lockstep with [`collect_core_gcrefs`] — a new `WireValue` variant must be added to both.
pub fn wire_summary(w: &WireValue) -> (usize, bool) {
    fn walk(acc: &mut (usize, bool), x: &WireValue) {
        let (b, d) = wire_summary(x);
        acc.0 += b;
        acc.1 |= d;
    }
    let mut acc = (std::mem::size_of::<WireValue>(), false);
    match w {
        WireValue::Handle(_) => acc.1 = true,
        WireValue::List { items: xs, .. } | WireValue::Tuple { items: xs, .. } => {
            xs.iter().for_each(|x| walk(&mut acc, x))
        }
        WireValue::Map { entries, .. } => entries.iter().for_each(|(_, k, v)| {
            acc.0 += std::mem::size_of::<u64>();
            walk(&mut acc, k);
            walk(&mut acc, v);
        }),
        WireValue::Set { entries, .. } => entries.iter().for_each(|(_, e)| {
            acc.0 += std::mem::size_of::<u64>();
            walk(&mut acc, e);
        }),
        WireValue::Struct { name, fields, .. } => {
            acc.0 += name.len();
            fields.iter().for_each(|(n, v)| {
                acc.0 += n.len();
                walk(&mut acc, v);
            })
        }
        WireValue::Enum { payload, .. } => payload.iter().for_each(|x| walk(&mut acc, x)),
        WireValue::NewType {
            type_key, inner, ..
        } => {
            acc.0 += type_key.len();
            walk(&mut acc, inner)
        }
        WireValue::Cell { inner, .. } => walk(&mut acc, inner),
        WireValue::Iter { items, .. } => items.iter().for_each(|x| walk(&mut acc, x)),
        WireValue::Generator { closure, state, .. } => {
            if let Some(c) = closure {
                walk(&mut acc, c);
            }
            match state {
                WireGenState::Pending(args, _) => args.iter().for_each(|x| walk(&mut acc, x)),
                WireGenState::Suspended { stack, .. } => {
                    stack.iter().for_each(|x| walk(&mut acc, x))
                }
                WireGenState::Done | WireGenState::Unsendable(_) => {}
            }
        }
        WireValue::Closure { captured, .. } => {
            captured.iter().for_each(|(n, v)| {
                acc.0 += n.len();
                walk(&mut acc, v);
            });
        }
        // A nested core: conservatively dirty (a store on the INNER core can introduce a handle
        // without ever touching this one's cache), and the walk stops here.
        WireValue::Channel(_)
        | WireValue::Shared(_)
        | WireValue::RwShared(_)
        | WireValue::Atomic(_)
        | WireValue::Executor(_) => acc.1 = true,
        WireValue::Str(s) => acc.0 += s.len(),
        WireValue::Bytes(b) | WireValue::ByteArray(b) => acc.0 += b.len(),
        // Leaves — root nothing, own no extra bytes. `Backref` also TERMINATES a cyclic wire graph
        // (exactly like `collect_core_gcrefs`).
        WireValue::Backref(_)
        | WireValue::AtomicInt(_)
        | WireValue::Int(_)
        | WireValue::Float(_)
        | WireValue::Bool(_)
        | WireValue::Socket(_)
        | WireValue::Listener(_)
        | WireValue::Writer(_)
        | WireValue::Reader(_)
        | WireValue::Ptr(_)
        | WireValue::Builtin(_)
        | WireValue::Native { .. }
        | WireValue::Cffi(_)
        | WireValue::Func { .. }
        | WireValue::Nil => {}
    }
    acc
}

/// W6-10r — the BYTE mirror of [`collect_core_gcrefs`]: sum the cached byte counts of every core
/// nested inside `w`, so `--max-heap` sees a payload that is reachable ONLY through another core.
///
/// [`wire_summary`] deliberately stops at a nested-core boundary (those bytes belong to that core's
/// own summary), and [`Heap::live_bytes`](super::heap::Heap::live_bytes) reaches a core's summary
/// only through its `Obj::*` alias slot. A nested core whose last alias slot has been swept — it
/// survives inside this payload's `Arc` — therefore used to be counted **nowhere**: a `Channel`
/// parked in a `Shared` backlogged 304 MB past an 8 MB cap and PASSED. Rooting was never affected
/// ([`collect_core_gcrefs`] does recurse); only the byte walk stopped.
///
/// `seen` is the caller's per-heap `Arc`-identity set, SHARED with `live_bytes`'s own per-slot
/// de-dup: a nested core that also has an alias slot in this heap is charged exactly once, whichever
/// way it is met first. It also terminates `Arc` cycles (the reason [`visit_core`] exists).
///
/// Only entered under a live `mem_cap` (see `live_bytes`) — with no cap `over_cap` is meaningless and
/// this walk is pure cost.
///
/// Keep the arms in lockstep with [`collect_core_gcrefs`] and [`wire_summary`] — a new `WireValue`
/// variant must be added to all three.
///
/// **Do NOT call this while holding any core's payload guard** — it drains nested cores, which locks
/// them, and holding a second core lock is the ABBA deadlock `drain_pending_core_bytes` documents.
/// A caller that already holds a guard calls the `_structural` half under it, drops it, then drains.
pub fn nested_core_bytes(w: &WireValue, seen: &mut super::fxhash::FxHashSet<usize>) -> usize {
    let mut pending: Vec<WireValue> = Vec::new();
    let acc = nested_core_bytes_structural(w, seen, &mut pending);
    acc + drain_pending_core_bytes(seen, &mut pending)
}

/// Bytes owned by the cores queued by the structural byte walk, locking exactly ONE at a time.
///
/// **Call with NO core guard held**, for the same reason as [`drain_pending_cores`]: this walk is the
/// byte-accounting twin of the rooting walk and had the identical ABBA shape — a parent core's guard
/// held while a child's lock was taken. It is reached through `--max-heap` (`Heap::live_bytes`'s
/// `deep` branch), so before this split a cyclic core graph could hang the very mechanism whose job is
/// to abort a runaway run. Measured pre-split: `chezzi test --max-heap=100000000` on a cyclic-core
/// fixture at `CHEZZI_THREADS=4` hung 1/40 runs, versus 0/20 for the same program without the cap.
pub fn drain_pending_core_bytes(
    seen: &mut super::fxhash::FxHashSet<usize>,
    pending: &mut Vec<WireValue>,
) -> usize {
    let mut acc = 0usize;
    while let Some(core) = pending.pop() {
        match &core {
            WireValue::Channel(c) => {
                let g = c.q.lock().unwrap();
                acc += queue_bytes_structural(g.summary(), g.iter(), seen, pending);
            }
            // W7-26 — the queue half under its own scoped guard, then `ExecutorCore::held_bytes` (the
            // one facade `Heap::live_bytes`'s `Obj::Executor` arm reads too), never both held at once.
            WireValue::Executor(c) => {
                let queued = {
                    let g = c.inner.lock().unwrap();
                    queue_bytes_structural(g.summary(), g.iter(), seen, pending)
                };
                acc += queued + c.held_bytes();
            }
            WireValue::Shared(c) => {
                let g = c.v.lock().unwrap();
                acc += value_core_bytes_structural(&c.summary, &g, seen, pending);
            }
            WireValue::RwShared(c) => {
                let g = c.v.read().unwrap();
                acc += value_core_bytes_structural(&c.summary, &g, seen, pending);
            }
            WireValue::Atomic(c) => {
                let g = c.v.lock().unwrap();
                acc += value_core_bytes_structural(&c.summary, &g, seen, pending);
            }
            // `pending` only ever receives the five core variants above.
            _ => {}
        }
        // Guard dropped HERE, before the next core is locked.
    }
    acc
}

/// The lock-free half of [`nested_core_bytes`]: walk one already-locked payload, QUEUEING nested cores
/// into `pending` instead of locking them. **Never lock a core from in here.**
fn nested_core_bytes_structural(
    w: &WireValue,
    seen: &mut super::fxhash::FxHashSet<usize>,
    pending: &mut Vec<WireValue>,
) -> usize {
    let mut acc = 0usize;
    match w {
        WireValue::List { items: xs, .. } | WireValue::Tuple { items: xs, .. } => {
            for x in xs {
                acc += nested_core_bytes_structural(x, seen, pending);
            }
        }
        WireValue::Map { entries, .. } => {
            for (_, k, v) in entries.iter() {
                acc += nested_core_bytes_structural(k, seen, pending)
                    + nested_core_bytes_structural(v, seen, pending);
            }
        }
        WireValue::Set { entries, .. } => {
            for (_, e) in entries.iter() {
                acc += nested_core_bytes_structural(e, seen, pending);
            }
        }
        WireValue::Struct { fields, .. } => {
            for (_, v) in fields {
                acc += nested_core_bytes_structural(v, seen, pending);
            }
        }
        WireValue::Enum { payload, .. } => {
            for x in payload {
                acc += nested_core_bytes_structural(x, seen, pending);
            }
        }
        WireValue::NewType { inner, .. } | WireValue::Cell { inner, .. } => {
            acc += nested_core_bytes_structural(inner, seen, pending)
        }
        WireValue::Iter { items, .. } => {
            for x in items {
                acc += nested_core_bytes_structural(x, seen, pending);
            }
        }
        WireValue::Generator { closure, state, .. } => {
            if let Some(c) = closure {
                acc += nested_core_bytes_structural(c, seen, pending);
            }
            match state {
                WireGenState::Pending(args, _) => {
                    for x in args {
                        acc += nested_core_bytes_structural(x, seen, pending);
                    }
                }
                WireGenState::Suspended { stack, .. } => {
                    for x in stack {
                        acc += nested_core_bytes_structural(x, seen, pending);
                    }
                }
                WireGenState::Done | WireGenState::Unsendable(_) => {}
            }
        }
        WireValue::Closure { captured, .. } => {
            for (_, v) in captured {
                acc += nested_core_bytes_structural(v, seen, pending);
            }
        }
        // The nested cores themselves — charge each one's payload ONCE per heap, then keep
        // recursing (a core nested two deep is just as invisible as one nested once).
        WireValue::Channel(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                pending.push(w.clone());
            }
        }
        // W7-26 — BOTH halves, exactly like `Heap::live_bytes`'s `Obj::Executor` arm. Keeping them
        // in lockstep is not cosmetic: `seen`/`cores` is SHARED between the two walks, so whichever
        // one meets a core first is the only one that charges it. An arm that reads a single half
        // would therefore silently drop the other half whenever the enclosing core happens to be
        // visited first — measured during review of this fix: an executor holding 880 400 bytes of
        // eager results, reached through an `Obj::Shared` payload, was counted as 240.
        WireValue::Executor(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                pending.push(w.clone());
            }
        }
        WireValue::Shared(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                pending.push(w.clone());
            }
        }
        WireValue::RwShared(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                pending.push(w.clone());
            }
        }
        WireValue::Atomic(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                pending.push(w.clone());
            }
        }
        // Leaves — own no nested core. `Backref` also TERMINATES a cyclic wire graph (exactly like
        // `collect_core_gcrefs` / `wire_summary`).
        WireValue::Handle(_)
        | WireValue::Backref(_)
        | WireValue::AtomicInt(_)
        | WireValue::Str(_)
        | WireValue::Bytes(_)
        | WireValue::ByteArray(_)
        | WireValue::Int(_)
        | WireValue::Float(_)
        | WireValue::Bool(_)
        | WireValue::Socket(_)
        | WireValue::Listener(_)
        | WireValue::Writer(_)
        | WireValue::Reader(_)
        | WireValue::Ptr(_)
        | WireValue::Builtin(_)
        | WireValue::Native { .. }
        | WireValue::Cffi(_)
        | WireValue::Func { .. }
        | WireValue::Nil => {}
    }
    acc
}

/// W6-10r — a queue core's own bytes PLUS every core nested in its messages. Takes the summary and
/// the messages rather than the state, so the identically-shaped [`ChanState`] and [`ExecState`]
/// share one implementation.
///
/// The `(bytes, dirty)` summary is maintained incrementally by `push`/`pop`, so the walk is skipped
/// outright for a clean queue.
///
/// `dirty` is conservative for this purpose: it is also set by a bare `Handle`, so a queue of plain
/// heap references is walked and finds nothing.
/// `ponytail: one bit conflates "has a handle" with "has a nested core"` — splitting them means a
/// third field threaded through `WireSummary` and `ChanState::push`'s tuple at every call site; do it
/// only if a profile says this walk matters.
///
/// **Do NOT call this while holding any core's payload guard** — it drains nested cores, which locks
/// them, and holding a second core lock is the ABBA deadlock `drain_pending_core_bytes` documents.
/// A caller that already holds a guard calls the `_structural` half under it, drops it, then drains.
pub fn queue_bytes_deep<'a>(
    summary: (usize, bool),
    msgs: impl Iterator<Item = &'a WireValue>,
    seen: &mut super::fxhash::FxHashSet<usize>,
) -> usize {
    let mut pending: Vec<WireValue> = Vec::new();
    let acc = queue_bytes_structural(summary, msgs, seen, &mut pending);
    acc + drain_pending_core_bytes(seen, &mut pending)
}

/// The lock-free half of [`queue_bytes_deep`] — call it under the queue's guard, then drop the guard
/// and call [`drain_pending_core_bytes`].
pub fn queue_bytes_structural<'a>(
    summary: (usize, bool),
    msgs: impl Iterator<Item = &'a WireValue>,
    seen: &mut super::fxhash::FxHashSet<usize>,
    pending: &mut Vec<WireValue>,
) -> usize {
    let (bytes, dirty) = summary;
    if !dirty {
        return bytes;
    }
    bytes
        + msgs
            .map(|w| nested_core_bytes_structural(w, seen, pending))
            .sum::<usize>()
}

/// W6-10r — a single-value core's own bytes PLUS every core nested in its payload. Call with the
/// payload lock held.
///
/// A `WS_UNKNOWN` summary is filled here (exactly as [`Heap::children`](super::heap::Heap::children)
/// fills it during marking): every core CONSTRUCTOR leaves it `UNKNOWN`, and a core reachable only
/// through a parent is never marked through an alias slot of its own — so without this fill it would
/// report 0 bytes forever, which is the very hole being closed.
///
/// **Do NOT call this while holding any core's payload guard** — it drains nested cores, which locks
/// them, and holding a second core lock is the ABBA deadlock `drain_pending_core_bytes` documents.
/// A caller that already holds a guard calls the `_structural` half under it, drops it, then drains.
pub fn value_core_bytes_deep(
    summary: &WireSummary,
    w: &WireValue,
    seen: &mut super::fxhash::FxHashSet<usize>,
) -> usize {
    let mut pending: Vec<WireValue> = Vec::new();
    let acc = value_core_bytes_structural(summary, w, seen, &mut pending);
    acc + drain_pending_core_bytes(seen, &mut pending)
}

/// The lock-free half of [`value_core_bytes_deep`] — call it under the payload guard, then drop the
/// guard and call [`drain_pending_core_bytes`].
pub fn value_core_bytes_structural(
    summary: &WireSummary,
    w: &WireValue,
    seen: &mut super::fxhash::FxHashSet<usize>,
    pending: &mut Vec<WireValue>,
) -> usize {
    if summary.state() == WS_UNKNOWN {
        summary.set(w);
    }
    let bytes = summary.bytes();
    if summary.state() == WS_DIRTY {
        bytes + nested_core_bytes_structural(w, seen, pending)
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    /// TICKET-192 — `adjust` keeps UNKNOWN, moves bytes by the delta (saturating), turns CLEAN into
    /// DIRTY on a dirty piece, and never turns DIRTY into CLEAN.
    #[test]
    fn wire_summary_adjust_never_cleans_a_dirty_summary() {
        let s = WireSummary::default();
        s.adjust(10, true, 0);
        assert_eq!((s.state(), s.bytes()), (WS_UNKNOWN, 0));
        s.store(100, false);
        s.adjust(5, false, 0);
        assert_eq!((s.state(), s.bytes()), (WS_CLEAN, 105));
        s.adjust(10, true, 15);
        assert_eq!((s.state(), s.bytes()), (WS_DIRTY, 100));
        s.adjust(0, false, 60);
        assert_eq!((s.state(), s.bytes()), (WS_DIRTY, 40));
        s.adjust(0, false, 1000);
        assert_eq!((s.state(), s.bytes()), (WS_DIRTY, 0));
    }

    /// D6 — every `SocketCore`/`ListenerCore` gets a fresh, distinct poll key (the ABA-avoiding
    /// identity), and a freshly-built core holds its stream `Some` (open).
    #[test]
    fn socket_cores_have_unique_keys_and_an_open_stream() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).unwrap();
        let s1 = SocketCore {
            stream: Mutex::new(Some(stream)),
            key: next_poll_key(),
            in_flight: new_in_flight(),
            closed: new_closed(),
            carry: Mutex::new(Vec::new()),
        };
        let s2 = ListenerCore {
            listener: Mutex::new(Some(listener)),
            key: next_poll_key(),
            in_flight: new_in_flight(),
            closed: new_closed(),
        };
        assert_ne!(s1.key, s2.key, "each core gets a distinct poll key");
        assert!(
            s1.stream.lock().unwrap().is_some(),
            "a fresh socket core is open"
        );
        assert!(
            s2.listener.lock().unwrap().is_some(),
            "a fresh listener core is open"
        );
    }

    /// W6-1 sibling — the drop-flush must walk a NESTED `buffered(buffered(file))` chain, not just a
    /// one-level `Buffered{inner=File}`. `docs/stdlib.md` promises a **file**-backed buffered writer's
    /// tail is recovered on drop, and a transitively file-backed chain is file-backed. Rust-only:
    /// `assert` can't observe drop timing (the Chezzi suite covers the explicit `flush`/`close` path).
    #[test]
    fn drop_flushes_a_nested_buffered_chain_to_the_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("chz_nested_drop_{}.txt", std::process::id()));
        let file = std::fs::File::create(&path).unwrap();
        let inner = Arc::new(WriterCore {
            inner: Mutex::new(Some(Backing::File(std::io::BufWriter::new(file)))),
            key: next_poll_key(),
        });
        let mid = Arc::new(WriterCore {
            inner: Mutex::new(Some(Backing::Buffered {
                inner: Arc::clone(&inner),
                buf: Vec::new(),
                cap: 8,
            })),
            key: next_poll_key(),
        });
        let outer = Arc::new(WriterCore {
            inner: Mutex::new(Some(Backing::Buffered {
                inner: Arc::clone(&mid),
                buf: b"abc".to_vec(),
                cap: 4,
            })),
            key: next_poll_key(),
        });
        drop(outer);
        drop(mid);
        drop(inner);
        let got = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(got, b"abc", "a nested buffered chain must drop-flush");
    }

    /// TICKET-049 — the mark walk's dedup cost must scale linearly with rooted core depth, never
    /// quadratically: charge one probe per `CoreId` comparison (`CoreId::eq`/`hash`), never a
    /// wall-clock sample. Doubling depth must not more than double the probe count; a linear-scan
    /// dedup (measured ratio ~4.0) would fail this, a hash-set dedup (measured ratio ~2.0) passes.
    #[test]
    fn mark_walk_dedup_is_linear_in_core_chain_depth() {
        fn chain(depth: usize) -> WireValue {
            let mut w = WireValue::Nil;
            for _ in 0..depth {
                let core: Arc<ChannelCore> = Arc::new(ChannelCore::default());
                let sum = wire_summary(&w);
                core.q.lock().unwrap().push(sum, w);
                w = WireValue::Channel(core);
            }
            w
        }

        fn probe_cost(depth: usize) -> u64 {
            let w = chain(depth);
            probe_reset();
            let mut out = Vec::new();
            let mut seen = SeenCores::default();
            collect_core_gcrefs(&w, &mut out, &mut seen);
            probes()
        }

        let shallow = probe_cost(1000);
        let deep = probe_cost(2000);
        assert!(
            shallow > 0,
            "the dedup must charge a probe per visited core"
        );
        assert!(
            deep * 10 < shallow * 28,
            "doubling rooted core depth must not more than double the mark walk's dedup cost (a \
             linear scan is ~4.0, the hash set ~2.0): got {shallow} probes -> {deep} probes"
        );
    }
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    fn list(items: Vec<WireValue>) -> WireValue {
        WireValue::List { id: 0, items }
    }

    /// W6-7/W6-10 — one walk yields both GC facts: approximate owned bytes, and whether the payload
    /// can root a heap object (a `Handle`, or ANY nested core that might come to hold one).
    #[test]
    fn wire_summary_bytes_and_dirtiness() {
        let node = std::mem::size_of::<WireValue>();
        let (b, d) = wire_summary(&list((0..1000).map(WireValue::Int).collect()));
        assert!(b >= 1000 * node, "1000 ints must be counted: {b}");
        assert!(!d, "pure ints root nothing");

        let (_, d) = wire_summary(&list(vec![WireValue::Handle(GcRef(0))]));
        assert!(d, "a nested Handle is dirty");

        // A nested core is ALWAYS dirty (it may gain a handle via its OWN store, invisible here)
        // and its bytes stop at the boundary.
        let inner = Arc::new(SharedCore {
            v: Mutex::new(list((0..1000).map(WireValue::Int).collect())),
            ..Default::default()
        });
        let (b, d) = wire_summary(&list(vec![WireValue::Shared(inner)]));
        assert!(d, "a nested core is conservatively dirty");
        assert!(
            b < 1000 * node,
            "nested-core bytes must NOT be included: {b}"
        );

        // A self-cycle terminates on `Backref` (this test completing IS the assertion).
        let (_, _) = wire_summary(&WireValue::List {
            id: 1,
            items: vec![WireValue::Backref(1)],
        });

        // Owned bytes of the by-value scalar arms are counted.
        let (b, _) = wire_summary(&WireValue::Str("hello".into()));
        assert_eq!(b, node + 5);
        let (b, _) = wire_summary(&WireValue::Bytes(vec![1u8; 32].into()));
        assert_eq!(b, node + 32);
        let (b, _) = wire_summary(&WireValue::ByteArray(vec![1u8; 7].into()));
        assert_eq!(b, node + 7);
    }

    /// The cached summary: `Default` is UNKNOWN (walk), a store is absolute, and it is fail-safe in
    /// the UNKNOWN direction only.
    #[test]
    fn wire_summary_state_transitions() {
        let s = WireSummary::default();
        assert_eq!(s.state(), WS_UNKNOWN);
        assert_eq!(s.bytes(), 0);

        s.set(&list((0..10).map(WireValue::Int).collect()));
        assert_eq!(s.state(), WS_CLEAN);
        assert!(s.bytes() > 0);

        s.set(&list(vec![WireValue::Handle(GcRef(3))]));
        assert_eq!(s.state(), WS_DIRTY);

        // A store is ABSOLUTE — a dirty payload replaced by a clean one goes back to CLEAN.
        s.set(&WireValue::Int(1));
        assert_eq!(s.state(), WS_CLEAN);
    }
}
