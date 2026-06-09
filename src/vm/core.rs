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
//! A `Condvar` (for real blocking `recv`) and close/cancel bits are deliberately **not** here yet —
//! cooperative `recv` parks the *fiber* (it does not block on a primitive), so a condvar would be dead
//! code until B3.3.

use super::value::GcRef;
use super::wire::WireValue;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// `Channel[T]` core (B3.1): the shared mailbox, an unbounded FIFO of wire-form messages. `send`
/// locks + `push_back`; `recv`/`try_recv` lock + `pop_front`; `len` locks + len. Single-thread at
/// B3.1 (the lock is uncontended); B3.3 adds a `Condvar` so a real OS thread can block on `recv`.
#[derive(Debug, Default)]
pub struct ChannelCore {
    pub q: Mutex<VecDeque<WireValue>>,
}

/// `Shared[T]` core (B3.1): the one box every task reaches. `get` locks + clones out; `set` locks +
/// overwrites; `update` reads out (lock dropped), runs the user fn, then writes back under the lock.
#[derive(Debug, Default)]
pub struct SharedCore {
    pub v: Mutex<WireValue>,
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
/// `Handle(GcRef)`s into the live heap (a `Channel[str]` queues `Str` handles; an `Executor` queues
/// `Closure` handles — closures can't cross by value until B3.3/G1).
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
        WireValue::Set(entries) => entries.iter().for_each(|(_, e)| collect_core_gcrefs(e, out, seen)),
        WireValue::Struct { fields, .. } => {
            fields.iter().for_each(|(_, v)| collect_core_gcrefs(v, out, seen))
        }
        WireValue::Enum { payload, .. } => payload.iter().for_each(|x| collect_core_gcrefs(x, out, seen)),
        WireValue::Channel(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            core.q.lock().unwrap().iter().for_each(|w| collect_core_gcrefs(w, out, s))
        }),
        WireValue::Shared(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            collect_core_gcrefs(&core.v.lock().unwrap(), out, s)
        }),
        WireValue::Executor(core) => visit_core(Arc::as_ptr(core) as usize, seen, |s| {
            core.inner.lock().unwrap().queue.iter().for_each(|w| collect_core_gcrefs(w, out, s))
        }),
        WireValue::Int(_) | WireValue::Float(_) | WireValue::Bool(_) | WireValue::Nil => {}
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
