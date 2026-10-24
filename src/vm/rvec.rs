//! A growable vector whose old buffers are retired and freed only at a stop-the-world GC.
//!
//! M2 (Go parity): two tasks may touch one container with no lock, and a race is the user's bug.
//! A racy reader must still never read freed memory. So an [`RVec`] that grows does not free its
//! old buffer: it RETIRES it, the way Go keeps the old `append` array alive until no one can
//! reach it. Only the stop-the-world collector frees a retired buffer.
//!
//! - An `RVec<T>` is one `AtomicPtr` to a [`Header`] block: one allocation, the header and then
//!   `cap` inline atomic slots, zeroed at allocation. A null pointer is the empty vec. The
//!   header's `cap` and `len` are `u32`s, so a vec holds at most `u32::MAX` elements; a larger
//!   capacity panics (a task fault).
//! - `T` is an [`Elem`]: `Copy`, no padding, in an atomic slot of its own size (`u64`, `i64`
//!   and `Value` in an `AtomicU64`, `u8` in an `AtomicU8`, so bytes pack one per byte; a map or
//!   set entry in one `AtomicU64` per field, so a racy reader can see it torn). Every slot
//!   access is a `Relaxed` atomic of the element, so a racing read and write on one slot is no data
//!   race and never tears. `Copy` is also what makes a racy read sound: a reader copies the bits,
//!   and a copied `T` with a drop would drop twice.
//! - A read loads the pointer (Acquire), then that block's `len` (Acquire), then a slot below
//!   `len`. An index at or past `len` panics; the VM's `catch_unwind` turns a racy index into a
//!   task fault.
//! - A push into spare capacity writes the slot, then stores `len + 1` (Release). A grow
//!   allocates a block of twice the capacity, copies the elements, and swaps the pointer with a
//!   `compare_exchange` (AcqRel). The winner retires the old block; a loser frees its own new
//!   block (no one else saw it) and retries.
//! - A shrink frees nothing: `truncate` lowers `len`, and an `Elem` has no drop.
//! - A whole-vec rewrite (sort, reverse) builds a new block and swaps it in with
//!   [`RVec::replace`]; the old block retires like a grown one.
//!
//! Retire lists: a grow takes the `Heap` handle of its thread (one handle per thread per run) and
//! puts the old block on the handle's [`RetireList`]. The handle moves its list to the run's
//! `HeapCore.retired` when its thread stops for a GC (`Vm::gc_park`), detaches
//! (`Vm::gc_blocking`, `Vm::gc_reenter`), leaves (`Vm::gc_leave`), sweeps (`Heap::sweep`), or
//! drops (`Heap`'s drop). The sweep frees every block on the core's list with the world still
//! stopped ([`RetireList::free_all`]). That is sound because a reader holds a block pointer only
//! between two safepoints: an [`Iter`] or a value read never lives across a poll or a door.
//! Nothing else frees a retired block, except the core's own drop, when no handle of the run is
//! left.
//!
//! Pacing: a grow charges the retired block's bytes to its handle (`Heap::charge_bytes`), once.
//! The hand-off does not charge them again. The sweep that frees the block zeroes the run's byte
//! count in the same stop, which uncharges them.
//!
//! No `as_slice`: a `&[T]` over a block that another thread can write with `set` is not sound.
//! No API hands out `&mut [T]`. Miri and TSan are not available on this box, so the races here
//! are tested only by the stress tests below.

use super::heap::Heap;
use super::value::Value;
use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::marker::PhantomData;
use std::mem::{align_of, size_of};
use std::ptr::{self, NonNull};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering};

mod sealed {
    pub trait Sealed {}
}

/// An element type of an [`RVec`]: a `Copy` value with no padding, stored in an atomic slot of
/// its own size (`AtomicU64` for the 8-byte words, `AtomicU8` for a byte).
pub trait Elem: Copy + sealed::Sealed {
    /// The atomic slot one element lives in. A zeroed slot is a valid one.
    type Slot;
    /// Evaluated by every slot access: an element is exactly its slot's size, and needs no more
    /// alignment than the slot has.
    const FITS_SLOT: () = assert!(
        size_of::<Self>() == size_of::<Self::Slot>()
            && align_of::<Self>() <= align_of::<Self::Slot>()
    );
    fn load(slot: &Self::Slot) -> Self;
    fn store(slot: &Self::Slot, v: Self);
}

/// An 8-byte word in an `AtomicU64`, as its bits (`$to` and `$from` convert).
macro_rules! word_elem {
    ($t:ty, $to:expr, $from:expr) => {
        impl sealed::Sealed for $t {}
        impl Elem for $t {
            type Slot = AtomicU64;
            fn load(slot: &AtomicU64) -> Self {
                $from(slot.load(Ordering::Relaxed))
            }
            fn store(slot: &AtomicU64, v: Self) {
                slot.store($to(v), Ordering::Relaxed);
            }
        }
    };
}

word_elem!(u64, |v: u64| v, |b: u64| b);
word_elem!(i64, |v: i64| v as u64, |b: u64| b as i64);
word_elem!(Value, Value::to_bits, Value::from_bits);

/// A map entry, `(cached hash, key, value)`, in one `AtomicU64` per field. A racy reader can
/// see a torn entry (fields of two writes): the user's race, like a torn Go struct.
impl sealed::Sealed for (u64, Value, Value) {}
impl Elem for (u64, Value, Value) {
    type Slot = [AtomicU64; 3];
    fn load([h, k, v]: &[AtomicU64; 3]) -> Self {
        (u64::load(h), Value::load(k), Value::load(v))
    }
    fn store([h, k, v]: &[AtomicU64; 3], (eh, ek, ev): Self) {
        u64::store(h, eh);
        Value::store(k, ek);
        Value::store(v, ev);
    }
}

/// An index slot of a map or a set, `(cached hash, position + 1)` (`vm/map_data.rs`).
impl sealed::Sealed for (u64, u64) {}
impl Elem for (u64, u64) {
    type Slot = [AtomicU64; 2];
    fn load([h, p]: &[AtomicU64; 2]) -> Self {
        (u64::load(h), u64::load(p))
    }
    fn store([h, p]: &[AtomicU64; 2], (eh, ep): Self) {
        u64::store(h, eh);
        u64::store(p, ep);
    }
}

/// A set entry, `(cached hash, element)`, as a map entry without the value.
impl sealed::Sealed for (u64, Value) {}
impl Elem for (u64, Value) {
    type Slot = [AtomicU64; 2];
    fn load([h, e]: &[AtomicU64; 2]) -> Self {
        (u64::load(h), Value::load(e))
    }
    fn store([h, e]: &[AtomicU64; 2], (eh, ee): Self) {
        u64::store(h, eh);
        Value::store(e, ee);
    }
}

impl sealed::Sealed for u8 {}
impl Elem for u8 {
    type Slot = AtomicU8;
    fn load(slot: &AtomicU8) -> Self {
        slot.load(Ordering::Relaxed)
    }
    fn store(slot: &AtomicU8, v: Self) {
        slot.store(v, Ordering::Relaxed);
    }
}

/// The head of one block. The slots follow it in the same allocation, at `data_offset()`.
///
/// The header is 8 bytes (`u32` fields), so a block of two slots is 24 bytes, the allocator's
/// smallest class, like a `Vec` of two: a list literal costs no more memory than before.
#[repr(C)]
pub(super) struct Header {
    cap: u32,
    len: AtomicU32,
}

impl Header {
    fn cap(&self) -> usize {
        self.cap as usize
    }

    fn len(&self) -> usize {
        self.len.load(Ordering::Acquire) as usize
    }

    /// Publish `n` elements (Release: the slots below `n` are written first). `n <= cap`, so it
    /// fits the `u32`.
    fn set_len(&self, n: usize) {
        self.len.store(n as u32, Ordering::Release);
    }
}

/// The offset of slot 0 from the block's start (the same for every `cap`).
fn data_offset<T: Elem>() -> usize {
    Layout::new::<Header>()
        .extend(Layout::new::<T::Slot>())
        .expect("an RVec block layout")
        .1
}

fn block_layout<T: Elem>(cap: usize) -> Layout {
    let data = Layout::array::<T::Slot>(cap).expect("an RVec capacity overflows");
    Layout::new::<Header>()
        .extend(data)
        .expect("an RVec capacity overflows")
        .0
        .pad_to_align()
}

#[cfg(test)]
static ALLOCS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
static FREES: AtomicUsize = AtomicUsize::new(0);

#[cfg(test)]
thread_local! {
    static TRACKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Count the blocks this thread allocates and frees from now on (tests only). Every VM test
/// allocates lists, so only the threads of a counting test feed the counters.
#[cfg(test)]
pub(super) fn test_track() {
    TRACKED.with(|t| t.set(true));
}

/// Bump `c` when this thread is counted (tests only).
#[cfg(test)]
fn count(c: &AtomicUsize) {
    if TRACKED.with(|t| t.get()) {
        c.fetch_add(1, Ordering::SeqCst);
    }
}

/// Blocks allocated and blocks freed so far by the counted threads ([`test_track`]; tests only).
#[cfg(test)]
pub(super) fn test_counts() -> (usize, usize) {
    (ALLOCS.load(Ordering::SeqCst), FREES.load(Ordering::SeqCst))
}

/// The block counters are process-wide: the tests that read them hold this lock, which also
/// counts the calling thread ([`test_track`]; tests only).
#[cfg(test)]
pub(super) fn test_serial() -> std::sync::MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    test_track();
    g
}

/// A new block of `cap` zeroed slots, `len` 0.
fn alloc_block<T: Elem>(cap: usize) -> *mut Header {
    let cap32 = u32::try_from(cap).expect("an RVec capacity overflows");
    let layout = block_layout::<T>(cap);
    // SAFETY: `layout` has a non-zero size (the header is 8 bytes).
    let p = unsafe { alloc_zeroed(layout) }.cast::<Header>();
    if p.is_null() {
        handle_alloc_error(layout);
    }
    // SAFETY: `p` is a fresh allocation with room and alignment for a `Header`; the zeroed slots
    // after it are valid `T::Slot`s.
    unsafe {
        p.write(Header {
            cap: cap32,
            len: AtomicU32::new(0),
        })
    };
    #[cfg(test)]
    count(&ALLOCS);
    p
}

/// Free a block that no thread can read any more.
///
/// # Safety
///
/// `ptr` and `layout` come from one `alloc_block`, and no reference to the block lives.
unsafe fn free_block(ptr: NonNull<u8>, layout: Layout) {
    // SAFETY: the caller's contract.
    unsafe { dealloc(ptr.as_ptr(), layout) };
    #[cfg(test)]
    count(&FREES);
}

/// Slot `i` of block `h`, a block of `T`s.
///
/// # Safety
///
/// `h` is a live block of `T`s and `i < cap`; the returned reference does not outlive the block.
unsafe fn slot<'a, T: Elem>(h: *mut Header, i: usize) -> &'a T::Slot {
    // SAFETY: the caller's contract: the slot lies inside the block's allocation, and
    // `alloc_block` zeroed it, so it is a valid `T::Slot`.
    unsafe {
        &*h.cast::<u8>()
            .add(data_offset::<T>())
            .cast::<T::Slot>()
            .add(i)
    }
}

/// Read slot `i` of `h`.
///
/// # Safety
///
/// As for [`slot`].
unsafe fn load<T: Elem>(h: *mut Header, i: usize) -> T {
    let () = T::FITS_SLOT;
    // SAFETY: the caller's contract.
    T::load(unsafe { slot::<T>(h, i) })
}

/// Write `v` into slot `i` of `h`.
///
/// # Safety
///
/// As for [`slot`].
unsafe fn store<T: Elem>(h: *mut Header, i: usize, v: T) {
    let () = T::FITS_SLOT;
    // SAFETY: the caller's contract.
    T::store(unsafe { slot::<T>(h, i) }, v);
}

/// Put the block `old` of `cap` slots, which no vec points to any more, on `heap`'s retire list,
/// and charge its bytes to `heap` (once; see the module doc's pacing).
fn retire<T: Elem>(heap: &Heap, old: *mut Header, cap: usize) {
    let ptr = NonNull::new(old.cast::<u8>()).expect("a retired block is not null");
    let layout = block_layout::<T>(cap);
    heap.retire_list().lock().push(Retired {
        ptr,
        layout,
        #[cfg(test)]
        elem: std::any::type_name::<T>(),
    });
    heap.charge_bytes(layout.size());
}

/// A retired block: freeing it needs only its layout (an `Elem` has no drop).
struct Retired {
    ptr: NonNull<u8>,
    layout: Layout,
    /// The element type of the retired block (tests only).
    #[cfg(test)]
    elem: &'static str,
}

// SAFETY: a retired block is plain memory that no `RVec` points to; the list that holds it is
// the only thing that frees it, on whichever thread drains the list.
unsafe impl Send for Retired {}

/// Retired blocks: a `Heap` handle's own (not yet handed off), or the run's (`HeapCore.retired`),
/// which the stop-the-world sweep frees. A handle hands its list to the core before it drops, so
/// only the core's list is ever dropped with blocks on it.
#[derive(Default)]
pub(super) struct RetireList(Mutex<Vec<Retired>>);

impl std::fmt::Debug for RetireList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RetireList({})", self.len())
    }
}

impl RetireList {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Retired>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Move every block of `self` onto `to`.
    pub(super) fn move_into(&self, to: &RetireList) {
        let blocks = std::mem::take(&mut *self.lock());
        if !blocks.is_empty() {
            to.lock().extend(blocks);
        }
    }

    /// Blocks on this list.
    pub(super) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Retired blocks of `T`s on this list (tests only).
    #[cfg(test)]
    pub(super) fn count_of<T: Elem>(&self) -> usize {
        let name = std::any::type_name::<T>();
        self.lock().iter().filter(|r| r.elem == name).count()
    }

    /// Free every block on this list.
    ///
    /// # Safety
    ///
    /// The world is stopped (every other handle of the run is parked at a safepoint or
    /// detached, `vm/gc_coord.rs`), or no handle of the run is left. Either way no reader holds a
    /// pointer to a block on this list.
    pub(super) unsafe fn free_all(&self) {
        let blocks = std::mem::take(&mut *self.lock());
        for b in blocks {
            // SAFETY: the caller's contract; no `RVec` points to a retired block.
            unsafe { free_block(b.ptr, b.layout) };
        }
    }
}

impl Drop for RetireList {
    /// Only the core's list drops with blocks on it (see the type doc): its run has no handle
    /// left, so no reader is left.
    fn drop(&mut self) {
        // SAFETY: the type doc: no handle of the run is left.
        unsafe { self.free_all() };
    }
}

/// The capacity a grow from `cap` slots to at least `min_cap` allocates: twice `cap`, at least 4,
/// clamped to the header's `u32` limit (a `min_cap` past it still panics in `alloc_block`).
fn grown_cap(cap: usize, min_cap: usize) -> usize {
    let doubled = cap.saturating_mul(2).min(u32::MAX as usize);
    doubled.max(min_cap).max(4)
}

/// A growable vector of `T: Elem` whose grows retire the old block (see the module doc).
pub struct RVec<T: Elem> {
    ptr: AtomicPtr<Header>,
    _t: PhantomData<T>,
}

impl<T: Elem> Default for RVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Elem> RVec<T> {
    /// An empty vec with no block.
    pub const fn new() -> Self {
        RVec {
            ptr: AtomicPtr::new(ptr::null_mut()),
            _t: PhantomData,
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        let p = if cap == 0 {
            ptr::null_mut()
        } else {
            alloc_block::<T>(cap)
        };
        RVec {
            ptr: AtomicPtr::new(p),
            _t: PhantomData,
        }
    }

    /// The current block (null when empty).
    fn block(&self) -> *mut Header {
        self.ptr.load(Ordering::Acquire)
    }

    /// The current block, its `len` and its `cap`.
    fn snapshot(&self) -> (*mut Header, usize, usize) {
        let h = self.block();
        if h.is_null() {
            return (h, 0, 0);
        }
        // SAFETY: a block this vec pointed to is live until a stop; this thread holds it only
        // until its next safepoint (see the module doc).
        let hr = unsafe { &*h };
        (h, hr.len(), hr.cap())
    }

    pub fn len(&self) -> usize {
        self.snapshot().1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The slots of the current block (`live_bytes` reads it).
    pub fn capacity(&self) -> usize {
        self.snapshot().2
    }

    /// A vec of `n` zeroed elements (a zeroed slot is a valid `T`, see [`Elem`]).
    pub fn zeroed(n: usize) -> Self {
        let v = Self::with_capacity(n);
        let (h, _, _) = v.snapshot();
        if !h.is_null() {
            // SAFETY: `h` is this vec's own new block of `n` zeroed slots.
            unsafe { (*h).set_len(n) };
        }
        v
    }

    /// A vec holding a copy of `xs`, at exactly its length.
    pub fn from_slice(xs: &[T]) -> Self {
        let v = Self::with_capacity(xs.len());
        let (h, _, _) = v.snapshot();
        // SAFETY: `h` is this vec's own new block of `xs.len()` slots (null only when `xs` is
        // empty, and then nothing is written); no other thread has seen it.
        unsafe {
            for (k, &x) in xs.iter().enumerate() {
                store(h, k, x);
            }
            if !h.is_null() {
                (*h).set_len(xs.len());
            }
        }
        v
    }

    /// A copy of the elements of one snapshot.
    pub fn to_vec(&self) -> Vec<T> {
        self.iter().collect()
    }

    /// Replace every element with `xs`: a new block, swapped in; the old block retires on
    /// `heap`. Sort, reverse and the other whole-vec rewrites go through it, so no `&mut [T]` is
    /// ever handed out over a block that a racy reader can hold.
    pub fn replace(&self, heap: &Heap, xs: &[T]) {
        self.install(heap, Self::from_slice(xs));
    }

    /// Swap in the block of `new`, which no other thread has seen; the old block retires on
    /// `heap`, as in [`replace`](RVec::replace).
    pub fn install(&self, heap: &Heap, new: Self) {
        let p = new.ptr.swap(ptr::null_mut(), Ordering::Relaxed);
        let old = self.ptr.swap(p, Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: `old` came from `alloc_block`; its `cap` is never written after that.
            retire::<T>(heap, old, unsafe { (*old).cap() });
        }
    }

    /// Element `i`. Panics when `i` is at or past `len`.
    pub fn get(&self, i: usize) -> T {
        let (h, len, _) = self.snapshot();
        assert!(i < len, "RVec index out of range: {i} >= {len}");
        // SAFETY: `i < len <= cap` in the live block `h`.
        unsafe { load(h, i) }
    }

    /// Element `i`, or `None` when `i` is at or past `len` (one snapshot answers both).
    pub fn try_get(&self, i: usize) -> Option<T> {
        let (h, len, _) = self.snapshot();
        // SAFETY: `i < len <= cap` in the live block `h`.
        (i < len).then(|| unsafe { load(h, i) })
    }

    /// Replace element `i`. Panics when `i` is at or past `len`.
    pub fn set(&self, i: usize, v: T) {
        let (h, len, _) = self.snapshot();
        assert!(i < len, "RVec index out of range: {i} >= {len}");
        // SAFETY: `i < len <= cap` in the live block `h`.
        unsafe { store(h, i, v) };
    }

    /// Replace the block `old` (null, or a block this vec pointed to, loaded since the caller's
    /// last safepoint) with one of at least `min_cap` slots that holds `old`'s elements. Only the
    /// thread whose swap wins retires `old`, on `heap`'s list; a loser frees its own block.
    /// Either way the caller reloads the pointer.
    fn grow(&self, heap: &Heap, old: *mut Header, min_cap: usize) {
        let (len, cap) = if old.is_null() {
            (0, 0)
        } else {
            // SAFETY: the contract above: `old` is live until the next stop.
            let o = unsafe { &*old };
            (o.len(), o.cap())
        };
        let new_cap = grown_cap(cap, min_cap);
        let new = alloc_block::<T>(new_cap);
        for i in 0..len {
            // SAFETY: `i < len <= cap` of the live block `old`, and `len <= cap < new_cap`.
            unsafe { store(new, i, load::<T>(old, i)) };
        }
        // SAFETY: `new` is this thread's own block; no other thread has seen it.
        unsafe { (*new).set_len(len) };
        match self
            .ptr
            .compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                if !old.is_null() {
                    retire::<T>(heap, old, cap);
                }
            }
            Err(_) => {
                let p = NonNull::new(new.cast::<u8>()).expect("a new block is not null");
                // SAFETY: the swap failed, so no vec points to `new` and no thread saw it.
                unsafe { free_block(p, block_layout::<T>(new_cap)) };
            }
        }
    }

    /// A block with room for `extra` more elements, and its `len`.
    fn reserve(&self, heap: &Heap, extra: usize) -> (*mut Header, usize) {
        loop {
            let (h, len, cap) = self.snapshot();
            let need = len.checked_add(extra).expect("an RVec capacity overflows");
            if need <= cap {
                return (h, len);
            }
            self.grow(heap, h, need);
        }
    }

    /// Append `v`; a grow retires the old block on `heap` (this thread's handle).
    pub fn push(&self, heap: &Heap, v: T) {
        let (h, len) = self.reserve(heap, 1);
        // SAFETY: `len < cap` in the live block `h`; the Release store publishes the slot.
        unsafe {
            store(h, len, v);
            (*h).set_len(len + 1);
        }
    }

    pub fn pop(&self) -> Option<T> {
        let (h, len, _) = self.snapshot();
        if len == 0 {
            return None;
        }
        // SAFETY: `len - 1 < len <= cap` in the live block `h`.
        unsafe {
            let v = load(h, len - 1);
            (*h).set_len(len - 1);
            Some(v)
        }
    }

    /// Insert `v` at `i`, shifting the tail right. Panics when `i > len`. A grow retires the old
    /// block on `heap`.
    pub fn insert(&self, heap: &Heap, i: usize, v: T) {
        let (h, len) = self.reserve(heap, 1);
        assert!(i <= len, "RVec insert index out of range: {i} > {len}");
        // SAFETY: every index is at most `len < cap` in the live block `h`.
        unsafe {
            for k in (i..len).rev() {
                store(h, k + 1, load::<T>(h, k));
            }
            store(h, i, v);
            (*h).set_len(len + 1);
        }
    }

    /// Remove and return element `i`, shifting the tail left. Panics when `i >= len`.
    pub fn remove(&self, i: usize) -> T {
        let (h, len, _) = self.snapshot();
        assert!(i < len, "RVec index out of range: {i} >= {len}");
        // SAFETY: every index is below `len <= cap` in the live block `h`.
        unsafe {
            let v = load(h, i);
            for k in i + 1..len {
                store(h, k - 1, load::<T>(h, k));
            }
            (*h).set_len(len - 1);
            v
        }
    }

    /// Lower `len` to `n`; frees nothing (an `Elem` has no drop).
    pub fn truncate(&self, n: usize) {
        let (h, len, _) = self.snapshot();
        if n < len {
            // SAFETY: `n < len`, so `h` is a live block.
            unsafe { (*h).set_len(n) };
        }
    }

    pub fn clear(&self) {
        self.truncate(0);
    }

    /// Append `xs`; a grow retires the old block on `heap`.
    pub fn extend_from_slice(&self, heap: &Heap, xs: &[T]) {
        if xs.is_empty() {
            return;
        }
        let (h, len) = self.reserve(heap, xs.len());
        // SAFETY: `len + xs.len() <= cap` in the live block `h`; the Release store publishes
        // the slots.
        unsafe {
            for (k, &x) in xs.iter().enumerate() {
                store(h, len + k, x);
            }
            (*h).set_len(len + xs.len());
        }
    }

    /// Random access to one snapshot of the block and its `len` (a probe of a fixed-size table,
    /// `vm/map_data.rs`). Do not hold the view across a safepoint, as for [`iter`](RVec::iter).
    pub fn view(&self) -> View<'_, T> {
        let (h, len, _) = self.snapshot();
        View {
            h,
            len,
            _v: PhantomData,
        }
    }

    /// The elements of one snapshot of the block and its `len`. Do not hold the iterator across
    /// a safepoint: a stop can free the block once a grow retired it.
    pub fn iter(&self) -> Iter<'_, T> {
        let (h, len, _) = self.snapshot();
        Iter {
            h,
            len,
            i: 0,
            _v: PhantomData,
        }
    }
}

impl<T: Elem> Clone for RVec<T> {
    /// A new block at exactly the snapshot's length.
    fn clone(&self) -> Self {
        Self::from_slice(&self.to_vec())
    }
}

impl<T: Elem + std::fmt::Debug> std::fmt::Debug for RVec<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<T: Elem> From<Vec<T>> for RVec<T> {
    fn from(v: Vec<T>) -> Self {
        Self::from_slice(&v)
    }
}

impl<T: Elem> FromIterator<T> for RVec<T> {
    fn from_iter<I: IntoIterator<Item = T>>(it: I) -> Self {
        Self::from_slice(&it.into_iter().collect::<Vec<T>>())
    }
}

impl<'a, T: Elem> IntoIterator for &'a RVec<T> {
    type Item = T;
    type IntoIter = Iter<'a, T>;
    /// See [`RVec::iter`].
    fn into_iter(self) -> Iter<'a, T> {
        self.iter()
    }
}

impl<T: Elem> Drop for RVec<T> {
    /// The owning heap object is swept (world stopped) or dropped by its only owner: free the
    /// current block at once.
    fn drop(&mut self) {
        let h = *self.ptr.get_mut();
        if let Some(p) = NonNull::new(h.cast::<u8>()) {
            // SAFETY: `&mut self`: no reader of this vec is left, and `h` came from
            // `alloc_block`; its `cap` is never written after that.
            let layout = block_layout::<T>(unsafe { (*h).cap() });
            // SAFETY: as above.
            unsafe { free_block(p, layout) };
        }
    }
}

/// See [`RVec::view`].
pub struct View<'a, T: Elem> {
    h: *mut Header,
    len: usize,
    _v: PhantomData<&'a RVec<T>>,
}

impl<T: Elem> View<'_, T> {
    pub fn len(&self) -> usize {
        self.len
    }

    /// Element `i` of the snapshot. Panics when `i` is at or past its `len`.
    pub fn get(&self, i: usize) -> T {
        assert!(i < self.len, "RVec index out of range: {i} >= {}", self.len);
        // SAFETY: `i < len <= cap` of the snapshot block, live until the next safepoint.
        unsafe { load(self.h, i) }
    }

    /// Replace element `i` of the snapshot. Panics when `i` is at or past its `len`.
    pub fn set(&self, i: usize, v: T) {
        assert!(i < self.len, "RVec index out of range: {i} >= {}", self.len);
        // SAFETY: as for `get`.
        unsafe { store(self.h, i, v) };
    }
}

/// See [`RVec::iter`].
pub struct Iter<'a, T: Elem> {
    h: *mut Header,
    len: usize,
    i: usize,
    _v: PhantomData<&'a RVec<T>>,
}

impl<T: Elem> Iterator for Iter<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.i >= self.len {
            return None;
        }
        // SAFETY: `i < len <= cap` of the snapshot block, live until the next safepoint.
        let v = unsafe { load(self.h, self.i) };
        self.i += 1;
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::{RVec, test_counts, test_serial};
    use crate::vm::heap::Heap;
    use crate::vm::value::Value;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A push past the capacity retires the old block on the handle's list. Neither the
    /// hand-off to the core nor anything else frees it; the next sweep (the stop) does.
    #[test]
    fn grow_retires_old_block_until_stw() {
        let _g = test_serial();
        let mut heap = Heap::new();
        let v: RVec<u64> = RVec::with_capacity(4);
        for i in 0..5 {
            v.push(&heap, i);
        }
        let (_, freed) = test_counts();
        assert_eq!(
            heap.retired_len(),
            (1, 0),
            "the 4-slot block is on the handle's list"
        );
        assert_eq!(
            (0..5).map(|i| v.get(i)).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        heap.adopt_retired();
        assert_eq!(
            heap.retired_len(),
            (0, 1),
            "the hand-off moves it to the core"
        );
        assert_eq!(test_counts().1, freed, "the hand-off frees nothing");
        heap.sweep();
        assert_eq!(heap.retired_len(), (0, 0));
        assert_eq!(
            test_counts().1,
            freed + 1,
            "the stop frees the retired block"
        );
    }

    /// A dropped handle hands its retired blocks to the core and frees none; the next sweep of
    /// another handle frees them.
    #[test]
    fn dropped_handle_hands_off_until_the_next_sweep() {
        let _g = test_serial();
        let mut a = Heap::new();
        // SAFETY: one thread drives both handles, and `a.sweep` runs after `b` is dropped.
        let b = unsafe { a.attach() };
        let v: RVec<u64> = RVec::with_capacity(1);
        v.push(&b, 1);
        v.push(&b, 2);
        let (_, freed) = test_counts();
        drop(b);
        assert_eq!(
            a.retired_len(),
            (0, 1),
            "the drop handed the block to the core"
        );
        assert_eq!(test_counts().1, freed, "the drop frees nothing");
        a.sweep();
        assert_eq!(test_counts().1, freed + 1, "the sweep frees it");
    }

    /// A retired block raises the run's byte count once: the hand-off and the trigger check do
    /// not charge it again, and the sweep that frees it clears it.
    #[test]
    fn retired_bytes_charge_once() {
        let _g = test_serial();
        let mut heap = Heap::new();
        let v: RVec<u64> = RVec::with_capacity(4);
        for i in 0..4 {
            v.push(&heap, i);
        }
        let before = heap.run_bytes_estimate();
        v.push(&heap, 4);
        let charged = heap.run_bytes_estimate() - before;
        assert_eq!(
            charged,
            super::block_layout::<u64>(4).size(),
            "the old block's bytes"
        );
        heap.should_collect();
        heap.adopt_retired();
        heap.should_collect();
        assert_eq!(heap.run_bytes_estimate() - before, charged, "charged once");
        heap.sweep();
        assert_eq!(
            heap.run_bytes_estimate(),
            heap.live_bytes(),
            "the sweep cleared the charge"
        );
    }

    /// One thread pushes 100k values while another reads `len` and `get`. Every read sees the
    /// value the writer stored at that index. The retired blocks are freed only after both join.
    #[test]
    fn racing_push_and_read() {
        const N: u64 = 100_000;
        let _g = test_serial();
        let mut heap = Heap::new();
        let v: RVec<u64> = RVec::new();
        let done = AtomicBool::new(false);
        let (_, freed) = test_counts();
        let reads = std::thread::scope(|s| {
            s.spawn(|| {
                super::test_track();
                for i in 0..N {
                    v.push(&heap, i);
                }
                done.store(true, Ordering::Release);
            });
            s.spawn(|| {
                let mut reads = 0u64;
                while !done.load(Ordering::Acquire) {
                    let len = v.len();
                    if len > 0 {
                        let i = (reads as usize * 7919) % len;
                        assert_eq!(v.get(i), i as u64);
                        assert_eq!(v.get(len - 1), len as u64 - 1);
                        reads += 1;
                    }
                }
                reads
            })
            .join()
            .unwrap()
        });
        assert!(reads > 0);
        assert_eq!(v.len(), N as usize);
        assert!(
            v.iter().eq(0..N),
            "the snapshot iterates every value in order"
        );
        assert_eq!(test_counts().1, freed, "no block is freed before the stop");
        heap.sweep();
        assert!(
            test_counts().1 > freed + 10,
            "each growth retired one block"
        );
    }

    /// Two threads grow from one full block at once. Exactly one installs its block and retires
    /// the old one; the other frees its own block. Every block allocated is freed.
    #[test]
    fn loser_frees_its_own_block() {
        let _g = test_serial();
        let (alloc0, freed0) = test_counts();
        let mut heap = Heap::new();
        for _ in 0..200 {
            let v: RVec<u64> = RVec::with_capacity(2);
            v.push(&heap, 1);
            v.push(&heap, 2);
            let old = v.block() as usize;
            let go = Barrier::new(2);
            std::thread::scope(|s| {
                for _ in 0..2 {
                    s.spawn(|| {
                        super::test_track();
                        go.wait();
                        v.grow(&heap, old as *mut super::Header, 3);
                    });
                }
            });
            assert_eq!(
                heap.retired_len(),
                (1, 0),
                "exactly one thread retires the old block"
            );
            assert_eq!(v.iter().collect::<Vec<_>>(), [1, 2]);
            drop(v);
            heap.sweep();
        }
        let (alloc, freed) = test_counts();
        assert_eq!(
            alloc - alloc0,
            3 * 200,
            "the first block and one new block per thread"
        );
        assert_eq!(freed - freed0, alloc - alloc0, "no block leaks");
    }

    /// The edit operations keep the order of a `Vec`, for `Value` slots too.
    #[test]
    fn edits_match_vec() {
        let _g = test_serial();
        let mut heap = Heap::new();
        let v: RVec<i64> = RVec::new();
        let mut w: Vec<i64> = Vec::new();
        v.extend_from_slice(&heap, &[1, 2, 3]);
        w.extend_from_slice(&[1, 2, 3]);
        v.insert(&heap, 0, -9);
        w.insert(0, -9);
        v.insert(&heap, 4, 8);
        w.insert(4, 8);
        assert_eq!(v.remove(1), w.remove(1));
        v.set(0, 7);
        w[0] = 7;
        assert_eq!(v.pop(), w.pop());
        assert_eq!(v.iter().collect::<Vec<_>>(), w);
        v.truncate(1);
        assert_eq!(v.iter().collect::<Vec<_>>(), [7]);
        v.clear();
        assert_eq!((v.len(), v.pop()), (0, None));
        let vals: RVec<Value> = RVec::new();
        vals.push(&heap, Value::int(-3));
        vals.push(&heap, Value::nil());
        assert!(vals.get(0) == Value::int(-3) && vals.get(1) == Value::nil());
        heap.sweep();
    }

    /// Bytes pack one per byte after the 8-byte header, and edit like a `Vec<u8>`.
    #[test]
    fn bytes_pack_one_per_byte() {
        let _g = test_serial();
        let mut heap = Heap::new();
        assert_eq!(super::block_layout::<u8>(16).size(), 8 + 16);
        let v: RVec<u8> = RVec::from_slice(b"ab");
        v.extend_from_slice(&heap, b"cdef");
        v.set(0, b'z');
        v.insert(&heap, 1, b'-');
        assert_eq!(v.pop(), Some(b'f'));
        assert_eq!(v.to_vec(), b"z-bcde");
        heap.sweep();
    }

    /// The header's `u32` capacity: a larger one panics before any allocation.
    #[test]
    #[should_panic(expected = "capacity overflows")]
    fn capacity_past_u32_panics() {
        let _ = RVec::<u64>::with_capacity(u32::MAX as usize + 1);
    }

    /// A grow near the `u32` limit stops at `u32::MAX` slots instead of doubling past it.
    #[test]
    fn growth_clamps_to_the_u32_limit() {
        let half = 1usize << 31;
        assert_eq!(super::grown_cap(half, half + 1), u32::MAX as usize);
        assert_eq!(super::grown_cap(4, 5), 8);
    }

    /// A whole-vec rewrite (sort, reverse) retires the old block like a grow; the stop frees it.
    #[test]
    fn replace_retires_old_block() {
        let _g = test_serial();
        let mut heap = Heap::new();
        let v: RVec<u64> = RVec::from_slice(&[3, 1, 2]);
        let (_, freed) = test_counts();
        v.replace(&heap, &[1, 2, 3]);
        assert_eq!(
            heap.retired_len(),
            (1, 0),
            "the old block is on the handle's list"
        );
        assert_eq!(v.to_vec(), [1, 2, 3]);
        heap.sweep();
        assert_eq!(test_counts().1, freed + 1, "the stop frees the old block");
    }

    #[test]
    #[should_panic(expected = "index out of range")]
    fn get_past_len_panics() {
        let _g = test_serial();
        let heap = Heap::new();
        let v: RVec<u64> = RVec::with_capacity(4);
        v.push(&heap, 1);
        v.get(1);
    }
}
