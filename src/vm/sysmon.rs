//! The watcher of long `extern` C calls (Go `sysmon` + `handoffp`). A C call keeps its runner
//! permit (`block::Permit::Watched`): the thread writes one stamp to its own [`Watch`] slot before
//! the call and swaps it back after. The `chezzi-sysmon` thread starts at the first watched call.
//! It reads every slot each tick. A slot that shows the same call on two ticks in a row, while a
//! thread waits for a permit or the caller's sched has a queued fiber, is retaken: the watcher
//! releases the permit for that thread and starts the caller's spare worker ([`Spare`]), so a
//! worker in C at T=1 has a replacement. The tick starts at 20 us and doubles to 10 ms while
//! nothing is retaken (Go `sysmon`); the call itself pays one thread-local read, one store and one
//! swap.
//!
//! The caller learns of a retake at the return ([`leave`]) or at a callback ([`callback_enter`]),
//! and takes a permit back before it runs VM code (Go `exitsyscall`).

use super::width;
use super::worker_loop::Spare;
use std::cell::{Cell, OnceCell};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, Weak};
use std::time::Duration;

/// The low two bits of [`Watch::state`]; the high bits number the thread's calls.
const RUNNING: u64 = 0;
const IN_C: u64 = 1;
const RETAKEN: u64 = 2;
const KIND: u64 = 3;

const FIRST_TICK: Duration = Duration::from_micros(20);
const LAST_TICK: Duration = Duration::from_millis(10);
/// The watcher runs no VM code; a small stack keeps its reservation out of a capped address space.
const WATCHER_STACK: usize = 64 * 1024;

/// One thread's slot.
struct Watch {
    state: AtomicU64,
    /// the state the watcher read at its previous tick; only the watcher touches it
    seen: AtomicU64,
    /// the [`Armed`] id of the `Vm` whose call the stamp marks (`0`: none)
    owner: AtomicU64,
    /// the replacement worker of each `Vm` that armed one on this thread, by [`Armed`] id (`None`
    /// once the OS refused its start); locked with the retake, so a caller that reads it after its
    /// swap sees whether the watcher started it
    spares: Mutex<Vec<(u64, Option<Spare>)>>,
}

impl Watch {
    /// Start the spare of the call's owner if its sched has a queued fiber. Under the `spares`
    /// lock. A refused start keeps the entry empty ([`SpareState::Refused`]), so the caller is not
    /// taken as demoted.
    fn start_spare(&self, spares: &mut Vec<(u64, Option<Spare>)>) {
        let owner = self.owner.load(Ordering::Relaxed);
        let found = spares
            .iter()
            .position(|(id, sp)| *id == owner && sp.as_ref().is_some_and(Spare::queued));
        if let Some(i) = found
            && let Some(sp) = spares[i].1.take()
            && sp.start()
        {
            spares.swap_remove(i);
        }
    }
}

/// The spare worker one `Vm` armed on its thread. Dropping it drops the spare if no retake
/// started it.
pub(super) struct Armed {
    slot: Arc<Watch>,
    id: u64,
}

/// What became of an armed spare.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum SpareState {
    /// not started yet
    Waiting,
    /// a retake started it: it is the worker's replacement
    Started,
    /// the OS refused its thread; the worker arms a new one at its next C call
    Refused,
}

impl Armed {
    pub(super) fn state(&self) -> SpareState {
        match lock(&self.slot.spares)
            .iter()
            .find(|(id, _)| *id == self.id)
        {
            Some((_, Some(_))) => SpareState::Waiting,
            Some((_, None)) => SpareState::Refused,
            None => SpareState::Started,
        }
    }

    pub(super) fn id(&self) -> u64 {
        self.id
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        let gone = {
            let mut spares = lock(&self.slot.spares);
            spares
                .iter()
                .position(|(id, _)| *id == self.id)
                .map(|i| spares.swap_remove(i))
        };
        drop(gone);
    }
}

/// Every slot, for the watcher to read.
static SLOTS: Mutex<Vec<Weak<Watch>>> = Mutex::new(Vec::new());
static START: Once = Once::new();

thread_local! {
    /// The `Arc` that keeps this thread's slot; its drop clears [`FAST`].
    static MINE: OnceCell<Owned> = const { OnceCell::new() };
    /// This thread's slot, or null: a const thread-local with no destructor, so the hot path is one
    /// read.
    static FAST: Cell<*const Watch> = const { Cell::new(std::ptr::null()) };
}

/// The owner of a thread's slot.
struct Owned(Arc<Watch>);

impl Drop for Owned {
    fn drop(&mut self) {
        let _ = FAST.try_with(|f| f.set(std::ptr::null()));
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A new slot, registered with the watcher, which starts with the first slot.
fn new_slot() -> Owned {
    START.call_once(|| {
        // The first C call waits until the thread runs: its start allocates, and the call may be
        // one that caps the process's memory (`setrlimit`). A refused thread leaves every C call
        // its permit, as with no watcher.
        static READY: AtomicBool = AtomicBool::new(false);
        let parent = std::thread::current();
        let started = std::thread::Builder::new()
            .name("chezzi-sysmon".into())
            .stack_size(WATCHER_STACK)
            .spawn(move || {
                READY.store(true, Ordering::Release);
                parent.unpark();
                watch_loop();
            });
        if started.is_ok() {
            while !READY.load(Ordering::Acquire) {
                std::thread::park();
            }
        }
    });
    let w = Arc::new(Watch {
        state: AtomicU64::new(RUNNING),
        seen: AtomicU64::new(RUNNING),
        owner: AtomicU64::new(0),
        spares: Mutex::new(Vec::new()),
    });
    lock(&SLOTS).push(Arc::downgrade(&w));
    Owned(w)
}

/// This thread's slot, made at the first use.
fn mine() -> Arc<Watch> {
    MINE.with(|m| {
        let o = m.get_or_init(new_slot);
        FAST.with(|f| f.set(Arc::as_ptr(&o.0)));
        Arc::clone(&o.0)
    })
}

/// This thread's slot, if it ever made a watched call.
fn try_mine<R>(f: impl FnOnce(&Watch) -> R) -> Option<R> {
    let p = FAST.with(Cell::get);
    // SAFETY: a non-null `FAST` points into the `Arc` that `MINE` holds on this thread, and the
    // drop of that holder nulls `FAST` first.
    (!p.is_null()).then(|| f(unsafe { &*p }))
}

/// [`try_mine`], with the slot made at the first use.
fn with_mine<R>(f: impl FnOnce(&Watch) -> R) -> R {
    let p = FAST.with(Cell::get);
    if p.is_null() {
        return f(&mine());
    }
    // SAFETY: as in `try_mine`.
    f(unsafe { &*p })
}

/// Go `entersyscall`: this thread, which holds a permit, starts a C call for the `Vm` whose
/// [`Armed`] id is `owner` (`0`: a `Vm` with no spare).
pub(super) fn enter(owner: u64) {
    with_mine(|w| {
        // Only this thread writes a call number, so the next one is the last one plus one.
        let n = (w.state.load(Ordering::Relaxed) >> 2) + 1;
        w.owner.store(owner, Ordering::Relaxed);
        w.state.store(n << 2 | IN_C, Ordering::Release);
    });
}

/// Mark this thread's slot `RUNNING`, with its call number kept, and return the state before. Only
/// this thread writes the number bits; the watcher's retake changes the kind bits alone.
fn stop(w: &Watch) -> u64 {
    let n = w.state.load(Ordering::Relaxed) & !KIND;
    w.state.swap(n | RUNNING, Ordering::AcqRel)
}

/// Go `exitsyscall`: the C call returned. `false` when the watcher retook the permit; the caller
/// then takes one back.
pub(super) fn leave() -> bool {
    let prev = with_mine(stop);
    if prev & KIND == RETAKEN {
        width::forfeit();
        return false;
    }
    true
}

/// A C callback starts while this thread is in a C call. `None` when the thread is in no watched
/// call; `Some(false)` when the watcher retook its permit, which the caller takes back before VM
/// code runs.
pub(super) fn callback_enter() -> Option<bool> {
    let prev = try_mine(stop)?;
    match prev & KIND {
        IN_C => Some(true),
        RETAKEN => {
            width::forfeit();
            Some(false)
        }
        _ => None,
    }
}

/// Give this thread's slot a spare worker for a later retake of a call of the arming `Vm`.
pub(super) fn arm(spare: Spare) -> Armed {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let slot = mine();
    lock(&slot.spares).push((id, Some(spare)));
    Armed { slot, id }
}

/// An armed spare whose start the OS refused, for a test.
#[cfg(test)]
pub(super) fn armed_refused() -> Armed {
    static NEXT: AtomicU64 = AtomicU64::new(u64::MAX / 2);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let slot = mine();
    lock(&slot.spares).push((id, None));
    Armed { slot, id }
}

/// One tick over one slot. Returns whether this tick retook it.
fn check(w: &Watch) -> bool {
    let s = w.state.load(Ordering::SeqCst);
    let seen = w.seen.swap(s, Ordering::Relaxed);
    match s & KIND {
        IN_C if seen == s => {
            let mut spares = lock(&w.spares);
            let owner = w.owner.load(Ordering::Relaxed);
            let queued = spares
                .iter()
                .any(|(id, sp)| *id == owner && sp.as_ref().is_some_and(Spare::queued));
            if !(queued || width::queued())
                || w.state
                    .compare_exchange(s, s & !KIND | RETAKEN, Ordering::SeqCst, Ordering::SeqCst)
                    .is_err()
            {
                return false;
            }
            width::hand_off();
            w.start_spare(&mut spares);
            true
        }
        RETAKEN => {
            let mut spares = lock(&w.spares);
            if w.state.load(Ordering::SeqCst) & KIND == RETAKEN {
                w.start_spare(&mut spares);
            }
            false
        }
        _ => false,
    }
}

fn watch_loop() {
    let mut tick = FIRST_TICK;
    loop {
        std::thread::sleep(tick);
        // A tick that retakes nothing allocates nothing: a program near its memory limit must not
        // die in this thread.
        let mut retook = false;
        {
            let mut slots = lock(&SLOTS);
            slots.retain(|w| w.strong_count() > 0);
            for w in slots.iter().filter_map(Weak::upgrade) {
                retook |= check(&w);
            }
        }
        tick = if retook {
            FIRST_TICK
        } else {
            (tick * 2).min(LAST_TICK)
        };
    }
}
