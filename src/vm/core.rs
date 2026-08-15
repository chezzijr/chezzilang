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
use std::collections::VecDeque;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};

/// `Channel[T]` core (B3.1): the shared mailbox, a FIFO of wire-form messages. `send` locks +
/// `push_back`; `recv`/`try_recv` lock + `pop_front`; `len` locks + len. `cap` is `None` for an
/// unbounded `Channel[T]()` (the default — `send` never blocks) and `Some(n)` for a bounded
/// `Channel[T](n)`: once `n` messages are queued a `send` BLOCKS/parks until a `recv` frees a slot
/// (backpressure), and `try_send` returns `false`. A freed slot wakes parked senders exactly as a
/// `send` wakes parked receivers (the not-full waiter set mirrors the not-empty one).
///
/// B3.3-threads: `cv` is the real-OS-thread blocking primitive. A `recv` on an empty queue waits on
/// `cv` (paired with `q`'s `Mutex`); a `send` `notify_all`s it after pushing. `cv` is **dead on the
/// cooperative default engine** — there a `recv` parks the *fiber* (it never touches `cv`), so
/// single-thread runs never wait on it. On the M:N engine a normal empty `recv` snapshot-parks the
/// fiber (still no `cv`), BUT **D5 owe #3 Path C revives `cv`**: a `recv` reached inside a native
/// callback can't snapshot-park, so the worker thread DEMOTES — it blocks in place on `cv` and resumes
/// when a sibling `send` `notify_all`s it (`MnSched::send_wake` + the non-mn `send`). The wait loop
/// re-checks the queue / cancel / terminate on every wake (spurious-wakeup-safe; bounded poll).
#[derive(Debug, Default)]
pub struct ChannelCore {
    pub q: Mutex<ChanState>,
    pub cv: Condvar,
    /// Bounded-channel capacity: `None` = unbounded (`send` never blocks); `Some(n>0)` = a bounded
    /// FIFO whose `send` parks the fiber once `n` messages are queued and whose `try_send` returns
    /// `false` when full. Immutable after construction (set once by `Op::NewChannel`).
    pub cap: Option<usize>,
    /// `timer(ms)` timeout channel: `Some(deadline)` iff this channel was built by `timer`. It is
    /// **level-triggered** — `recv` yields `true` on any call at/after the deadline (the typical use
    /// recvs it once, in a `wait` arm). Delivery is handled at `recv` time in the receiver's own
    /// scheduler: `--parallel` schedules a background `send(true)` + parks; cooperative VM / interp /
    /// callbacks inline-sleep to the deadline and synthesise `true`. `None` for an ordinary `Channel[T]`.
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
}

/// The locked interior of a [`ChannelCore`]: the message FIFO plus a `closed` flag. Folding `closed`
/// into the *same* mutex as the queue is deliberate — every park decision ([`super::Vm::park`],
/// `send_wake`, the recv arm, the demote loop) re-checks the queue under this lock, so "a value is
/// waiting OR the channel is closed" is one atomic observation. A separate `AtomicBool` would leave a
/// TOCTOU gap (check empty, then close happens, then park) that could strand a parked fiber. Once
/// `closed`: `send`/`try_send` are rejected, `recv` drains then faults, and `for v in ch:` ends once
/// drained. `close()` wakes every parked/demoted receiver via `cv` + the scheduler.
#[derive(Debug, Default)]
pub struct ChanState {
    /// PRIVATE on purpose (W6-7/W6-10): every mutation must go through [`push`](Self::push) /
    /// [`pop`](Self::pop) / [`clear`](Self::clear) so the cached GC summary (`bytes`/`dirty`) can
    /// never go stale. A stale `dirty == false` would stop the GC tracing a live handle queued in
    /// this channel — a use-after-free. Rust module privacy is what makes "did I catch every push
    /// site?" a compile error instead of a code review.
    ///
    /// Each message carries its own [`wire_summary`] byte count so `pop` is O(1): these queues are
    /// popped under the GLOBAL `MnSched` lock (`sched.rs` demote paths), and re-deriving the count
    /// on removal would put an O(payload) walk in that critical section.
    queue: VecDeque<(usize, WireValue)>,
    /// Approximate owned bytes of the queued messages (see [`wire_summary`]) — the off-heap storage
    /// `Heap::live_bytes` could not see before W6-10.
    bytes: usize,
    /// True while ANY queued message can root a heap object (a `Handle` or a nested core). Cleared
    /// only when the queue empties, so it is conservative (over-walk = safe) and self-healing.
    dirty: bool,
    pub closed: bool,
}

impl ChanState {
    /// Enqueue a message with its PRE-COMPUTED [`wire_summary`].
    ///
    /// The summary MUST be computed by the caller **before taking any lock**: `send_wake` /
    /// `send_wake_bounded` hold `MnSched::core` — the process-wide lock that serializes every
    /// fiber's park/wake/finish — across this call, and `wire_summary` is O(payload). Global-lock
    /// hold time must not scale with user payload size.
    pub fn push(&mut self, sum: (usize, bool), w: WireValue) {
        self.bytes += sum.0;
        self.dirty |= sum.1;
        self.queue.push_back((sum.0, w));
    }

    pub fn pop(&mut self) -> Option<WireValue> {
        let (b, w) = self.queue.pop_front()?;
        self.bytes = self.bytes.saturating_sub(b);
        if self.queue.is_empty() {
            self.bytes = 0;
            self.dirty = false;
        }
        Some(w)
    }

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

    /// Cached GC summary of the queued messages: `(approximate owned bytes, can-root-a-heap-object)`.
    pub fn summary(&self) -> (usize, bool) {
        (self.bytes, self.dirty)
    }
}

/// `Shared[T]` core (B3.1): the one box every task reaches. `get` locks + clones out; `set` locks +
/// overwrites; `update` reads out (value lock dropped so the closure can re-enter), runs the user fn,
/// then writes back.
///
/// B3.3-threads: `update`'s read-modify-write must be **atomic across threads** — that is the entire
/// promise of `Shared[T]` ("the single owner serialises writes, so the torn-write race is
/// unrepresentable"). The value lock `v` cannot be held across the user closure (it would deadlock a
/// closure that re-enters `get`/`set` on the same box — `Mutex` is not reentrant), so a **separate**
/// `update_lock` serialises whole updates: held for the entire RMW *only under `--parallel`*, while
/// `v` is still locked only for the brief read and the brief write-back. The cooperative engine
/// never takes `update_lock` (single-thread; taking it would needlessly deadlock a same-box nested
/// update that merely lost-updated before).
#[derive(Debug, Default)]
pub struct SharedCore {
    pub v: Mutex<WireValue>,
    pub update_lock: Mutex<()>,
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
/// SHARED read guard, clones the value out, drops the guard, then runs `f` (no write-back). `write(f)`
/// (and `set`) take the EXCLUSIVE write guard.
///
/// `write`'s read-modify-write must be **atomic across threads** (the box's contract, exactly like
/// `Shared.update`). The value lock `v` cannot be held across the user closure (a `RwLock` write guard
/// is not reentrant — it would deadlock a closure that re-enters `get`/`set`/`read` on the same box),
/// so a **separate** `update_lock` serialises whole writes: held for the entire RMW *only under
/// `--parallel`*, while `v` is taken only for the brief read-out and the brief write-back. The
/// `RwLock` alone is NOT enough — because the write guard is dropped across the closure, two
/// concurrent `write`s could otherwise clone the same base value and lose an update. The cooperative
/// engine never takes `update_lock` (single-thread; it would needlessly deadlock a same-box nested
/// write). A `--parallel` closure that re-enters `write` on the SAME box still deadlocks (a documented
/// edge, mirroring `Shared.update`); a write-inside-a-read on the same box likewise deadlocks.
#[derive(Debug, Default)]
pub struct RwSharedCore {
    pub v: RwLock<WireValue>,
    pub update_lock: Mutex<()>,
    /// W6-7/W6-10 — cached GC summary of `v`. MUST be re-`set` under `v`'s write lock by every store.
    pub summary: WireSummary,
}

impl RwSharedCore {
    /// Replace the payload AND refresh the cached GC summary under the same write lock — see
    /// [`SharedCore::store`] (the walk is hoisted OFF the exclusive lock for the same reason).
    pub fn store(&self, w: WireValue) {
        let sum = wire_summary(&w);
        let mut g = self.v.write().unwrap();
        self.summary.store(sum.0, sum.1);
        *g = w;
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
/// Mutex gave, so serial == M:N stays byte-identical. `add`/`sub` use a CHECKED compare_exchange CAS-loop
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
}

/// D6 — a fresh, not-yet-parked in-flight flag for a new socket/listener core.
pub fn new_in_flight() -> Arc<AtomicBool> {
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
/// * `Stdout`/`Stderr` — markers: a write ROUTES through [`Vm::emit_out`]/[`Vm::emit_err`] (the parity
///   oracle `Vm.out` / the streaming-CLI sink), NEVER a raw fd — else capture/parity/streaming break.
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

/// The eager (M:N) half of an [`ExecutorCore`]. Eager dispatch is the only path: a job starts at its
/// `submit` and `shutdown` is purely the join.
///
/// `slots` is indexed by SUBMISSION ORDER, which is the whole reason eager execution keeps the W7-5
/// fault contract for free: `shutdown` hands this vector straight to `Vm::reduce_task_slots`, so
/// lowest-index-fault selection, hard-halt-over-ordinary precedence and the per-slot output flush
/// (W7-5c) are inherited rather than re-implemented.
///
/// **What that flush does and does NOT order.** It governs the BUFFERED stdout sink only — the
/// `out`/`stderr` a job's worker `Vm` accumulates when [`HostConfig::stream`] is off, which is what
/// every test helper and every embedder gets. `chezzi run` sets `stream`, and a streamed `print`
/// goes to the real fd at the moment it runs (line-atomic, never withheld — the D5 invariant in
/// [`Vm::emit_out`]); a job's slot buffers are then EMPTY and this flush reorders nothing. So under
/// `chezzi run` an `Executor`'s jobs interleave their output in COMPLETION order, with no
/// submission-order guarantee — exactly like the `parallel:` nursery, and exactly like the ancestor
/// (`ThreadPoolExecutor`, CPython 3.14.6, three jobs each doing real work: measured 0/30 runs in
/// submission order; three jobs that only `print`: 30/30, because they are too short to overlap).
/// Do not read the submission-order slot indexing as a promise about interleaved live output — it is
/// a promise about WHICH fault wins and about the buffered sink's byte order.
#[derive(Debug, Default)]
pub struct EagerState {
    /// Submitted-but-not-yet-finished jobs. `shutdown` waits for this to reach 0.
    outstanding: usize,
    /// One slot per `submit`, in submission order; `None` until that job finishes.
    /// PRIVATE on purpose — see [`ChanState::queue`]: the cached `(bytes, dirty)` summary below is
    /// only trustworthy while `finish`/`take_slots` are the sole ways to change this vector.
    slots: Vec<Option<super::TaskOutcome>>,
    /// W7-26 — cached `(bytes, dirty)` of the collected outcomes, the shape `ChanState`/`ExecState`
    /// already carry. Without it a finished job's result was reachable by `Heap::live_bytes`
    /// NOWHERE: `--max-heap` reads the executor core, and the only half it could read was `inner`,
    /// the `--serial` queue — which the eager (default M:N) engine leaves empty forever.
    bytes: usize,
    dirty: bool,
    /// How much of `bytes` has already been reported to a submitting heap's GC pacing counter — see
    /// [`take_charge`](EagerState::take_charge).
    charged: usize,
}

impl EagerState {
    /// Claim the next submission-order slot. Called under the core lock at `submit`, BEFORE the job
    /// is handed to the pool, so slot order is submission order even when jobs finish out of order.
    pub(super) fn reserve(&mut self) -> usize {
        self.outstanding += 1;
        self.slots.push(None);
        self.slots.len() - 1
    }

    /// Record a finished job's outcome with its PRE-COMPUTED [`outcome_summary`] — see
    /// [`ChanState::push`], and hoisted for the same reason `SharedCore::store` hoists its walk: the
    /// summary is a recursive O(result) walk, and this lock is contended by every `submit`
    /// (`dispatch_eager_job`'s `reserve`, taken while the submitter holds `inner`) and by every
    /// `live_bytes`. Lock hold time must not scale with user payload size. The caller must
    /// `notify_all` the core's `eager_cv` after dropping the guard so a waiting `shutdown` re-checks.
    pub(super) fn finish(&mut self, idx: usize, sum: (usize, bool), outcome: super::TaskOutcome) {
        // `take_slots` empties the vector, which would invalidate a live job's index. It only ever runs
        // at `outstanding == 0` (the join waits for that first) and `submit` reserves under the `shut`
        // check, so no job can be holding a stale index here. Asserted rather than defended: the
        // failure mode is a panic on a pool thread AFTER its `catch_unwind`, which would leave
        // `outstanding` short and hang `shutdown` forever — worth catching in tests, not papering over.
        debug_assert!(
            idx < self.slots.len(),
            "eager slot {idx} was taken while a job was still outstanding"
        );
        self.bytes += sum.0;
        self.dirty |= sum.1;
        self.slots[idx] = Some(outcome);
        self.outstanding -= 1;
    }

    /// Take the collected outcomes, leaving the slot vector empty. A second `shutdown` therefore
    /// reduces an empty vector — a clean no-op, matching the serial engine's drained queue.
    pub(super) fn take_slots(&mut self) -> Vec<Option<super::TaskOutcome>> {
        self.bytes = 0;
        self.dirty = false;
        self.charged = 0;
        std::mem::take(&mut self.slots)
    }

    /// W7-60 — take the outcomes of the jobs that have ALREADY finished, leaving the slot vector at
    /// its current LENGTH (each taken slot becomes `None` again). For the bail-out paths in
    /// [`super::Vm::join_eager_jobs`], which unwind while other jobs are still outstanding: those jobs
    /// hold indices into this vector and will `finish` into them, so [`take_slots`](Self::take_slots)'s
    /// `mem::take` is not available — but the jobs that DID finish still own buffered output, and
    /// dropping it is a silent loss (a `print` that ran, completed, and never reached stdout).
    ///
    /// Length-preserving, so a concurrent `finish` stays in range; idempotent, so a second call
    /// returns nothing and the same bytes can never be flushed twice. The byte accounting is reset
    /// for what leaves, exactly as `take_slots` does — what remains is re-accrued by the outstanding
    /// jobs' own `finish` calls.
    pub(super) fn take_finished(&mut self) -> Vec<super::TaskOutcome> {
        let taken: Vec<_> = self.slots.iter_mut().filter_map(Option::take).collect();
        if !taken.is_empty() {
            self.bytes = 0;
            self.dirty = false;
            self.charged = 0;
        }
        taken
    }

    /// W7-26, the SAMPLING half — the growth in `bytes` since this was last called, to be charged
    /// against the SUBMITTING heap's GC pacing counter (`Heap::charge_bytes`).
    ///
    /// Counting the results is worthless if the cap is never sampled (the W6-10 review lesson):
    /// `over_cap` is only evaluated in `sweep()`, `sweep()` only runs when `should_collect()` fires,
    /// and a `for … : ex.submit(f)` loop over a job that BUILDS its own payload allocates almost
    /// nothing in the parent and wires almost nothing at submit — so the parent never swept and
    /// 300 × ~1 MB of results measured PASS at 330 MB against an 8 MB cap even with the accounting
    /// above in place. A DELTA rather than the absolute total: the pacing counter is monotonic and
    /// reset at each sweep, so charging the total again per submit would sweep on every submit.
    ///
    /// Known ceiling, and safe by the same "fails open only by under-triggering" argument the
    /// pacing counter itself carries: `charged` is ONE watermark on a core that any number of heaps
    /// can submit to (an `Executor` handle crosses by `Arc`), so with two tasks sharing an executor
    /// the growth is charged to whichever submits next, not split. Detection is mis-attributed or
    /// delayed, never lost — every heap that can reach the core still counts its FULL bytes in its
    /// own `live_bytes`; this only decides who gets swept sooner.
    pub(super) fn take_charge(&mut self) -> usize {
        let d = self.bytes.saturating_sub(self.charged);
        self.charged = self.bytes;
        d
    }

    pub fn outstanding(&self) -> usize {
        self.outstanding
    }

    /// Cached GC summary of the collected outcomes: `(approximate owned bytes, holds a nested core)`
    /// — the [`ExecState::summary`] counterpart for the eager half (W7-26).
    pub fn summary(&self) -> (usize, bool) {
        (self.bytes, self.dirty)
    }

    /// The finished jobs' return values, for the nested-core walk in
    /// [`queue_bytes_deep`] — the [`ExecState::iter`] counterpart. Only `Done` carries a value; the
    /// other outcomes own buffered output only, already in `bytes`.
    ///
    /// W7-27: in the bin build every `Done` value is `WireValue::Nil` (nothing can read a job's
    /// result, so it is dropped rather than retained), which makes this walk a `Nil` match per slot.
    /// Kept anyway — it is what the accounting would need again the day a result IS stored, and the
    /// `Nil` arm costs nothing; the unit tests store real values through `EagerState::finish`.
    pub fn values(&self) -> impl Iterator<Item = &WireValue> {
        self.slots.iter().filter_map(|s| match s {
            Some(super::TaskOutcome::Done(r)) => Some(&r.value),
            _ => None,
        })
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
/// Called OFF the `eager` lock (see [`EagerState::finish`]) — the walk is O(result).
pub(super) fn outcome_summary(o: &super::TaskOutcome) -> (usize, bool) {
    use super::TaskOutcome as T;
    let (out, stderr, value) = match o {
        T::Done(r) => (&r.out, &r.stderr, Some(&r.value)),
        T::Cancelled { out, stderr }
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
    };
    match outcome {
        T::Done(r) => (
            T::Fault {
                err,
                out: r.out,
                stderr: r.stderr,
            },
            true,
        ),
        T::Cancelled { out, stderr } => (T::Fault { err, out, stderr }, true),
        other => (other, false),
    }
}

/// `Executor` core (B3.1 / C5 escape hatch): the explicitly-owned work queue. `submit` runs EAGERLY
/// (the job goes straight to the pool, matching Python's `ThreadPoolExecutor` / Java's
/// `ExecutorService`) and [`ExecState::queue`] stays empty — the pending work lives in `eager`.
/// `shut` lives in the **shared** core, so any handle aliasing this core sees the same shutdown state
/// (this is what prevents a `from_wire`'d alias from being drained twice at program exit).
#[derive(Debug, Default)]
pub struct ExecutorCore {
    pub inner: Mutex<ExecState>,
    /// Eager (M:N) execution state. Guarded by its OWN lock, never `inner`'s: a finishing pool job
    /// touches only this one, so it can never contend with a `submit` mid-`wire_callable`.
    pub eager: Mutex<EagerState>,
    /// Signalled whenever a job finishes; `shutdown` waits on it for `outstanding == 0`.
    pub eager_cv: Condvar,
    /// W7-26r sibling — the live heap bytes of jobs DISPATCHED BUT NOT YET STARTED, i.e. the ones
    /// sitting in the process-global pool queue. `prepare_eager_job` rebuilds each submitted closure
    /// into its own worker `Vm` at submit time, so a deep queue is N fully-built worker heaps: each
    /// one comfortably under a per-heap `--max-heap`, summing to hundreds of MB that were charged to
    /// NOBODY (measured on the release binary: 300 slow jobs capturing ~1 MB each **PASS at 666 MB**
    /// against an 8 MB cap). The cap is per-heap by definition, so this needs an OWNER — and the
    /// submitter is it: the work is its own, it can still be reached only through this executor
    /// handle, and the submit loop is running bytecode, so the parent samples it normally.
    ///
    /// Added at dispatch and removed the instant the pool thread picks the job up (from then on the
    /// bytes are the worker heap's own, charged against the worker's copy of the cap), so the charge
    /// never overlaps in TIME. It cannot overlap by ALIASING either, and that took a review to get
    /// right: the measurement is `Heap::own_bytes`, which excludes `Arc`-shared core payloads — a
    /// captured `Shared`/`Channel` crosses as one shared allocation the submitter already counts, and
    /// charging it per queued job reported 60 MB against a true 3.8 MB. What is charged here is only
    /// the deep-copied plain data the submit actually added. Maintained under a live cap only — the
    /// walk is O(the new worker's slots) and would be pure cost otherwise.
    pub pending: AtomicUsize,
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
    /// This core was marked `shut` by a join that will reduce NOTHING, so its submission slots are
    /// still owed a reduce. Set by exactly one site: `Vm::join_eager_jobs`, on entry, when the
    /// joining thread is itself one of this core's jobs (`slack > 0`) and therefore may not
    /// `take_slots` — its own index is still live. Cleared by the two paths that actually discharge
    /// the debt: the `take_slots` that finally reduces the vector (always `slack == 0`), and an
    /// ORDINARY (non-self, `slack == 0`) bail-out, which flushes what finished and has no successor
    /// to promise. A SELF-join's own bail (`slack > 0`) does **not** clear it — a self-join never
    /// discharges the debt, bail or not, so clearing there would drop a mark this call did not set
    /// (an earlier self-join may have left it true, promising a later join) and leave the vector
    /// unreduced with nothing left to pick it up.
    ///
    /// Without it, `shut` was read as "already handled" and [`Vm::drain_live_executors`] skipped the
    /// core, dropping every sibling's buffered `out`/`stderr` and any fault they raised. Invisible
    /// under `chezzi run` (streamed output already reached fd 1) and a silent loss on the buffered
    /// sink, where the slot is the only copy.
    ///
    /// A dedicated flag rather than "the slot vector is non-empty": the deadlock-BAIL path also
    /// leaves a non-empty vector behind (`take_finished` is length-preserving), and re-joining a
    /// core whose join just reported a deadlock would undo the "last chance to ask" reasoning in
    /// `join_eager_jobs`. Only the self-join promises someone else will reduce, so only it marks.
    pub unreduced: AtomicBool,
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

/// B3.1 GC support — collect every `GcRef` reachable from a core's wire contents into `out`, so the
/// heap's `children()` can keep those heap objects rooted. A core's `WireValue`s can still carry
/// `Handle(GcRef)`s into the live heap (an `Executor` queues `Closure` handles — closures can't cross
/// by value until B3.3/G1; a `Channel[str]` now queues owned bytes (B3.3a), rooting nothing).
///
/// It **recurses into nested cores** (a `Channel` stored inside a `Shared`, etc.): a nested core may
/// be reachable *only* through its parent core (its own heap handle already swept), so its embedded
/// handles would dangle if we stopped at the boundary. `seen` (core identities by `Arc` pointer)
/// breaks the `Arc` reference cycles decision E warns about — a cycle is walked once, not forever.
pub fn collect_core_gcrefs(w: &WireValue, out: &mut Vec<GcRef>, seen: &mut Vec<usize>) {
    match w {
        WireValue::Handle(g) => out.push(*g),
        WireValue::List { items: xs, .. } | WireValue::Tuple { items: xs, .. } => {
            xs.iter().for_each(|x| collect_core_gcrefs(x, out, seen))
        }
        WireValue::Map { entries, .. } => entries.iter().for_each(|(_, k, v)| {
            collect_core_gcrefs(k, out, seen);
            collect_core_gcrefs(v, out, seen);
        }),
        WireValue::Set { entries, .. } => entries
            .iter()
            .for_each(|(_, e)| collect_core_gcrefs(e, out, seen)),
        WireValue::Struct { fields, .. } => fields
            .iter()
            .for_each(|(_, v)| collect_core_gcrefs(v, out, seen)),
        WireValue::Enum { payload, .. } => payload
            .iter()
            .for_each(|x| collect_core_gcrefs(x, out, seen)),
        WireValue::NewType { inner, .. } => collect_core_gcrefs(inner, out, seen),
        // A cell queued in a channel/executor roots its inner value's handles (like `NewType`).
        WireValue::Cell { inner, .. } => collect_core_gcrefs(inner, out, seen),
        // A cursor queued in a channel/executor roots its snapshot items' handles (like `List`).
        WireValue::Iter { items, .. } => {
            items.iter().for_each(|x| collect_core_gcrefs(x, out, seen))
        }
        // F3 path C: a generator queued in a channel/executor crosses by value, but its backing
        // closure or a parked slot could still embed a `Handle` into the live heap — root them while
        // the generator sits in the queue (like `Closure`/`Iter`).
        WireValue::Generator { closure, state, .. } => {
            if let Some(c) = closure {
                collect_core_gcrefs(c, out, seen);
            }
            match state {
                WireGenState::Pending(args) => {
                    args.iter().for_each(|x| collect_core_gcrefs(x, out, seen))
                }
                WireGenState::Suspended { stack, .. } => {
                    stack.iter().for_each(|x| collect_core_gcrefs(x, out, seen))
                }
                WireGenState::Done => {}
            }
        }
        WireValue::Channel(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            core.q
                .lock()
                .unwrap()
                .iter()
                .for_each(|w| collect_core_gcrefs(w, out, s))
        }),
        WireValue::Shared(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            collect_core_gcrefs(&core.v.lock().unwrap(), out, s)
        }),
        WireValue::RwShared(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            collect_core_gcrefs(&core.v.read().unwrap(), out, s)
        }),
        WireValue::Atomic(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            collect_core_gcrefs(&core.v.lock().unwrap(), out, s)
        }),
        // `AtomicInt` holds a plain i64 — no heap refs to trace (identity-only wire visit).
        WireValue::AtomicInt(_) => {}
        WireValue::Executor(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            core.inner
                .lock()
                .unwrap()
                .iter()
                .for_each(|w| collect_core_gcrefs(w, out, s))
        }),
        // B3.6: a submitted closure queued in an `Executor` crosses by value, but its captures may
        // still embed `Handle`s into the live heap (a captured `Channel[str]`'s bytes root nothing,
        // but a captured callable would) — root them while the task sits in the queue.
        WireValue::Closure { captured, .. } => captured
            .iter()
            .for_each(|(_, v)| collect_core_gcrefs(v, out, seen)),
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
                WireGenState::Pending(args) => args.iter().for_each(|x| walk(&mut acc, x)),
                WireGenState::Suspended { stack, .. } => {
                    stack.iter().for_each(|x| walk(&mut acc, x))
                }
                WireGenState::Done => {}
            }
        }
        WireValue::Closure { captured, .. } => captured.iter().for_each(|(n, v)| {
            acc.0 += n.len();
            walk(&mut acc, v);
        }),
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
pub fn nested_core_bytes(w: &WireValue, seen: &mut super::fxhash::FxHashSet<usize>) -> usize {
    let mut acc = 0usize;
    match w {
        WireValue::List { items: xs, .. } | WireValue::Tuple { items: xs, .. } => {
            for x in xs {
                acc += nested_core_bytes(x, seen);
            }
        }
        WireValue::Map { entries, .. } => {
            for (_, k, v) in entries {
                acc += nested_core_bytes(k, seen) + nested_core_bytes(v, seen);
            }
        }
        WireValue::Set { entries, .. } => {
            for (_, e) in entries {
                acc += nested_core_bytes(e, seen);
            }
        }
        WireValue::Struct { fields, .. } => {
            for (_, v) in fields {
                acc += nested_core_bytes(v, seen);
            }
        }
        WireValue::Enum { payload, .. } => {
            for x in payload {
                acc += nested_core_bytes(x, seen);
            }
        }
        WireValue::NewType { inner, .. } | WireValue::Cell { inner, .. } => {
            acc += nested_core_bytes(inner, seen)
        }
        WireValue::Iter { items, .. } => {
            for x in items {
                acc += nested_core_bytes(x, seen);
            }
        }
        WireValue::Generator { closure, state, .. } => {
            if let Some(c) = closure {
                acc += nested_core_bytes(c, seen);
            }
            match state {
                WireGenState::Pending(args) => {
                    for x in args {
                        acc += nested_core_bytes(x, seen);
                    }
                }
                WireGenState::Suspended { stack, .. } => {
                    for x in stack {
                        acc += nested_core_bytes(x, seen);
                    }
                }
                WireGenState::Done => {}
            }
        }
        WireValue::Closure { captured, .. } => {
            for (_, v) in captured {
                acc += nested_core_bytes(v, seen);
            }
        }
        // The nested cores themselves — charge each one's payload ONCE per heap, then keep
        // recursing (a core nested two deep is just as invisible as one nested once).
        WireValue::Channel(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                let g = core.q.lock().unwrap();
                acc += queue_bytes_deep(g.summary(), g.iter(), seen);
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
                let queued = {
                    let g = core.inner.lock().unwrap();
                    queue_bytes_deep(g.summary(), g.iter(), seen)
                };
                let g = core.eager.lock().unwrap_or_else(|e| e.into_inner());
                acc += queued + queue_bytes_deep(g.summary(), g.values(), seen);
            }
        }
        WireValue::Shared(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                acc += value_core_bytes_deep(&core.summary, &core.v.lock().unwrap(), seen);
            }
        }
        WireValue::RwShared(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                acc += value_core_bytes_deep(&core.summary, &core.v.read().unwrap(), seen);
            }
        }
        WireValue::Atomic(core) => {
            if seen.insert(Arc::as_ptr(core) as usize) {
                acc += value_core_bytes_deep(&core.summary, &core.v.lock().unwrap(), seen);
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
pub fn queue_bytes_deep<'a>(
    summary: (usize, bool),
    msgs: impl Iterator<Item = &'a WireValue>,
    seen: &mut super::fxhash::FxHashSet<usize>,
) -> usize {
    let (bytes, dirty) = summary;
    if !dirty {
        return bytes;
    }
    bytes + msgs.map(|w| nested_core_bytes(w, seen)).sum::<usize>()
}

/// W6-10r — a single-value core's own bytes PLUS every core nested in its payload. Call with the
/// payload lock held.
///
/// A `WS_UNKNOWN` summary is filled here (exactly as [`Heap::children`](super::heap::Heap::children)
/// fills it during marking): every core CONSTRUCTOR leaves it `UNKNOWN`, and a core reachable only
/// through a parent is never marked through an alias slot of its own — so without this fill it would
/// report 0 bytes forever, which is the very hole being closed.
pub fn value_core_bytes_deep(
    summary: &WireSummary,
    w: &WireValue,
    seen: &mut super::fxhash::FxHashSet<usize>,
) -> usize {
    if summary.state() == WS_UNKNOWN {
        summary.set(w);
    }
    let bytes = summary.bytes();
    if summary.state() == WS_DIRTY {
        bytes + nested_core_bytes(w, seen)
    } else {
        bytes
    }
}

/// Run `f` over a not-yet-visited core (by `Arc`-pointer identity), recording it in `seen` first so a
/// cycle back to it is a no-op. Already-seen cores are skipped.
fn visit_core(ptr: usize, seen: &mut Vec<usize>, f: impl FnOnce(&mut Vec<usize>)) {
    if seen.contains(&ptr) {
        return;
    }
    seen.push(ptr);
    f(seen);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

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
            carry: Mutex::new(Vec::new()),
        };
        let s2 = ListenerCore {
            listener: Mutex::new(Some(listener)),
            key: next_poll_key(),
            in_flight: new_in_flight(),
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
}

#[cfg(test)]
mod summary_tests {
    use super::*;

    fn list(items: Vec<WireValue>) -> WireValue {
        WireValue::List { id: 0, items }
    }

    /// W7-26, the sampling half — `take_charge` reports GROWTH, never the running total. Charging
    /// the total per submit would re-trigger a sweep on every submit once any results exist (the
    /// pacing counter is monotonic and reset at each sweep), and charging nothing leaves the cap
    /// unsampled: 300 × ~1 MB of results tripped at **313 MB → 180 MB → 11 MB peak RSS** as the
    /// accounting and then this charge landed.
    #[test]
    fn eager_charge_reports_growth_only() {
        fn done(g: &mut EagerState) {
            let o = crate::vm::TaskOutcome::Done(crate::vm::WorkerResult {
                value: list((0..1000).map(WireValue::Int).collect()),
                out: Vec::new(),
                stderr: Vec::new(),
            });
            let i = g.reserve();
            let sum = outcome_summary(&o);
            g.finish(i, sum, o);
        }
        let mut g = EagerState::default();
        assert_eq!(g.take_charge(), 0, "an empty executor charges nothing");

        done(&mut g);
        let first = g.take_charge();
        assert!(
            first >= 1000 * std::mem::size_of::<WireValue>(),
            "the finished result's bytes must be charged once: {first}"
        );
        assert_eq!(g.take_charge(), 0, "a re-read charges nothing new");
        assert_eq!(g.summary().0, first, "the total itself is unchanged");

        // The join drains the slots: the next result starts from zero again, not from a stale
        // `charged` watermark that would swallow it.
        g.take_slots();
        assert_eq!(g.summary(), (0, false));
        done(&mut g);
        assert_eq!(
            g.take_charge(),
            first,
            "a post-drain result must charge in full"
        );
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
