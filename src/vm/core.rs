//! B3.1 — the shared cores for `Channel` / `Shared` / `Executor`, lifted OUT of the GC heap.
//!
//! The heap keeps only a *handle* — `Obj::Channel(Arc<ChannelCore>)` — and two handles (e.g. one
//! per task) can point at the same core, across OS threads. M3.2: a channel queue and an `Atomic`
//! hold [`Value`]s of the run's one heap, so a send or a store copies one word; the collector
//! walks a core's values under its lock (`Heap::children`).
//!
//! A `Condvar` (for real blocking `recv`) was added at B3.3; a `closed` flag (for `Channel.close()`)
//! lives alongside the queue under [`ChannelCore::q`]'s lock (see [`ChanState`]).

use super::value::Value;
use crate::lexer::Span;
use std::collections::{HashMap, VecDeque};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

/// M3.3 — the lock of one `Shared` / `RwShared` box (Go `sync.RWMutex`). One atomic word owns the
/// uncontended path: an acquire and a release are one CAS or one `fetch_*`, with no registry, no
/// allocation and no syscall.
///
/// `state` bits: [`WRITER`] (a writer holds the box), the count of writers that wait
/// ([`WW_ONE`] units, Go writer preference: while it is not zero a new reader waits), and the
/// count of readers that hold the box (the low 32 bits). `writer` is the task token of the writer
/// (0 = none), read only on a failed acquire to find a same-task re-entry. A reader is anonymous
/// here; the task that reads keeps its own list (`Vm::held_reads`). `sleepers` counts the tasks that
/// wait on the registry condvar for this box, so a release wakes them only when one exists.
/// `wait_readers` counts the readers that wait with a [`WaitTicket`] (their flags are in
/// `GuardGraph::readers`), so a write release admits them (Go `sync/rwmutex.go:214`, "Unblock
/// blocked readers, if any") only when one exists.
#[derive(Debug)]
pub struct BoxLock {
    state: AtomicU64,
    writer: AtomicU64,
    sleepers: AtomicU32,
    wait_readers: AtomicU32,
    /// `"Shared"` or `"RwShared"`: the ONE owner of the box kind a fault text names.
    pub kind: &'static str,
}

const WRITER: u64 = 1 << 63;
const WW_ONE: u64 = 1 << 32;
const WW_MASK: u64 = 0x7fff_ffff << 32;
const R_MASK: u64 = 0xffff_ffff;

impl BoxLock {
    pub fn new(kind: &'static str) -> Self {
        BoxLock {
            state: AtomicU64::new(0),
            writer: AtomicU64::new(0),
            sleepers: AtomicU32::new(0),
            wait_readers: AtomicU32::new(0),
            kind,
        }
    }

    /// The task token of the writer that holds the box, 0 when none does.
    pub fn writer(&self) -> u64 {
        self.writer.load(Ordering::Acquire)
    }

    /// Whether a writer waits for the box (a new reader must wait behind it).
    pub fn writer_waits(&self) -> bool {
        self.state.load(Ordering::Acquire) & WW_MASK != 0
    }

    /// One acquire attempt with no wait. `counted`: this writer is one of the waiting writers
    /// (it holds a [`WaitTicket`]), so the waiting count does not keep it out and the take moves it
    /// from waiting to holding.
    fn try_take(&self, mode: GuardMode, me: u64, counted: bool) -> bool {
        let mut s = self.state.load(Ordering::Relaxed);
        loop {
            let next = match mode {
                GuardMode::Write if s & (WRITER | R_MASK) == 0 => {
                    let ww = if counted { WW_ONE } else { 0 };
                    if !counted && s & WW_MASK != 0 {
                        return false;
                    }
                    (s - ww) | WRITER
                }
                GuardMode::Read if s & (WRITER | WW_MASK) == 0 && s & R_MASK != R_MASK => s + 1,
                GuardMode::Write | GuardMode::Read => return false,
            };
            match self
                .state
                .compare_exchange_weak(s, next, Ordering::Acquire, Ordering::Relaxed)
            {
                Ok(_) => {
                    if mode == GuardMode::Write {
                        self.writer.store(me, Ordering::Release);
                    }
                    return true;
                }
                Err(now) => s = now,
            }
        }
    }

    /// Whether an acquire in `mode` by a (counted) waiter could succeed now. SeqCst: with the
    /// waiter's `sleepers` increment before it and the releaser's SeqCst RMW then `sleepers` load,
    /// it is a Dekker pair, so either the waiter sees the release or the releaser sees the waiter.
    fn looks_free(&self, mode: GuardMode) -> bool {
        let s = self.state.load(Ordering::SeqCst);
        match mode {
            GuardMode::Write => s & (WRITER | R_MASK) == 0,
            GuardMode::Read => s & (WRITER | WW_MASK) == 0,
        }
    }

    /// The write release (Go `sync/rwmutex.go:201-222`, `Unlock`): with readers waiting, admit
    /// every one of them in the same RMW that clears [`WRITER`] (their read holds counted in R,
    /// their flags set), under the registry lock, so no writer that waits gets in ahead of them.
    /// A reader that registers after the check waits for the next release.
    fn release_write(&self) {
        self.writer.store(0, Ordering::Relaxed);
        if self.wait_readers.load(Ordering::SeqCst) == 0 {
            self.state.fetch_and(!WRITER, Ordering::SeqCst);
            self.wake();
            return;
        }
        let (mtx, cv) = guard_registry();
        let mut g = mtx.lock().unwrap_or_else(|e| e.into_inner());
        let flags = g
            .readers
            .remove(&(self as *const BoxLock as usize))
            .unwrap_or_default();
        let n = flags.len() as u64;
        let mut cur = self.state.load(Ordering::Relaxed);
        while let Err(now) = self.state.compare_exchange_weak(
            cur,
            (cur & !WRITER) + n,
            Ordering::SeqCst,
            Ordering::Relaxed,
        ) {
            cur = now;
        }
        for f in &flags {
            f.store(true, Ordering::SeqCst);
        }
        self.wait_readers.fetch_sub(n as u32, Ordering::SeqCst);
        drop(g);
        cv.notify_all();
    }

    /// Wake the tasks that wait for this box, if any. The registry has one condvar, so this
    /// wakes the waiters of every box (each re-checks its own word and sleeps again); a condvar
    /// per box would cost a `Mutex` + `Condvar` per box for a contended-only gain.
    fn wake(&self) {
        if self.sleepers.load(Ordering::SeqCst) > 0 {
            let (mtx, cv) = guard_registry();
            drop(mtx.lock().unwrap_or_else(|e| e.into_inner()));
            cv.notify_all();
        }
    }
}

/// TICKET-016 (W8-3) — the process-global wait-for graph, read only on the slow path (a contended
/// acquire). `waiting` maps a blocked task's token to the box and mode it wants. A walk from the
/// requester follows `waiting` -> the box's writer (`BoxLock::writer`) or a writer that waits for
/// it -> `waiting`: a walk that returns to the requester is a wait-for cycle, which faults instead
/// of blocking. A reader is anonymous ([`BoxLock`]), so a cycle that passes through a read guard is
/// left to the run's deadlock verdict (Go: `fatal error: all goroutines are asleep - deadlock!`).
///
/// No std lock here lives across user code or a fiber park: the registry `Mutex` is held only for
/// the map reads and writes below (and released by the `Condvar` wait).
#[derive(Default)]
struct GuardGraph {
    waiting: HashMap<u64, (Arc<BoxLock>, GuardMode)>,
    /// The readers that wait for a box (keyed by its `BoxLock` address), each by its admission
    /// flag: [`BoxLock::release_write`] admits them all.
    readers: HashMap<usize, Vec<Arc<AtomicBool>>>,
}

impl GuardGraph {
    /// The tasks a wait for `(lock, mode)` by `me` waits on: the writer, and for a read a writer
    /// that waits for the same box.
    fn blockers(&self, lock: &Arc<BoxLock>, mode: GuardMode, me: u64, out: &mut Vec<u64>) {
        let w = lock.writer();
        if w != 0 && w != me {
            out.push(w);
        }
        if mode == GuardMode::Read {
            for (t, (l, m)) in &self.waiting {
                if *t != me && *m == GuardMode::Write && Arc::ptr_eq(l, lock) {
                    out.push(*t);
                }
            }
        }
    }
}

fn guard_registry() -> &'static (Mutex<GuardGraph>, Condvar) {
    static REGISTRY: OnceLock<(Mutex<GuardGraph>, Condvar)> = OnceLock::new();
    REGISTRY.get_or_init(|| (Mutex::new(GuardGraph::default()), Condvar::new()))
}

/// M3.3 — how a box method holds its box: `Write` exclusive (`Shared` always; `RwShared.write`,
/// `set` and the single-entry writers), `Read` shared (`RwShared.get`, `read` and the read views).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMode {
    Write,
    Read,
}

/// One guard: a box's lock in one mode. Borrowed: the method that takes it holds the box core.
#[derive(Debug, Clone, Copy)]
pub struct GuardKey<'a> {
    pub lock: &'a Arc<BoxLock>,
    pub mode: GuardMode,
}

/// Why a guard acquire refused to block: a wait-for cycle exists. `SelfHeld` is the length-1 case
/// (the requester already holds the box's write guard, e.g. a nested `set`/`update`/`write`, or a
/// read, on the same box inside its own closure); `ReadHeld` is a write by a task that holds the
/// box's read guard (M3.3, Q2: Go `RLock(); Lock()`); `Cycle` is a longer cycle: a cross-task or
/// cross-box one (e.g. AB-BA), or a nested read past a waiting writer that waits for this task's
/// outer read (Go: a recursive `RLock` with a writer queued deadlocks).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardCycle {
    SelfHeld,
    ReadHeld,
    Cycle,
}

/// RAII handle for a held guard (TICKET-016 / W8-3): dropping it releases the hold and wakes the
/// tasks that wait for the box, if any. It belongs to the task, not to a thread or a lock guard,
/// so it may live across a park.
pub struct UpdateGuard<'a> {
    lock: &'a BoxLock,
    mode: GuardMode,
}

impl Drop for UpdateGuard<'_> {
    fn drop(&mut self) {
        match self.mode {
            GuardMode::Write => self.lock.release_write(),
            GuardMode::Read => {
                self.lock.state.fetch_sub(1, Ordering::SeqCst);
                self.lock.wake();
            }
        }
    }
}

/// M3.3 — a waiter's place in the line for a box, from its first failed acquire to its take or its
/// exit, across every stage of the wait and the width-permit handoff (Go `sync.RWMutex`):
/// - a writer is counted in the waiting-writer count: while it lives a new reader waits (Go
///   `rwmutex.go:151`, "Announce to readers there is a pending writer");
/// - a reader is registered by its admission flag (`GuardGraph::readers`): the next write release
///   admits it past every waiting writer (`rwmutex.go:214`), so back-to-back writers cannot starve
///   a reader that waited during a write.
pub struct WaitTicket<'a> {
    key: GuardKey<'a>,
    live: bool,
    admitted: Option<Arc<AtomicBool>>,
}

impl<'a> WaitTicket<'a> {
    pub fn new(key: GuardKey<'a>) -> Self {
        let live = key.mode == GuardMode::Write;
        let mut admitted = None;
        if live {
            key.lock.state.fetch_add(WW_ONE, Ordering::SeqCst);
        } else {
            let flag = Arc::new(AtomicBool::new(false));
            let (mtx, _cv) = guard_registry();
            let mut g = mtx.lock().unwrap_or_else(|e| e.into_inner());
            g.readers
                .entry(Arc::as_ptr(key.lock) as usize)
                .or_default()
                .push(Arc::clone(&flag));
            key.lock.wait_readers.fetch_add(1, Ordering::SeqCst);
            admitted = Some(flag);
        }
        WaitTicket {
            key,
            live,
            admitted,
        }
    }

    /// A reader's admission flag (set by a write release), for its registered wait.
    pub fn admission(&self) -> Option<Arc<AtomicBool>> {
        self.admitted.clone()
    }

    /// Whether a write release admitted this reader: it holds the read already.
    fn is_admitted(&self) -> bool {
        self.admitted
            .as_ref()
            .is_some_and(|f| f.load(Ordering::SeqCst))
    }

    /// Take a reader out of `GuardGraph::readers`: after it took the guard by its own CAS, or when
    /// its ticket drops before a take (a halt). A release can admit it before this lock (the release
    /// clears `WRITER` before it sets the flags, all under the registry lock): then it holds a read
    /// count it does not keep, and gives it back.
    fn unregister(&mut self) {
        if let Some(flag) = self.admitted.take() {
            let (mtx, _cv) = guard_registry();
            let mut g = mtx.lock().unwrap_or_else(|e| e.into_inner());
            if flag.load(Ordering::SeqCst) {
                drop(g);
                self.key.lock.state.fetch_sub(1, Ordering::SeqCst);
                self.key.lock.wake();
                return;
            }
            let addr = Arc::as_ptr(self.key.lock) as usize;
            if let Some(list) = g.readers.get_mut(&addr) {
                list.retain(|f| !Arc::ptr_eq(f, &flag));
                if list.is_empty() {
                    g.readers.remove(&addr);
                }
            }
            self.key.lock.wait_readers.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// The guard, when this waiter can take it now (no wait).
    pub fn try_take(&mut self, me: u64) -> Option<UpdateGuard<'a>> {
        // Built only on a take: a dropped guard releases a hold.
        let (lock, mode) = (self.key.lock, self.key.mode);
        if self.is_admitted() {
            // The release counted this read hold and dropped the registration.
            self.admitted = None;
            return Some(UpdateGuard { lock, mode });
        }
        if lock.try_take(mode, me, self.live) {
            // A counted writer's take moved it from waiting to holding.
            self.live = false;
            self.unregister();
            return Some(UpdateGuard { lock, mode });
        }
        None
    }
}

impl Drop for WaitTicket<'_> {
    fn drop(&mut self) {
        if self.live {
            self.key.lock.state.fetch_sub(WW_ONE, Ordering::SeqCst);
            self.key.lock.wake();
        }
        self.unregister();
    }
}

/// TICKET-016 — `GUARD_DEMOTE_BUDGET` is how long a guard acquire waits IN PLACE on its worker before
/// paying for a replacement OS thread. `Vm::block_enter` spawns one OS thread per demoting worker
/// shell, so an unconditional demote on every guarded `set`/`update`/`write` made 50 000 one-`update`
/// fibers exhaust a 32 768-task ceiling and never finish. Measured budgets on that test
/// (`TasksMax=32768`): 0 ms still peaks at 1091 threads, 1 ms at 35, 5 ms at 19.
pub const GUARD_DEMOTE_BUDGET: Duration = Duration::from_millis(5);

/// The fast path: take the guard `key` as task `me` with one CAS, or `None`. A `None` caller checks
/// a same-task re-entry, then waits through a [`WaitTicket`].
pub fn try_update_guard<'a>(key: GuardKey<'a>, me: u64) -> Option<UpdateGuard<'a>> {
    // Not `then_some`: its argument is built eagerly, and a dropped guard releases a hold.
    if key.lock.try_take(key.mode, me, false) {
        Some(UpdateGuard {
            lock: key.lock,
            mode: key.mode,
        })
    } else {
        None
    }
}

/// TICKET-193 — wait until `key` looks free for this waiter WITHOUT taking it: `Ok(true)` free,
/// `Ok(false)` budget spent (`None` = no limit), `Err` a wait-for cycle. `Vm::guard_wait_block`
/// waits here holding no width permit, then takes the permit, then the guard (permit before
/// guard; DEC-141). The waiter's [`WaitTicket`] stays live across the gap, so no new reader gets in.
pub fn await_update_guard_free(
    ticket: &WaitTicket<'_>,
    me: u64,
    budget: Option<Duration>,
) -> Result<bool, GuardCycle> {
    let key = ticket.key;
    let deadline = budget.map(|d| std::time::Instant::now() + d);
    let (mtx, cv) = guard_registry();
    let mut g = mtx.lock().unwrap_or_else(|e| e.into_inner());
    key.lock.sleepers.fetch_add(1, Ordering::SeqCst);
    g.waiting.insert(me, (Arc::clone(key.lock), key.mode));
    let out = loop {
        if ticket.is_admitted() || key.lock.looks_free(key.mode) {
            break Ok(true);
        }
        if wait_for_cycle(&g, me) {
            break Err(GuardCycle::Cycle);
        }
        let left = match deadline {
            None => Duration::from_millis(50),
            Some(d) => match d.checked_duration_since(std::time::Instant::now()) {
                None => break Ok(false),
                Some(l) => l.min(Duration::from_millis(50)),
            },
        };
        g = cv
            .wait_timeout(g, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    };
    g.waiting.remove(&me);
    key.lock.sleepers.fetch_sub(1, Ordering::SeqCst);
    out
}

/// TICKET-063 — a registered guard wait (`PartyWait::Guard`): the box's lock, the mode, and
/// `taking`, which the waiter sets before each take attempt and clears when the attempt fails. A
/// waiter that has just taken the guard stays registered until it leaves its wait loop, and its own
/// hold is no wait: `taking` says so (a lock word cannot name an anonymous reader).
#[derive(Debug, Clone)]
pub struct GuardWait {
    pub lock: Arc<BoxLock>,
    pub mode: GuardMode,
    pub taking: Arc<AtomicBool>,
    /// A reader's admission flag ([`WaitTicket::admission`]): an admitted reader holds the read.
    pub admitted: Option<Arc<AtomicBool>>,
}

/// TICKET-063 — would an acquire of `w` by `me`, issued now, NOT block? `true` too when the wait
/// is a cycle (the acquire faults at once), and while the waiter takes the guard. Deliberately
/// generous: a false `true` only vetoes the verdict.
pub fn guard_wait_satisfiable(w: &GuardWait, me: u64) -> bool {
    if w.taking.load(Ordering::SeqCst)
        || w.admitted
            .as_ref()
            .is_some_and(|f| f.load(Ordering::SeqCst))
        || w.lock.looks_free(w.mode)
        || w.lock.writer() == me
    {
        return true;
    }
    let (mtx, _cv) = guard_registry();
    let g = mtx.lock().unwrap_or_else(|e| e.into_inner());
    wait_for_cycle(&g, me)
}

/// Walk `waiting` -> blockers -> `waiting` -> ... from `start`, each task once: a walk that reaches
/// `start` again is a wait-for cycle.
fn wait_for_cycle(g: &GuardGraph, start: u64) -> bool {
    let mut seen = std::collections::HashSet::new();
    let mut work = vec![start];
    let mut next = Vec::new();
    while let Some(cur) = work.pop() {
        let Some((lock, mode)) = g.waiting.get(&cur) else {
            continue;
        };
        next.clear();
        g.blockers(lock, *mode, cur, &mut next);
        for &t in &next {
            if t == start {
                return true;
            }
            if seen.insert(t) {
                work.push(t);
            }
        }
    }
    false
}

/// Test-only blocking acquire: the fast path, then a [`WaitTicket`] wait of at most `budget`.
/// The engine path is `Vm::take_update_guard` (re-entry checks, halts, width permit, parties).
#[cfg(test)]
pub fn acquire_update_guard_within<'a>(
    key: GuardKey<'a>,
    me: u64,
    budget: Duration,
) -> Result<Option<UpdateGuard<'a>>, GuardCycle> {
    if let Some(g) = try_update_guard(key, me) {
        return Ok(Some(g));
    }
    let deadline = std::time::Instant::now() + budget;
    let mut ticket = WaitTicket::new(key);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if await_update_guard_free(&ticket, me, Some(left))?
            && let Some(g) = ticket.try_take(me)
        {
            return Ok(Some(g));
        }
        if left.is_zero() {
            return Ok(None);
        }
    }
}

/// `Channel[T]` core (B3.1): the shared mailbox, a FIFO of values (M3.2). `send` locks +
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
    v: Value,
}

/// A blocked rendezvous receiver's place (TICKET-185): a sender that finds it commits it and
/// stores the value in `got`, where the receiver takes it on its next run.
#[derive(Debug)]
struct Slot {
    p: Arc<Pending>,
    arm: u32,
    got: Option<Value>,
}

/// TICKET-194 — what a `recv` on a channel gets WITHOUT waiting (`Vm::recv_ready`).
pub enum RecvReady {
    /// A buffered value or a live sender's offer, taken.
    Value(Value),
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

/// The locked interior of a [`ChannelCore`]: the buffer, the offers, the slots and the `closed`
/// flag. M3.2: every message is a [`Value`] of the run's one heap. The collector reads them all
/// under this lock (`Heap::children`, [`iter`](Self::iter)); no thread holds the lock across a
/// safepoint, a GC door or a user call, so a collection that waits for it never waits for a
/// stopped thread.
#[derive(Debug, Default)]
pub struct ChanState {
    queue: VecDeque<Value>,
    /// TICKET-185 — offers of blocked senders (a cap-0 channel, or a FULL bounded one).
    sendq: VecDeque<Offer>,
    /// TICKET-185 — slots of blocked cap-0 receivers.
    recvq: VecDeque<Slot>,
    pub closed: bool,
    /// TICKET-219 — set by [`seal`](Self::seal): the last buffered value is the handle's outcome.
    sealed: bool,
    /// M3.2 — the `Err` text of a Task outcome sealed by a party with no heap at hand (the job
    /// transition under the sched lock, [`seal_err`](Self::seal_err)). The first receive builds
    /// it into the sealed value (`Vm::recv_ready`), so every reader then shares one object.
    unbuilt: Option<Box<str>>,
}

impl ChanState {
    /// Buffer a message.
    pub fn push(&mut self, v: Value) {
        self.queue.push_back(v);
    }

    /// TICKET-219 — write a handle job's one outcome: buffer `v` as the last value and close the
    /// channel. The first writer wins under `core.q`: a second seal returns `false` and changes
    /// nothing. Every receive of a sealed channel reads `v` and takes nothing
    /// ([`pop_for`](Self::pop_for)), so `len()` stays 1 and every reader shares the one outcome.
    pub fn seal(&mut self, v: Value) -> bool {
        if self.sealed {
            return false;
        }
        self.queue.push_back(v);
        self.sealed = true;
        self.closed = true;
        true
    }

    /// [`seal`](Self::seal) with the outcome `Err(msg)`, written as text: the caller holds no
    /// heap. The first receive builds the value ([`build_outcome`](Self::build_outcome)).
    pub fn seal_err(&mut self, msg: &str) -> bool {
        if self.sealed {
            return false;
        }
        self.unbuilt = Some(msg.into());
        self.sealed = true;
        self.closed = true;
        true
    }

    /// The `Err` text of a sealed outcome that no receive built yet.
    pub fn unbuilt_outcome(&self) -> Option<&str> {
        self.unbuilt.as_deref()
    }

    /// Store `v`, built from [`unbuilt_outcome`](Self::unbuilt_outcome), as the sealed outcome.
    pub fn build_outcome(&mut self, v: Value) {
        if self.unbuilt.take().is_some() {
            self.queue.push_back(v);
        }
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed
    }

    /// Publish a blocked sender's value as `p`'s offer on `arm`.
    pub fn offer(&mut self, p: &Arc<Pending>, arm: u32, v: Value) {
        self.sendq.push_back(Offer {
            p: Arc::clone(p),
            arm,
            v,
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

    /// Hand `v` to the first live, unfilled slot that is not `me`'s, committing it. Dead slots are
    /// dropped on the way. `false` when no such slot exists.
    pub fn give(&mut self, v: Value, me: Option<&Arc<Pending>>) -> bool {
        let mut i = 0;
        while i < self.recvq.len() {
            let s = &self.recvq[i];
            if s.got.is_some() || is_me(&s.p, me) {
                i += 1;
                continue;
            }
            if s.p.try_commit(s.arm) {
                self.recvq[i].got = Some(v);
                return true;
            }
            self.recvq.remove(i);
        }
        false
    }

    /// THE send decision (TICKET-185), made in one `core.q` hold. In order: closed; unbounded
    /// buffers; rendezvous gives to a slot; bounded buffers while there is room; otherwise the
    /// value becomes `offer`'s offer, or the send is [`SendOutcome::Full`].
    pub fn send(
        &mut self,
        cap: Option<usize>,
        v: Value,
        offer: Option<(&Arc<Pending>, u32)>,
    ) -> SendOutcome {
        if self.closed {
            return SendOutcome::Closed;
        }
        match cap {
            None => {
                self.push(v);
                return SendOutcome::Sent;
            }
            Some(0) => {
                if self.give(v, offer.map(|(p, _)| p)) {
                    return SendOutcome::Sent;
                }
            }
            Some(n) if self.queue.len() < n => {
                self.push(v);
                return SendOutcome::Sent;
            }
            Some(_) => {}
        }
        match offer {
            Some((p, arm)) => {
                self.offer(p, arm, v);
                SendOutcome::Offered
            }
            None => SendOutcome::Full,
        }
    }

    /// Take the next value: the buffer front (then move the first live offer into the freed
    /// place — Go's recv refill), else the first live offer that is not `me`'s. Each offer taken
    /// is committed by CAS; dead offers are dropped on the way.
    pub fn pop_for(&mut self, me: Option<&Arc<Pending>>) -> Option<Value> {
        if self.sealed {
            return self.queue.back().copied();
        }
        if let Some(v) = self.queue.pop_front() {
            if let Some(ov) = self.take_offer(me) {
                self.queue.push_back(ov);
            }
            return Some(v);
        }
        self.take_offer(me)
    }

    /// Commit and remove the first live offer that is not `me`'s; dead offers are dropped.
    fn take_offer(&mut self, me: Option<&Arc<Pending>>) -> Option<Value> {
        let mut i = 0;
        while i < self.sendq.len() {
            if is_me(&self.sendq[i].p, me) {
                i += 1;
                continue;
            }
            let o = self.sendq.remove(i).unwrap();
            if o.p.try_commit(o.arm) {
                return Some(o.v);
            }
        }
        None
    }

    pub fn pop(&mut self) -> Option<Value> {
        self.pop_for(None)
    }

    /// Remove `p`'s filled slot and return its value.
    pub fn take(&mut self, p: &Arc<Pending>) -> Option<Value> {
        let i = self
            .recvq
            .iter()
            .position(|s| s.got.is_some() && Arc::ptr_eq(&s.p, p))?;
        self.recvq.remove(i)?.got
    }

    /// Remove every offer and slot of `p`.
    pub fn withdraw(&mut self, p: &Arc<Pending>) {
        self.sendq.retain(|o| !Arc::ptr_eq(&o.p, p));
        self.recvq.retain(|s| !Arc::ptr_eq(&s.p, p));
    }

    /// `close()` (TICKET-185): in this one hold, set `closed`, close every offer (each such sender
    /// faults `send on a closed channel`) and drop the unfilled slots. A filled slot stays: its
    /// value was delivered before the close.
    pub fn close(&mut self) {
        self.closed = true;
        for o in self.sendq.drain(..) {
            o.p.try_close();
        }
        self.recvq.retain(|s| s.got.is_some());
    }

    /// The buffered message count — Go's `len`: offers and slots never count.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// The buffer is empty — like [`len`](Self::len), offers and slots never count.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Every value held here — buffer, offers and filled slots — for the collector.
    pub fn iter(&self) -> impl Iterator<Item = Value> + '_ {
        self.queue
            .iter()
            .copied()
            .chain(self.sendq.iter().map(|o| o.v))
            .chain(self.recvq.iter().filter_map(|s| s.got))
    }

    /// The bytes this state holds beside the heap objects of its values: one word per value.
    pub fn held_bytes(&self) -> usize {
        (self.queue.capacity() + self.sendq.len() + self.recvq.len()) * std::mem::size_of::<Value>()
    }

    /// Would a `recv` by `me` take a value now? A buffered value, a sealed outcome, or a live
    /// offer of another party.
    pub fn recv_ready_for(&self, me: Option<&Arc<Pending>>) -> bool {
        !self.queue.is_empty()
            || self.sealed
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
    Got(u32, Value),
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

/// `Shared[T]` core: the one box every task reaches (Go `struct { mu sync.Mutex; v T }`). M3.3: it
/// holds ONE [`Value`] of the run's one heap, by handle, in an atomic slot. Every method takes the
/// box's guard ([`acquire_update_guard`], the lock) and then works on the live value in the
/// caller's task: `get` returns the value itself, `set` stores one word, and the closure of `update`
/// / `read` runs on the live value while its task holds the guard. A same-box re-entry inside that
/// closure is the guard's length-1 cycle and FAULTS; a cross-task contender waits; a cross-box
/// wait-for cycle (AB-BA) faults.
///
/// The slot is read with no lock (Acquire), so the collector and a `&self` render never wait for a
/// holder; a write stores with Release. No std lock is held across user code. The guard is the
/// box's [`BoxLock`]: one CAS when free.
#[derive(Debug)]
pub struct SharedCore {
    pub v: super::rvec::RCell<Value>,
    /// The box's lock ([`BoxLock`]): an `Arc` so a slow-path waiter can name it in the registry.
    pub lock: Arc<BoxLock>,
}

impl SharedCore {
    pub fn new(v: Value) -> Self {
        SharedCore {
            v: super::rvec::RCell::new(v),
            lock: Arc::new(BoxLock::new("Shared")),
        }
    }
}

impl Default for SharedCore {
    fn default() -> Self {
        SharedCore::new(Value::nil())
    }
}

/// `RwShared[T]` core: the read-write counterpart to [`SharedCore`] (Go `sync.RWMutex`). The same
/// one slot; the guard has two modes ([`GuardMode`]): MANY readers (`get`, `read`, the `_` read
/// views) hold it at once, or ONE writer (`write`, `set`, the single-entry writers). A writer that
/// waits keeps new readers out (Go writer preference); a nested read by a reader is granted unless a
/// writer waits (then it faults, a certain cycle). A write inside the same task's read is a certain
/// deadlock and faults (`ReadHeld`).
#[derive(Debug)]
pub struct RwSharedCore {
    pub v: super::rvec::RCell<Value>,
    /// The box's lock ([`BoxLock`]): an `Arc` so a slow-path waiter can name it in the registry.
    pub lock: Arc<BoxLock>,
}

impl RwSharedCore {
    pub fn new(v: Value) -> Self {
        RwSharedCore {
            v: super::rvec::RCell::new(v),
            lock: Arc::new(BoxLock::new("RwShared")),
        }
    }
}

impl Default for RwSharedCore {
    fn default() -> Self {
        RwSharedCore::new(Value::nil())
    }
}

/// `Atomic[T]` core: the cross-task atomic box. One [`Value`] of the run's one heap behind a
/// `Mutex`, reachable across threads via the `Arc` handle, with atomic-operation methods —
/// `load`/`store`/`exchange`/`cas` and (numeric `T`) `add`/`sub`. Each method is a single
/// lock-op-unlock, so the read-modify-write of `add`/`sub`/`exchange`/`cas` is atomic across threads
/// without a separate `update_lock` (no user closure runs under the lock, unlike `Shared.update`).
/// M3.2: a store copies one word (Go `atomic.Value`); the collector reads `v` under the lock.
#[derive(Debug)]
pub struct AtomicCore {
    pub v: Mutex<Value>,
}

impl AtomicCore {
    pub fn new(v: Value) -> Self {
        AtomicCore { v: Mutex::new(v) }
    }

    /// Lock the value. No thread holds it across a safepoint, a GC door or a user call.
    pub fn lock(&self) -> std::sync::MutexGuard<'_, Value> {
        self.v.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn load(&self) -> Value {
        *self.lock()
    }

    pub fn store(&self, v: Value) {
        *self.lock() = v;
    }
}

/// `AtomicInt` core: the monomorphic, LOCK-FREE int atomic (Rust `AtomicI64` / Java `AtomicInteger` /
/// Go `atomic.Int64` style). Unlike [`AtomicCore`] (a `Mutex<Value>` holding an arbitrary
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
    /// The fd is a FIFO, a pipe or a device: a read can wait without end for its writer, so it
    /// runs as a `HostFd` wait (`WaitSpec::of_fd`). Fixed at the open.
    pub waits: bool,
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
/// `key` is a stable identity (like `SocketCore.key`); there is NO netpoller registration (file
/// writes are synchronous blocking syscalls — no park).
#[derive(Debug)]
pub struct WriterCore {
    pub inner: Mutex<Option<Backing>>,
    pub key: usize,
    /// The fd is a FIFO, a pipe or a device: a write can wait without end for its reader, so it
    /// runs as a `HostFd` wait (`WaitSpec::of_fd`). Fixed at the open; `false` for a console or an
    /// `io.buffered` core, which reach their fd through an inner core.
    pub waits: bool,
}

/// R2 — where a [`WriterCore`] sends bytes.
/// * `File` — a `create`/`append` file writer. M5c: unbuffered, as Go's `os.File`: each write is a
///   `write(2)`, so an unclosed file writer holds no bytes to lose.
/// * `Stdout`/`Stderr` — markers: a write ROUTES through [`Vm::emit_out`]/[`Vm::emit_err`] (the
///   captured `Vm.out` buffer / the streaming-CLI sink), NEVER a raw fd — else capture/streaming break.
/// * `Buffered` — the Go `bufio.NewWriter` escape hatch: accumulate in `buf`, drain to `inner` on
///   flush / buffer-full / close. M5d: a dropped `Buffered` core discards its tail, as Go's
///   `bufio.Writer` does (no finalizer flushes it), so no drop of a writer writes to an fd.
#[derive(Debug)]
pub enum Backing {
    File(std::fs::File),
    Stdout,
    Stderr,
    Buffered {
        inner: Arc<WriterCore>,
        buf: Vec<u8>,
        cap: usize,
    },
}

/// The mutable inside of an [`ExecutorCore`]: the shut flag. `submit` starts every job at once
/// (decision D3), so the Executor holds no queue of its own.
#[derive(Debug, Default)]
pub struct ExecState {
    pub shut: bool,
}

/// W7-26 — the bytes one finished job's [`TaskOutcome`](super::TaskOutcome) holds outside every
/// heap: two buffered-output `Vec<u8>`s (W7-5c flushes them at the slot's task-order position, so
/// they are retained until `shutdown`). A job's return value is no part of it: `_run_outcome`
/// seals it into the Task channel, a heap value (M3.2).
///
/// Charged UNCONDITIONALLY: both ancestors keep accounting live and the *limit* separate (Go's
/// `runtime.MemStats` vs `GOMEMLIMIT`). Called OFF the `eager` lock (see the Executor's
/// `sched::finish`).
pub(super) fn outcome_summary(o: &super::TaskOutcome) -> usize {
    use super::TaskOutcome as T;
    let (out, stderr) = match o {
        T::Done(r) => (&r.out, &r.stderr),
        T::Cancelled { out, stderr }
        | T::CancelledFault { out, stderr, .. }
        | T::Exit { out, stderr, .. }
        | T::Fault { out, stderr, .. }
        | T::Deadlocked { out, stderr, .. } => (out, stderr),
    };
    std::mem::size_of::<super::TaskOutcome>() + out.capacity() + stderr.capacity()
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
        other @ T::CancelledFault { .. }
        | other @ T::Exit { .. }
        | other @ T::Fault { .. }
        | other @ T::Deadlocked { .. } => (other, false),
    }
}

/// `Executor` core (B3.1 / C5 escape hatch): the explicitly-owned work queue. `submit` runs EAGERLY
/// (the job goes straight to the pool, matching Python's `ThreadPoolExecutor` / Java's
/// `ExecutorService`); the pending work lives in its scope's sched.
/// `shut` lives in the **shared** core, so any handle on this core sees the same shutdown state.
#[derive(Default)]
pub struct ExecutorCore {
    /// At most this many jobs run at once; zero means no cap; set once at construction.
    pub limit: usize,
    /// TICKET-213 — runner threads this Executor has started, over its whole life. Observability
    /// only (the runner-reuse test reads it); no scheduling decision reads it.
    pub(super) runner_starts: AtomicUsize,
    pub inner: Mutex<ExecState>,
    /// TICKET-208 — the detached scope this Executor's jobs run in: a sched built at the first
    /// `submit` and taken by the join that reduces its slots. The Executor's tasks, their results
    /// and their byte charge live in that sched and nowhere else.
    pub(super) scope: Mutex<Option<super::EagerScope>>,
    /// The cooperative cancel flag shared by every job this executor has dispatched. Per-CORE, not
    /// per-drain (the pre-eager model had no running jobs to cancel): `cancel` trips it so
    /// already-started jobs die at their next back-edge (decision D4 — "attempts to stop",
    /// cooperative, not preemptive), and a hard halt inside a job trips it via `run_outcome`.
    pub cancel: Arc<AtomicBool>,
    /// The cancel-flag chain of the job that CREATED this executor (`Vm::scope_ancestors()` captured at
    /// `Op::NewExecutor`), empty for an executor created by `main` or by a `parallel:`/`spawn` fiber.
    /// Every job dispatched from this core inherits it as its `cancel_outer`, so an outer
    /// `cancel()` reaches a nested executor's jobs (W7-39, `docs/reference/concurrency/` §Executor).
    ///
    /// **Keyed on the CREATOR, never on the submitter.** Any task can hold an `Executor`
    /// handle, so `submit` can be reached from a job of an entirely unrelated executor; keying on the
    /// submitter made *that* executor's `cancel()` kill a job belonging to `main`'s executor, and
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
        self.sched().map_or(0, |s| s.lock().undone_tasks())
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
/// so it is swapped per fiber (`swap_ctx`, under `owns_side`) and an executor created INSIDE an M:N task
/// lands in that task's throwaway worker list, which is dropped when the task finishes. The top-level
/// join therefore never saw it, and its work was silently lost. An `ExecutorCore` lives OUTSIDE every
/// heap (B3.1), so a list of `Arc`s is heap-independent by construction: `spawn_worker` hands the SAME
/// list to every worker, and an executor created anywhere in the run is visible to the one join.
/// That is why closing W7-5b needs no change to `swap_ctx`'s `owns_side` gate — the change that a
/// previous attempt stopped at because it drags in GC rooting for a parked parent ctx.
///
/// Strong `Arc`s, not `Weak`: the whole point is to join work whose creating heap is already gone, so
/// the core must outlive its `Obj::Executor` handle. Entries are never pruned — same shape (and same
/// bound: one small struct per `Executor` the program constructs) as `Vm.executors`, whose
/// "reap only those alive at exit" snapshot has always been push-only too.
pub type ExecRegistry = Arc<Mutex<Vec<Arc<ExecutorCore>>>>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    /// TICKET-219 / M3.2 — a sealed channel is a handle's one outcome record: every receive reads
    /// the one value and takes nothing, `len()` stays 1, the first seal wins, and a `send` faults
    /// closed. A text seal stays unbuilt until a receive builds it; then it is that one value.
    #[test]
    fn sealed_channel_receive_shares_and_refuses_a_second_seal() {
        let mut q = ChanState::default();
        assert!(q.seal(Value::int(7)));
        assert!(q.is_sealed());
        assert_eq!(q.pop(), Some(Value::int(7)));
        assert_eq!(q.pop(), Some(Value::int(7)));
        assert_eq!(q.len(), 1);
        assert!(q.recv_ready_for(None));
        assert!(!q.seal(Value::int(9)));
        assert!(!q.seal_err("late"));
        assert_eq!(q.pop(), Some(Value::int(7)));
        assert!(matches!(
            q.send(Some(1), Value::int(1), None),
            SendOutcome::Closed
        ));

        let mut e = ChanState::default();
        assert!(e.seal_err("task cancelled"));
        assert!(e.recv_ready_for(None), "an unbuilt outcome is ready");
        assert_eq!(e.pop(), None);
        assert_eq!(e.unbuilt_outcome(), Some("task cancelled"));
        assert!(!e.seal(Value::int(1)), "the text seal won");
        e.build_outcome(Value::int(3));
        e.build_outcome(Value::int(4));
        assert_eq!(e.unbuilt_outcome(), None);
        assert_eq!(e.pop(), Some(Value::int(3)));
        assert_eq!(e.len(), 1);
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
}
