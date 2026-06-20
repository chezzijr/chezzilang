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
use super::wire::WireValue;
use std::collections::VecDeque;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// `Channel[T]` core (B3.1): the shared mailbox, an unbounded FIFO of wire-form messages. `send`
/// locks + `push_back`; `recv`/`try_recv` lock + `pop_front`; `len` locks + len.
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
    pub queue: VecDeque<WireValue>,
    pub closed: bool,
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
}

/// `Atomic[T]` core: the cross-task atomic box. Like [`SharedCore`] (one boxed wire value behind a
/// `Mutex`, reachable across threads via the `Arc` handle), but presents atomic-operation methods —
/// `load`/`store`/`exchange`/`cas` and (numeric `T`) `add`/`sub`. Each method is a single
/// lock-op-unlock, so the read-modify-write of `add`/`sub`/`exchange`/`cas` is atomic across threads
/// without a separate `update_lock` (no user closure runs under the lock, unlike `Shared.update`).
#[derive(Debug, Default)]
pub struct AtomicCore {
    pub v: Mutex<WireValue>,
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

/// The mutable inside of an [`ExecutorCore`]: the pending-task FIFO + the shut flag, behind one lock
/// (one `Mutex` for both, so `submit`/`shutdown` see a consistent view and to avoid a `Mutex<bool>`).
#[derive(Debug, Default)]
pub struct ExecState {
    pub queue: VecDeque<WireValue>,
    pub shut: bool,
}

/// `Executor` core (B3.1 / C5 escape hatch): the explicitly-owned work queue. `submit` enqueues a
/// wire-form task closure (rejected once `shut`); `shutdown` drains FIFO; `shutdown_now` discards.
/// `shut` lives in the **shared** core, so any handle aliasing this core sees the same shutdown state
/// (this is what prevents a `from_wire`'d alias from being drained twice at program exit).
#[derive(Debug, Default)]
pub struct ExecutorCore {
    pub inner: Mutex<ExecState>,
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
pub fn collect_core_gcrefs(w: &WireValue, out: &mut Vec<GcRef>, seen: &mut Vec<usize>) {
    match w {
        WireValue::Handle(g) => out.push(*g),
        WireValue::List(xs) | WireValue::Tuple(xs) => {
            xs.iter().for_each(|x| collect_core_gcrefs(x, out, seen))
        }
        WireValue::Map(entries) => entries.iter().for_each(|(_, k, v)| {
            collect_core_gcrefs(k, out, seen);
            collect_core_gcrefs(v, out, seen);
        }),
        WireValue::Set(entries) => entries
            .iter()
            .for_each(|(_, e)| collect_core_gcrefs(e, out, seen)),
        WireValue::Struct { fields, .. } => fields
            .iter()
            .for_each(|(_, v)| collect_core_gcrefs(v, out, seen)),
        WireValue::Enum { payload, .. } => payload
            .iter()
            .for_each(|x| collect_core_gcrefs(x, out, seen)),
        WireValue::NewType { inner, .. } => collect_core_gcrefs(inner, out, seen),
        // A cursor queued in a channel/executor roots its snapshot items' handles (like `List`).
        WireValue::Iter { items, .. } => {
            items.iter().for_each(|x| collect_core_gcrefs(x, out, seen))
        }
        WireValue::Channel(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            core.q
                .lock()
                .unwrap()
                .queue
                .iter()
                .for_each(|w| collect_core_gcrefs(w, out, s))
        }),
        WireValue::Shared(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            collect_core_gcrefs(&core.v.lock().unwrap(), out, s)
        }),
        WireValue::Atomic(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            collect_core_gcrefs(&core.v.lock().unwrap(), out, s)
        }),
        WireValue::Executor(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            core.inner
                .lock()
                .unwrap()
                .queue
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
        WireValue::Str(_)
        | WireValue::Bytes(_)
        | WireValue::ByteArray(_)
        | WireValue::Int(_)
        | WireValue::Float(_)
        | WireValue::Bool(_)
        | WireValue::Socket(_)
        | WireValue::Listener(_)
        | WireValue::Ptr(_)
        | WireValue::Nil => {}
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
}
