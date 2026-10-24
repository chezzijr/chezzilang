//! The shared half of the GC heap: slots, free list, mark and copy bits, and the GC counters.
//!
//! A [`HeapCore`] holds the state that every runner thread of one run shares (M1.7), behind an
//! `Arc`: `Vm::new` makes the run's core, and `Vm::spawn_worker` gives every other `Vm` of the run
//! a handle on it (`Heap::attach`). `Heap` (`vm/heap.rs`) is the per-`Vm` handle: it keeps a small
//! cache of free slots, the counts it has not yet added to the run-wide trigger, its copy log and
//! the live context's TICKET-213 view state.
//!
//! The counters are atomics, not fields under a lock: `alloc` reads `mem_cap`, and
//! `should_collect` (every instruction) reads `next_gc` and `mem_cap` each time its handle's
//! cached threshold (`Heap.gc_check_at`) is reached. A lock there would cost a lock round trip. A
//! `Relaxed` atomic load is a plain load. The free list and the slab append share
//! one lock (`alloc_lock`). A handle takes it once per refill of its free-slot cache: up to 64
//! freed slots, or up to 64 new slots (one new slot per lock below slot 64).
//!
//! Pacing is run-wide: `allocs` and `bytes` count what every handle allocated and charged since the
//! last sweep. A handle adds its own count at most every `FLUSH_EVERY` allocations
//! (`Heap::should_collect`) and when it drops, and asks for a collection when the run's count
//! reaches `next_gc` (twice the objects the last sweep kept, at least 256), or when the run's
//! charged bytes reach the byte goal: with the cap off the live bytes of the last sweep, at least
//! 16 MiB (the shape of Go's GOGC=100), and under a `--max-heap` cap a quarter of the cap.

use super::boxctx::is_arc_core;
use super::fxhash::FxHashMap;
use super::heap::Obj;
use super::heap_slab::{Slab, SlabAppend, Slot, SlotBits};
use super::rvec::RetireList;
use super::value::GcRef;
use crate::lexer::Span;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// Smallest GC threshold — don't collect until at least this many objects have been allocated
/// since the last collection (avoids thrashing on tiny programs).
const MIN_GC_THRESHOLD: usize = 256;

/// How many free slots a handle takes from the core at once.
const FREE_BATCH: usize = 64;

/// Process-wide source for [`HeapCore::id`]: one fresh id per core, never reused.
static NEXT_HEAP_ID: AtomicU64 = AtomicU64::new(1);

/// One handle's copy log (see `Heap::begin_copy_log`), shared with the core's registry so `sweep`
/// can drop a freed slot from every handle's log.
pub(super) type CopyLog = Arc<Mutex<Vec<u32>>>;

/// What `alloc_lock` guards: the free slots and the right to append to the slab.
#[derive(Debug)]
struct AllocState {
    /// Freed slot numbers, used as a stack: the last freed is the first reused.
    free: Vec<u32>,
    append: SlabAppend,
}

/// The shared heap state. See the module doc.
#[derive(Debug)]
pub struct HeapCore {
    /// Private: [`get`](HeapCore::get), [`slot_mut`](HeapCore::slot_mut) and
    /// [`iter`](HeapCore::iter) are the only ways to a slot, and only `heap.rs` holds a core.
    slots: Slab,
    alloc_lock: Mutex<AllocState>,
    /// GC mark bits, one per slot. Two writers:
    /// - the collector (`mark`, `sweep_slots`) sets and clears bits with a load and a store, while
    ///   no handle allocates (the collector runs alone);
    /// - `reuse` clears the bit of the slot it hands out with `fetch_and`, because two handles can
    ///   reuse slots of one word at once.
    ///
    /// Post-sweep invariant: all bits of live slots 0 (survivors cleared). A free slot can keep a
    /// stray mark (see `reuse`).
    marks: SlotBits,
    /// D4 layer C (TICKET-169): a side bitset marking a heap object as an airlock COPY (a spawned
    /// task's captures/args/receiver, its module-global snapshot, or a crossing closure's
    /// captures). Set only in `Sched::from_wire_memo`/`replay_snap` under `Vm::copy_mark`. Written
    /// with `fetch_or` / `fetch_and`: mutators on two threads can share a word. `any_copied` lets a
    /// heap that holds no copy (the main task, in every bench) skip straight past the bit test.
    copied: SlotBits,
    any_copied: AtomicBool,
    /// A per-core identity, assigned once at construction from a process-wide counter (TICKET-213
    /// view stamps).
    id: u64,
    /// A handle collects once its `since_gc` reaches this; grows with the live set after each
    /// collection.
    ///
    /// W6-10s: [`request_collect`](HeapCore::request_collect) sets it to 0 to force the next
    /// `Heap::should_collect`, regardless of object count. That happens only under a live cap, for
    /// a heap that is handed a large payload in very few `Obj`s (a worker's rebuilt task
    /// args/captures): the object-count trigger cannot see those bytes, so without this nobody ever
    /// samples the cap. The next `sweep()` sets the threshold again. (One field, not a separate
    /// `force_collect` flag, so the per-instruction test is one load.)
    next_gc: AtomicUsize,
    /// Objects every handle allocated since the last sweep, as far as the handles added them
    /// (`flush_allocs`). The trigger compares it with `next_gc`.
    allocs: AtomicUsize,
    /// Bytes every handle charged since the last sweep (`flush_bytes`): the `--max-heap` byte
    /// trigger's run-wide count.
    bytes: AtomicUsize,
    /// Peak of `live_bytes()` sampled at each `sweep()`: the memory probe's high-water mark
    /// (reported behind `CHEZZI_HEAP_STATS=1`). The Phase-1 8B-`Value` gate compares this.
    peak_live_bytes: AtomicUsize,
    /// `chezzi test --max-heap`: the per-test live-heap cap in bytes (`0` = OFF, the default; the
    /// `chezzi run` engine never sets it). Checked once per `sweep()` against the `lb` already
    /// computed for `peak_live_bytes`, so it costs one `!= 0 &&` on the common (cap-off) path.
    mem_cap: AtomicUsize,
    /// Re-evaluated by every `sweep()` to `mem_cap != 0 && live_bytes() > mem_cap`. The VM reads it
    /// at its GC boundary to hard-abort the running test, re-observed each sweep like a cancel
    /// checkpoint (so a runaway `defer` during the abort unwind re-trips it); cleared per test by
    /// `clear_over_cap`.
    over_cap: AtomicBool,
    /// `live_bytes()` as of the last `sweep()`: the cap-off byte goal (`Heap::bytes_gc_threshold`).
    last_live_bytes: AtomicUsize,
    /// W8-22: the origin span of a caught error, keyed on the RAW SLOT INDEX of the `Err` payload's
    /// `GcRef`. The key is a reused index (`alloc` reuses freed slots), so `sweep()`'s free branch
    /// is what makes this table sound: leaving an entry behind would hand a dead error's coordinates
    /// to whatever `alloc` reuses the slot for next.
    err_spans: Mutex<FxHashMap<u32, Span>>,
    /// The copy log of every handle that has started one (TICKET-246), so `sweep` drops a freed
    /// slot from each.
    copy_logs: Mutex<Vec<CopyLog>>,
    /// M2: the `RVec` blocks the run's threads retired and handed in (`adopt_retired`). The
    /// sweep frees them with the world stopped (`vm/rvec.rs`).
    retired: RetireList,
}

/// `HeapCore` is shared by every handle of one core; a new `Cell` / `RefCell` in it must fail here.
const _: fn() = || {
    fn s<T: Sync + Send>() {}
    s::<HeapCore>();
};

/// Take `m`'s guard; a panic while it was held leaves the data whole, so poisoning is ignored.
pub(super) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Default for HeapCore {
    fn default() -> Self {
        let (slots, append) = Slab::new();
        HeapCore {
            slots,
            alloc_lock: Mutex::new(AllocState {
                free: Vec::new(),
                append,
            }),
            marks: SlotBits::default(),
            copied: SlotBits::default(),
            any_copied: AtomicBool::new(false),
            id: NEXT_HEAP_ID.fetch_add(1, Ordering::Relaxed),
            next_gc: AtomicUsize::new(MIN_GC_THRESHOLD),
            allocs: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            peak_live_bytes: AtomicUsize::new(0),
            mem_cap: AtomicUsize::new(0),
            over_cap: AtomicBool::new(false),
            last_live_bytes: AtomicUsize::new(0),
            err_spans: Mutex::new(FxHashMap::default()),
            copy_logs: Mutex::new(Vec::new()),
            retired: RetireList::default(),
        }
    }
}

impl HeapCore {
    /// Slot `h` (see `Slab::get`).
    #[inline]
    pub(super) fn get(&self, h: GcRef) -> &Slot {
        self.slots.get(h)
    }

    /// Slot `h`, writable.
    ///
    /// # Safety
    ///
    /// As for `Slab::slot_mut`: while the `&mut` lives, no other reference to slot `h` exists.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub(super) unsafe fn slot_mut(&self, h: GcRef) -> &mut Slot {
        // SAFETY: the caller's contract is `Slab::slot_mut`'s; the gc_coord invariant (no sweep
        // while another handle runs, `vm/gc_coord.rs`) keeps the collector off the slot.
        unsafe { self.slots.slot_mut(h) }
    }

    /// Every slot, in index order (see `Slab::iter`).
    pub(super) fn iter(&self) -> impl Iterator<Item = &Slot> {
        self.slots.iter()
    }

    /// This core's identity, stamped once at construction. Never reused: a fresh core (one per
    /// run) always gets a fresh id, even after the process has torn down other cores.
    #[inline]
    pub(super) fn id(&self) -> u64 {
        self.id
    }

    /// For a handle whose free cache is empty: take a slot for `obj` and refill `cache`. The slot
    /// is the TOP of the core's free stack, and the refill is the next `FREE_BATCH - 1` below it,
    /// kept in order. When none is free, the slot is the first of up to `FREE_BATCH` new empty
    /// slots (`Slab::push_holes`), and the rest are cached so that the lowest number pops first.
    /// Either way, the cache hands out slot numbers in the order a single free list and one-slot
    /// appends would.
    pub(super) fn alloc_slot(&self, cache: &mut Vec<u32>, obj: Obj) -> GcRef {
        let idx = {
            let mut st = lock(&self.alloc_lock);
            match st.free.pop() {
                Some(idx) => {
                    let from = st.free.len().saturating_sub(FREE_BATCH - 1);
                    cache.extend(st.free.drain(from..));
                    idx
                }
                None => {
                    let holes = self.slots.push_holes(FREE_BATCH, &mut st.append);
                    cache.extend((holes.start + 1..holes.end).rev().map(|i| i as u32));
                    holes.start as u32
                }
            }
        };
        // SAFETY (gc_coord invariant: no sweep while this handle runs, `vm/gc_coord.rs`): `idx`
        // came off the free list or the slab end under `alloc_lock` just now, so no
        // other handle holds it.
        unsafe { self.reuse(idx, obj) }
    }

    /// Put a handle's cached free slots back on top of the core's stack, in order.
    pub(super) fn give_back(&self, cache: &mut Vec<u32>) {
        if !cache.is_empty() {
            lock(&self.alloc_lock).free.append(cache);
        }
    }

    /// Put `obj` into the empty slot `idx` from a handle's cache, clearing the slot's mark and copy
    /// bit so the new object does not inherit them. The mark is 0 after a normal sweep, but a
    /// collection that marks a dangling ref and then panics in `children` (caught by `boxctx.rs` or
    /// `call.rs`) leaves a mark on a free slot, which `sweep_slots` never visits.
    ///
    /// # Safety
    ///
    /// `idx` is an empty slot the caller took from the core (`alloc_slot`'s refill of its cache)
    /// and holds alone: no other handle reuses it and no reference to it is live.
    #[inline]
    pub(super) unsafe fn reuse(&self, idx: u32, obj: Obj) -> GcRef {
        let h = GcRef(idx);
        // SAFETY: the caller holds the empty slot `idx` alone (the contract above), and the gc_coord
        // invariant keeps a sweep off it while the caller's handle runs (`vm/gc_coord.rs`).
        unsafe { self.slots.slot_mut(h) }.obj = Some(obj);
        // Load first: the bit is almost always clear, and a clear bit needs no write. Only the
        // collector sets marks, and it does not run while a handle allocates.
        if self.marks.get(idx as usize) {
            self.marks.clear(idx as usize);
        }
        if self.any_copied.load(Ordering::Relaxed) {
            self.copied.clear(idx as usize);
        }
        h
    }

    /// Live (allocated, not freed) object count: the slots that hold an object. Derived, not
    /// counted at `alloc`: a shared counter would cost every allocation an atomic add.
    #[cfg(test)]
    pub(super) fn live(&self) -> usize {
        self.slots.iter().filter(|s| s.obj.is_some()).count()
    }

    #[inline]
    pub(super) fn next_gc(&self) -> usize {
        self.next_gc.load(Ordering::Relaxed)
    }

    /// Force the next `Heap::should_collect` (see `next_gc`) of the handle that calls
    /// `Heap::request_collect`, which also empties that handle's cached threshold. Every other
    /// handle reads the 0 at its next boundary: a request is made only under a live cap, and under
    /// a cap no handle caches a threshold (`Heap::should_collect` caches only with the cap off).
    pub(super) fn request_collect(&self) {
        self.next_gc.store(0, Ordering::Relaxed);
    }

    /// Add `n` allocations of one handle to the run's count; returns the run's count since the
    /// last sweep.
    #[inline]
    pub(super) fn flush_allocs(&self, n: usize) -> usize {
        if n == 0 {
            self.allocs.load(Ordering::Relaxed)
        } else {
            self.allocs.fetch_add(n, Ordering::Relaxed) + n
        }
    }

    /// Add `n` charged bytes of one handle to the run's count; returns the run's count since the
    /// last sweep.
    #[inline]
    pub(super) fn flush_bytes(&self, n: usize) -> usize {
        if n == 0 {
            self.bytes.load(Ordering::Relaxed)
        } else {
            self.bytes.fetch_add(n, Ordering::Relaxed).saturating_add(n)
        }
    }

    /// Mark slot `i`; returns whether it was unmarked before.
    #[inline]
    pub(super) fn mark(&self, i: usize) -> bool {
        self.marks.set_owned(i)
    }

    pub(super) fn set_copied(&self, i: usize) {
        self.copied.set(i);
        self.any_copied.store(true, Ordering::Relaxed);
    }

    /// Test the copy bit of slot `i`. `any_copied` makes this one predictable branch on a core
    /// that has never marked anything (the main task in every bench).
    #[inline]
    pub(super) fn is_copied(&self, i: usize) -> bool {
        self.any_copied.load(Ordering::Relaxed) && self.copied.get(i)
    }

    #[inline]
    pub(super) fn clear_copied(&self, i: usize) {
        self.copied.clear(i);
    }

    /// Add `log` to the registry `sweep` walks.
    pub(super) fn register_copy_log(&self, log: CopyLog) {
        lock(&self.copy_logs).push(log);
    }

    /// Remove `log` from the registry (its handle is dropped).
    pub(super) fn unregister_copy_log(&self, log: &CopyLog) {
        lock(&self.copy_logs).retain(|l| !Arc::ptr_eq(l, log));
    }

    pub(super) fn set_err_span(&self, r: GcRef, span: Span) {
        lock(&self.err_spans).insert(r.0, span);
    }

    pub(super) fn err_span(&self, r: GcRef) -> Option<Span> {
        lock(&self.err_spans).get(&r.0).copied()
    }

    /// The slot half of a collection: free every unmarked object, clear the marks, set the next
    /// threshold from the survivors (twice their count) and zero the run's counts. A freed slot's
    /// copy bit and error span go with it, and every handle's copy log drops it: the slot can be
    /// handed to another object before a log is unmarked. The caller (`Heap::sweep`) gave its free
    /// cache back first and measures the live bytes after. The run's retired `RVec` blocks are
    /// freed too.
    ///
    /// The slots in another handle's free cache are empty, so this skips them: it neither counts
    /// them live nor puts them on the free list a second time, and they stay with their handle.
    /// A freed object that owns an `Arc` core or a file goes to `graveyard` and is dropped by the
    /// caller after the world restarts: its drop can make a syscall (a buffered writer flushes).
    ///
    /// # Safety
    ///
    /// As for `Slab::segments_mut`: no other thread touches a slot while this runs. Every other
    /// handle of the core is stopped at a safepoint or detached (`vm/gc_coord.rs`), or the caller's
    /// handle is the core's only one.
    pub(super) unsafe fn sweep_slots(&self, graveyard: &mut Vec<Obj>) {
        let mut live = 0usize;
        {
            let mut st = lock(&self.alloc_lock);
            let mut spans = lock(&self.err_spans);
            // SAFETY: the caller's contract above: the gc_coord invariant, every other handle
            // stopped or detached for the sweep (`vm/gc_coord.rs`).
            for (start, seg) in unsafe { self.slots.segments_mut() } {
                let marks = self.marks.segment_words(start);
                for (idx, slot) in (start..).zip(seg.iter_mut()) {
                    if slot.obj.is_none() {
                        continue;
                    }
                    let word = marks.get((idx >> 6) - (start >> 6));
                    let bit = 1u64 << (idx & 63);
                    match word {
                        Some(w) if w.load(Ordering::Relaxed) & bit != 0 => {
                            w.store(w.load(Ordering::Relaxed) & !bit, Ordering::Relaxed);
                            live += 1;
                        }
                        Some(_) | None => {
                            if let Some(obj) = slot.obj.take()
                                && is_arc_core(&obj)
                            {
                                graveyard.push(obj);
                            }
                            st.free.push(idx as u32);
                            spans.remove(&(idx as u32));
                            self.copied.clear(idx);
                        }
                    }
                }
            }
        }
        // Every other handle handed in its retired blocks when it stopped, detached or dropped;
        // zeroing `bytes` below uncharges them.
        // SAFETY: the caller's contract above: the world is stopped (`vm/gc_coord.rs`).
        unsafe { self.retired.free_all() };
        self.next_gc
            .store((live * 2).max(MIN_GC_THRESHOLD), Ordering::Relaxed);
        self.allocs.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
        for log in lock(&self.copy_logs).iter() {
            lock(log).retain(|&i| self.slots.get(GcRef(i)).obj.is_some());
        }
    }

    /// Move a handle's retired `RVec` blocks to the run's list. Called when the handle's thread
    /// stops, detaches, leaves or sweeps, and when the handle drops. The bytes were charged at
    /// the retire (`vm/rvec.rs`).
    pub(super) fn adopt_retired(&self, from: &RetireList) {
        from.move_into(&self.retired);
    }

    /// Blocks on the run's retire list (tests only).
    #[cfg(test)]
    pub(super) fn retired_len(&self) -> usize {
        self.retired.len()
    }

    /// Retired blocks of `T`s on the run's retire list (tests only).
    #[cfg(test)]
    pub(super) fn retired_count_of<T: super::rvec::Elem>(&self) -> usize {
        self.retired.count_of::<T>()
    }

    /// Record the `live_bytes()` a sweep measured: the last value, the peak and the cap verdict.
    pub(super) fn record_live_bytes(&self, lb: usize) {
        self.last_live_bytes.store(lb, Ordering::Relaxed);
        self.peak_live_bytes.fetch_max(lb, Ordering::Relaxed);
        // `--max-heap` runaway-alloc guard: off when `mem_cap == 0`, so `over_cap` stays false
        // forever on the common path.
        let cap = self.mem_cap();
        self.over_cap.store(cap != 0 && lb > cap, Ordering::Relaxed);
    }

    /// Bytes the handles added to the run's count since the last sweep (`flush_bytes`).
    #[inline]
    pub(super) fn charged_bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }

    #[inline]
    pub(super) fn last_live_bytes(&self) -> usize {
        self.last_live_bytes.load(Ordering::Relaxed)
    }

    pub(super) fn peak_live_bytes(&self) -> usize {
        self.peak_live_bytes.load(Ordering::Relaxed)
    }

    #[inline]
    pub(super) fn mem_cap(&self) -> usize {
        self.mem_cap.load(Ordering::Relaxed)
    }

    /// Set the cap and clear the `over_cap` latch.
    pub(super) fn set_mem_cap(&self, cap: usize) {
        self.mem_cap.store(cap, Ordering::Relaxed);
        self.over_cap.store(false, Ordering::Relaxed);
    }

    pub(super) fn over_cap(&self) -> bool {
        self.over_cap.load(Ordering::Relaxed)
    }

    pub(super) fn clear_over_cap(&self) {
        self.over_cap.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use crate::vm::heap::{Heap, Obj};
    use crate::vm::rvec::RVec;
    use crate::vm::value::GcRef;
    use std::collections::HashSet;

    /// Two handles on one core, on two threads, never get the same slot, and each handle reads back
    /// what both threads wrote.
    #[test]
    fn two_handles_alloc_disjoint_slots_concurrently() {
        const N: i64 = 100_000;
        let first = Heap::new();
        // SAFETY: each handle allocates its own slots and reads only after both threads joined.
        let second = unsafe { first.attach() };
        let run = |mut heap: Heap, tid: i64| {
            std::thread::spawn(move || {
                let refs: Vec<GcRef> = (0..N)
                    .map(|i| heap.alloc(Obj::BigInt(tid * N + i)))
                    .collect();
                (heap, refs)
            })
        };
        let (a, b) = (run(first, 0), run(second, 1));
        let (heap_a, refs_a) = a.join().unwrap();
        let (heap_b, refs_b) = b.join().unwrap();
        let all: HashSet<u32> = refs_a.iter().chain(&refs_b).map(|r| r.0).collect();
        assert_eq!(all.len(), 2 * N as usize, "every GcRef is distinct");
        for heap in [&heap_a, &heap_b] {
            for (tid, refs) in [(0, &refs_a), (1, &refs_b)] {
                for (i, r) in refs.iter().enumerate() {
                    match heap.get(*r) {
                        Obj::BigInt(n) => assert_eq!(*n, tid * N + i as i64),
                        other => panic!("slot {} holds {other:?}", r.0),
                    }
                }
            }
        }
    }

    /// A sweep drops a freed slot from the copy log of every handle on the core, so no handle
    /// clears the copy mark of the object that reuses the slot.
    #[test]
    fn copy_log_retain_spans_handles() {
        let mut a = Heap::new();
        // SAFETY: one thread drives both handles, and `a.sweep` runs with `b` idle.
        let mut b = unsafe { a.attach() };
        let x = a.alloc(Obj::List(RVec::new()));
        let y = b.alloc(Obj::List(RVec::new()));
        let z = a.alloc(Obj::List(RVec::new()));
        a.begin_copy_log();
        a.set_copied(x);
        a.set_copied(y);
        a.end_copy_log();
        b.begin_copy_log();
        b.set_copied(x);
        b.set_copied(z);
        b.end_copy_log();
        a.mark(y);
        a.mark(z);
        a.sweep();
        let w = a.alloc(Obj::List(RVec::new()));
        assert_eq!(w, x, "the freed slot is reused");
        a.set_copied(w);
        a.unmark_copy_log();
        b.unmark_copy_log();
        assert!(a.is_copied(w), "no log kept the freed slot");
        assert!(
            !a.is_copied(y) && !b.is_copied(z),
            "each log cleared its own survivor"
        );
    }

    /// A mark left on a freed slot (the collector marks a dangling ref, then `children` panics on
    /// it and a `catch_unwind` resumes the run) must not survive into the object that reuses the
    /// slot: that object would look marked, and the next collection would skip its children.
    #[test]
    fn reused_slot_drops_a_stale_mark() {
        let mut heap = Heap::new();
        let x = heap.alloc(Obj::List(RVec::new()));
        heap.sweep();
        assert!(heap.mark(x), "the freed slot takes a stray mark");
        let y = heap.alloc(Obj::List(RVec::new()));
        assert_eq!(y, x, "the freed slot is reused");
        assert!(heap.mark(y), "the new object starts unmarked");
    }

    /// Two handles that reuse slots of one mark word at once must both clear their slot's mark: a
    /// load-then-store clear can lose the other thread's clear, and the next collection would then
    /// skip the new object's children. Slots `[64, 128)` share one word; every round marks all of
    /// them, then two threads reuse the even and the odd slots.
    #[test]
    fn concurrent_reuse_clears_every_mark_in_a_shared_word() {
        use std::sync::Barrier;
        const ROUNDS: usize = 20_000;
        let core = super::HeapCore::default();
        let mut cache = Vec::new();
        for i in 0..128 {
            core.alloc_slot(&mut cache, Obj::BigInt(i));
        }
        let start = Barrier::new(3);
        let done = Barrier::new(3);
        let lost = std::thread::scope(|s| {
            for parity in 0..2u32 {
                let (core, start, done) = (&core, &start, &done);
                s.spawn(move || {
                    for _ in 0..ROUNDS {
                        start.wait();
                        for idx in (64 + parity..128).step_by(2) {
                            // SAFETY: the two threads reuse disjoint slots (even and odd), and
                            // the main thread reads only the marks between the barriers.
                            unsafe { core.reuse(idx, Obj::BigInt(idx as i64)) };
                        }
                        done.wait();
                    }
                });
            }
            let mut lost = 0usize;
            for _ in 0..ROUNDS {
                for i in 64..128 {
                    core.marks.set_owned(i);
                }
                start.wait();
                done.wait();
                lost += (64..128).filter(|&i| core.marks.get(i)).count();
            }
            lost
        });
        assert_eq!(lost, 0, "a reuse lost another thread's mark clear");
    }
}
