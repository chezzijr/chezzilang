//! A growable vector whose old buffers are retired and freed only at a stop-the-world GC.
//!
//! M2 (Go parity): two tasks may touch one container with no lock, and a race is the user's bug.
//! A racy reader must still never read freed memory. So an [`RVec`] that grows does not free its
//! old buffer: it RETIRES it, the way Go keeps the old `append` array alive until no one can
//! reach it. Only the stop-the-world collector frees a retired buffer.
//!
//! - An `RVec<T>` is one `AtomicPtr` to a [`Header`] block: one allocation, the header and then
//!   `cap` inline atomic slots, zeroed at allocation. A null pointer is the empty vec. The
//!   header's `cap` and `len` are `u64`s, as a Go slice's length is an `int`: a vec holds as
//!   many elements as a block of at most `isize::MAX` bytes has room for (`max_cap`).
//! - Allocation failure: [`RVec::try_with_capacity`] and [`RVec::try_zeroed`] return
//!   [`AllocFailed`] when the block's byte size passes `isize::MAX` or the allocator returns
//!   null, so an op that sizes a buffer from a user int (`bytearray(n)`, `[x] * n`) reports a
//!   clean fault. Every other allocation (a push, an insert, an extend) goes through the same
//!   path and panics on that error, or aborts on a null allocation, as a `Vec` does.
//! - `T` is an [`Elem`]: `Copy`, no padding, in an atomic slot of its own size (`u64`, `i64`
//!   and `Value` in an `AtomicU64`, `u8` in an `AtomicU8`, so bytes pack one per byte; a map or
//!   set entry in one `AtomicU64` per field, so a racy reader can see it torn; a map's index
//!   slot is one `u64` word, so it never tears, `vm/map_data.rs`). Every slot
//!   access is an atomic of the element, so a racing read and write on one slot is no data race
//!   and never tears. A `Value` slot stores with Release and loads with Acquire ([`VALUE_STORE`]):
//!   it is how a `GcRef` reaches another thread, and the reader must see the object it names. A
//!   byte or a `u64`/`i64`/`usize` word is Relaxed: no reader derives a pointer from it. `Copy`
//!   is also what makes a racy read sound: a reader copies the bits, and a copied `T` with a drop
//!   would drop twice.
//! - A read loads the pointer (Acquire), then that block's `len` (Acquire), then a slot below
//!   `len`. [`get`](RVec::get) and [`set`](RVec::set) panic on an index at or past `len`, and a
//!   panic is no fault `recover:` catches (in a spawned task it is "internal error: a parallel
//!   task panicked"; in the entry task it ends the process). M3.0a — so they serve only a vec
//!   whose `len` no other thread can lower (a private snapshot, a fixed block); an op on a vec
//!   that another thread can shrink checks its index against the one snapshot it reads
//!   ([`view`](RVec::view), [`try_get`](RVec::try_get), [`remove`](RVec::remove)) and gives its
//!   own out-of-range fault.
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
//! count in the same stop, which uncharges them. Each [`RetireList`] also counts the bytes it
//! holds (push adds, hand-off moves, free empties): the retired-bytes trigger reads that count,
//! with a goal of the live bytes of the last sweep, at least 1 MiB (`Heap::should_collect`).
//!
//! No `as_slice`: a `&[T]` over a block that another thread can write with `set` is not sound.
//! No API hands out `&mut [T]`. Miri and TSan are not available on this box, so the races here
//! are tested only by the stress tests below.
//!
//! [`RCell`] is the one-slot sibling for an object field written in place (a struct's inline
//! field, a cell, a cursor's position).
//!
//! [`AtomicBox`] is the owner-pointer sibling for a value that a run takes whole (a parked
//! generator): a take swaps the pointer to a RUNNING sentinel and waits out the readers, so a
//! second taker sees it running, and reads never block each other.

use super::heap::Heap;
use super::value::Value;
use std::alloc::{Layout, alloc_zeroed, dealloc, handle_alloc_error};
use std::marker::PhantomData;
use std::mem::{align_of, size_of};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, RwLock};

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
    /// A new slot that holds `v`.
    fn new_slot(v: Self) -> Self::Slot;
}

/// An 8-byte word in an `AtomicU64`, as its bits (`$to` and `$from` convert), with the
/// orderings `$load` and `$store`.
macro_rules! word_elem {
    ($t:ty, $to:expr, $from:expr, $load:expr, $store:expr) => {
        impl sealed::Sealed for $t {}
        impl Elem for $t {
            type Slot = AtomicU64;
            #[inline(always)]
            fn load(slot: &AtomicU64) -> Self {
                $from(slot.load($load))
            }
            #[inline(always)]
            fn store(slot: &AtomicU64, v: Self) {
                slot.store($to(v), $store);
            }
            #[inline(always)]
            fn new_slot(v: Self) -> AtomicU64 {
                AtomicU64::new($to(v))
            }
        }
    };
}

/// The orderings of a `Value` slot: the one place a `GcRef` is published to another thread.
/// A store is Release, so a reader that loads the ref (Acquire) also sees every write the storing
/// thread made before it: the new object's contents in its heap slot, and its own buffers. Go's
/// `publicationBarrier` after a malloc init (`runtime/malloc.go`) gives the same order. On x86
/// both are plain moves; an ARM build is where a Relaxed slot would show the bug.
pub(super) const VALUE_LOAD: Ordering = Ordering::Acquire;
pub(super) const VALUE_STORE: Ordering = Ordering::Release;

// A `u64`, `i64` or `usize` slot holds user data or a position, never a pointer that a reader
// follows, so it stays Relaxed (Go parity: a racy int read gives a wrong value).
word_elem!(
    u64,
    |v: u64| v,
    |b: u64| b,
    Ordering::Relaxed,
    Ordering::Relaxed
);
word_elem!(
    i64,
    |v: i64| v as u64,
    |b: u64| b as i64,
    Ordering::Relaxed,
    Ordering::Relaxed
);
word_elem!(
    usize,
    |v: usize| v as u64,
    |b: u64| b as usize,
    Ordering::Relaxed,
    Ordering::Relaxed
);
word_elem!(
    Value,
    Value::to_bits,
    Value::from_bits,
    VALUE_LOAD,
    VALUE_STORE
);

/// A map entry, `(cached hash, key, value)`, in one `AtomicU64` per field. A racy reader can
/// see a torn entry (fields of two writes): the user's race, like a torn Go struct.
impl sealed::Sealed for (u64, Value, Value) {}
impl Elem for (u64, Value, Value) {
    type Slot = [AtomicU64; 3];
    #[inline(always)]
    fn load([h, k, v]: &[AtomicU64; 3]) -> Self {
        (u64::load(h), Value::load(k), Value::load(v))
    }
    #[inline(always)]
    fn store([h, k, v]: &[AtomicU64; 3], (eh, ek, ev): Self) {
        u64::store(h, eh);
        Value::store(k, ek);
        Value::store(v, ev);
    }
    #[inline(always)]
    fn new_slot((h, k, v): Self) -> [AtomicU64; 3] {
        [u64::new_slot(h), Value::new_slot(k), Value::new_slot(v)]
    }
}

/// A set entry, `(cached hash, element)`, as a map entry without the value.
impl sealed::Sealed for (u64, Value) {}
impl Elem for (u64, Value) {
    type Slot = [AtomicU64; 2];
    #[inline(always)]
    fn load([h, e]: &[AtomicU64; 2]) -> Self {
        (u64::load(h), Value::load(e))
    }
    #[inline(always)]
    fn store([h, e]: &[AtomicU64; 2], (eh, ee): Self) {
        u64::store(h, eh);
        Value::store(e, ee);
    }
    #[inline(always)]
    fn new_slot((h, e): Self) -> [AtomicU64; 2] {
        [u64::new_slot(h), Value::new_slot(e)]
    }
}

/// An entry that caches its key's hash in its first word (a map or set entry): a probe reads that
/// word alone ([`View::hash`], `vm/map_data.rs`).
pub trait CachedHash: Elem {
    fn load_hash(slot: &Self::Slot) -> u64;
}

impl CachedHash for (u64, Value, Value) {
    #[inline(always)]
    fn load_hash([h, _, _]: &[AtomicU64; 3]) -> u64 {
        u64::load(h)
    }
}

impl CachedHash for (u64, Value) {
    #[inline(always)]
    fn load_hash([h, _]: &[AtomicU64; 2]) -> u64 {
        u64::load(h)
    }
}

impl sealed::Sealed for u8 {}
impl Elem for u8 {
    type Slot = AtomicU8;
    #[inline(always)]
    fn load(slot: &AtomicU8) -> Self {
        slot.load(Ordering::Relaxed)
    }
    #[inline(always)]
    fn store(slot: &AtomicU8, v: Self) {
        slot.store(v, Ordering::Relaxed);
    }
    #[inline(always)]
    fn new_slot(v: Self) -> AtomicU8 {
        AtomicU8::new(v)
    }
}

/// One element in one atomic slot: the single-word sibling of an [`RVec`], for an object field
/// that an op writes in place while another thread can read it (a struct's inline field, a cell,
/// a cursor's position). A read and a write take `&self` and go through the element's own
/// [`Elem`] load and store, so a `Value` cell publishes with Release as an `RVec` slot does.
pub struct RCell<T: Elem>(T::Slot);

impl<T: Elem> RCell<T> {
    #[inline(always)]
    pub fn new(v: T) -> Self {
        RCell(T::new_slot(v))
    }

    #[inline(always)]
    pub fn get(&self) -> T {
        T::load(&self.0)
    }

    #[inline(always)]
    pub fn set(&self, v: T) {
        T::store(&self.0, v)
    }
}

impl<T: Elem> Clone for RCell<T> {
    fn clone(&self) -> Self {
        RCell::new(self.get())
    }
}

impl<T: Elem + std::fmt::Debug> std::fmt::Debug for RCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.get().fmt(f)
    }
}

/// The head of one block. The slots follow it in the same allocation, at `data_offset()`.
///
/// The header is 16 bytes (`u64` fields), so a block of two 8-byte slots is 32 bytes.
#[repr(C)]
pub(super) struct Header {
    cap: u64,
    len: AtomicU64,
}

impl Header {
    #[inline(always)]
    fn cap(&self) -> usize {
        self.cap as usize
    }

    #[inline(always)]
    fn len(&self) -> usize {
        self.len.load(Ordering::Acquire) as usize
    }

    /// Publish `n` elements (Release: the slots below `n` are written first).
    #[inline(always)]
    fn set_len(&self, n: usize) {
        self.len.store(n as u64, Ordering::Release);
    }
}

/// The offset of slot 0 from the block's start (the same for every `cap`): the header, padded to
/// the slot's alignment, as `Layout::extend` places it. A constant, so a slot access in a debug
/// build does not run the layout math (it was most of a list op's time there).
#[inline(always)]
fn data_offset<T: Elem>() -> usize {
    const { size_of::<Header>().next_multiple_of(align_of::<T::Slot>()) }
}

/// The layout of a block of `cap` slots, or `None` when its byte size passes `isize::MAX`.
fn try_block_layout<T: Elem>(cap: usize) -> Option<Layout> {
    let data = Layout::array::<T::Slot>(cap).ok()?;
    Some(Layout::new::<Header>().extend(data).ok()?.0.pad_to_align())
}

/// The layout of a block that `alloc_block` allocated, so its size fits.
fn block_layout<T: Elem>(cap: usize) -> Layout {
    try_block_layout::<T>(cap).unwrap_or_else(|| capacity_overflow())
}

/// The fault of a vec that cannot hold one more element (a size past `isize::MAX` bytes, or a
/// map's index past its position field, `vm/map_data.rs`): a panic, as for a `Vec`.
#[cold]
pub(super) fn capacity_overflow() -> ! {
    panic!("an RVec capacity overflows")
}

/// The largest capacity whose block fits `isize::MAX` bytes, after the header and the padding
/// to the header's alignment.
fn max_cap<T: Elem>() -> usize {
    (isize::MAX as usize - data_offset::<T>() - (align_of::<Header>() - 1)) / size_of::<T::Slot>()
}

/// A block of `cap` slots could not be allocated: its byte size passes `isize::MAX`, or the
/// allocator returned null.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AllocFailed;

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

/// A new block of `cap` zeroed slots, `len` 0, or [`AllocFailed`] (the one allocation path).
fn try_alloc_block<T: Elem>(cap: usize) -> Result<*mut Header, AllocFailed> {
    let layout = try_block_layout::<T>(cap).ok_or(AllocFailed)?;
    // SAFETY: `layout` has a non-zero size (the header is 16 bytes).
    let p = unsafe { alloc_zeroed(layout) }.cast::<Header>();
    if p.is_null() {
        return Err(AllocFailed);
    }
    // SAFETY: `p` is a fresh allocation with room and alignment for a `Header`; the zeroed slots
    // after it are valid `T::Slot`s.
    unsafe {
        p.write(Header {
            cap: cap as u64,
            len: AtomicU64::new(0),
        })
    };
    #[cfg(test)]
    count(&ALLOCS);
    Ok(p)
}

/// A new block of `cap` zeroed slots, `len` 0. A size past `isize::MAX` panics; a null
/// allocation aborts, as for a `Vec`.
fn alloc_block<T: Elem>(cap: usize) -> *mut Header {
    try_alloc_block::<T>(cap).unwrap_or_else(|AllocFailed| match try_block_layout::<T>(cap) {
        Some(layout) => handle_alloc_error(layout),
        None => capacity_overflow(),
    })
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
#[inline(always)]
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
#[inline(always)]
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
#[inline(always)]
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
    heap.retire_list().push(Retired {
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
///
/// `bytes` is the one count of the retired bytes the list holds: a push adds the block's size, a
/// hand-off moves it, and the free empties it. The retired-bytes GC trigger reads it
/// (`Heap::should_collect`). It changes only under the list's lock, so a read without the lock is
/// at most one block stale.
#[derive(Default)]
pub(super) struct RetireList {
    blocks: Mutex<Vec<Retired>>,
    bytes: AtomicUsize,
}

impl std::fmt::Debug for RetireList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RetireList({})", self.len())
    }
}

impl RetireList {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Retired>> {
        self.blocks.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Put `b` on the list.
    fn push(&self, b: Retired) {
        let mut blocks = self.lock();
        self.bytes.fetch_add(b.layout.size(), Ordering::Relaxed);
        blocks.push(b);
    }

    /// Move every block of `self` onto `to`.
    pub(super) fn move_into(&self, to: &RetireList) {
        let (blocks, n) = {
            let mut own = self.lock();
            (
                std::mem::take(&mut *own),
                self.bytes.swap(0, Ordering::Relaxed),
            )
        };
        if !blocks.is_empty() {
            let mut theirs = to.lock();
            to.bytes.fetch_add(n, Ordering::Relaxed);
            theirs.extend(blocks);
        }
    }

    /// The bytes of the blocks on this list.
    #[inline]
    pub(super) fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
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
    /// pointer to a block on this list: a parked reader dropped it at its safepoint, and a
    /// detached thread's door body gets a `Detached` handle, which reaches no heap object
    /// (`vm/gc_door.rs`, M3.0a).
    pub(super) unsafe fn free_all(&self) {
        let blocks = {
            let mut own = self.lock();
            self.bytes.store(0, Ordering::Relaxed);
            std::mem::take(&mut *own)
        };
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
/// clamped to [`max_cap`] (a `min_cap` past it still panics in `alloc_block`).
fn grown_cap<T: Elem>(cap: usize, min_cap: usize) -> usize {
    let doubled = cap.saturating_mul(2).min(max_cap::<T>());
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

    /// An empty vec with room for `cap` elements, or [`AllocFailed`] when that block cannot be
    /// allocated (see the module doc).
    pub fn try_with_capacity(cap: usize) -> Result<Self, AllocFailed> {
        let p = if cap == 0 {
            ptr::null_mut()
        } else {
            try_alloc_block::<T>(cap)?
        };
        Ok(RVec {
            ptr: AtomicPtr::new(p),
            _t: PhantomData,
        })
    }

    /// The current block (null when empty).
    #[inline]
    fn block(&self) -> *mut Header {
        self.ptr.load(Ordering::Acquire)
    }

    /// The current block, its `len` and its `cap`.
    #[inline]
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

    #[inline]
    pub fn len(&self) -> usize {
        self.snapshot().1
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The slots of the current block (`live_bytes` reads it). Reads no `len`.
    #[inline]
    pub fn capacity(&self) -> usize {
        let h = self.block();
        if h.is_null() {
            return 0;
        }
        // SAFETY: as in `snapshot`; `cap` is never written after `alloc_block`.
        unsafe { (*h).cap() }
    }

    /// A vec of `n` zeroed elements (a zeroed slot is a valid `T`, see [`Elem`]).
    pub fn zeroed(n: usize) -> Self {
        Self::fill_zeroed(Self::with_capacity(n), n)
    }

    /// [`zeroed`](RVec::zeroed), or [`AllocFailed`] when the block cannot be allocated.
    pub fn try_zeroed(n: usize) -> Result<Self, AllocFailed> {
        Ok(Self::fill_zeroed(Self::try_with_capacity(n)?, n))
    }

    /// Publish the `n` zeroed slots of `v`, a new vec with a block of exactly `n` slots.
    fn fill_zeroed(v: Self, n: usize) -> Self {
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
    #[inline]
    pub fn get(&self, i: usize) -> T {
        let (h, len, _) = self.snapshot();
        assert!(i < len, "RVec index out of range: {i} >= {len}");
        // SAFETY: `i < len <= cap` in the live block `h`.
        unsafe { load(h, i) }
    }

    /// Element `i`, or `None` when `i` is at or past `len` (one snapshot answers both).
    #[inline]
    pub fn try_get(&self, i: usize) -> Option<T> {
        let (h, len, _) = self.snapshot();
        // SAFETY: `i < len <= cap` in the live block `h`.
        (i < len).then(|| unsafe { load(h, i) })
    }

    /// Replace element `i`. Panics when `i` is at or past `len`.
    #[inline]
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
        let new_cap = grown_cap::<T>(cap, min_cap);
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
    #[inline]
    fn reserve(&self, heap: &Heap, extra: usize) -> (*mut Header, usize) {
        loop {
            let (h, len, cap) = self.snapshot();
            let need = len
                .checked_add(extra)
                .unwrap_or_else(|| capacity_overflow());
            if need <= cap {
                return (h, len);
            }
            self.grow(heap, h, need);
        }
    }

    /// Append `v`; a grow retires the old block on `heap` (this thread's handle).
    #[inline]
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

    /// Insert `v` at `i`, shifting the tail right; an `i` past `len` inserts at `len`, as
    /// Python's `insert` clamps (a caller clamps to the `len` it read, and another thread can
    /// shrink the vec after that read). A grow retires the old block on `heap`.
    pub fn insert(&self, heap: &Heap, i: usize, v: T) {
        let (h, len) = self.reserve(heap, 1);
        let i = i.min(len);
        // SAFETY: every index is at most `len < cap` in the live block `h`.
        unsafe {
            for k in (i..len).rev() {
                store(h, k + 1, load::<T>(h, k));
            }
            store(h, i, v);
            (*h).set_len(len + 1);
        }
    }

    /// Remove and return element `i`, shifting the tail left, or `None` when `i` is at or past
    /// `len` (one snapshot answers both).
    pub fn remove(&self, i: usize) -> Option<T> {
        let (h, len, _) = self.snapshot();
        if i >= len {
            return None;
        }
        // SAFETY: every index is below `len <= cap` in the live block `h`.
        unsafe {
            let v = load(h, i);
            for k in i + 1..len {
                store(h, k - 1, load::<T>(h, k));
            }
            (*h).set_len(len - 1);
            Some(v)
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
    /// A new block at exactly the snapshot's length, copied block to block (no temporary).
    fn clone(&self) -> Self {
        self.iter().collect()
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
    /// An iterator that reports an exact size (`size_hint` lower == upper) is written straight
    /// into a block of that size, with no temporary `Vec` (a closure's captures, `MakeClosure`).
    /// The hint is not trusted: at most that many elements are taken, and `len` is the count
    /// actually written.
    fn from_iter<I: IntoIterator<Item = T>>(it: I) -> Self {
        let it = it.into_iter();
        match it.size_hint() {
            (lo, Some(hi)) if lo == hi => {
                let v = Self::with_capacity(lo);
                let (h, _, _) = v.snapshot();
                let mut n = 0;
                for x in it.take(lo) {
                    // SAFETY: `n < lo`, the capacity of this vec's own new block `h`; no other
                    // thread has seen it.
                    unsafe { store(h, n, x) };
                    n += 1;
                }
                if !h.is_null() {
                    // SAFETY: as above; the slots below `n` are written.
                    unsafe { (*h).set_len(n) };
                }
                v
            }
            _ => Self::from_slice(&it.collect::<Vec<T>>()),
        }
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

impl<T: CachedHash> View<'_, T> {
    /// The cached hash of element `i` of the snapshot (its first word only). Panics when `i` is
    /// at or past its `len`.
    #[inline(always)]
    pub fn hash(&self, i: usize) -> u64 {
        assert!(i < self.len, "RVec index out of range: {i} >= {}", self.len);
        let () = T::FITS_SLOT;
        // SAFETY: `i < len <= cap` of the snapshot block, live until the next safepoint.
        T::load_hash(unsafe { slot::<T>(self.h, i) })
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

    #[inline(always)]
    fn next(&mut self) -> Option<T> {
        if self.i >= self.len {
            return None;
        }
        // SAFETY: `i < len <= cap` of the snapshot block, live until the next safepoint.
        let v = unsafe { load(self.h, self.i) };
        self.i += 1;
        Some(v)
    }

    /// Exact: the snapshot's `len` is fixed, so a collect sizes its block once.
    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = self.len - self.i;
        (n, Some(n))
    }
}

impl<T: Elem> ExactSizeIterator for Iter<'_, T> {}

/// One boxed `T` behind an atomic owner pointer, with two kinds of access. A parked generator
/// lives here (`vm/exec.rs`), so two threads that resume one generator never both write its
/// frames, and any number of threads can read it at once.
///
/// - A RUN takes the box whole ([`take`](AtomicBox::take)): it swaps the pointer to the RUNNING
///   sentinel (null), then waits until no reader is left. A second take sees RUNNING and gets
///   `None`. [`put`](AtomicBox::put) stores the box back.
/// - A READ ([`with`](AtomicBox::with)) registers in `readers`, then loads the pointer. RUNNING
///   gives the reader `None`; any other pointer is a parked box that no run can mutate or free
///   until the reader unregisters. Reads never wait for each other.
///
/// Every counter and pointer access is `SeqCst`: the read side is "add to `readers`, then load
/// the pointer", and the run side is "swap the pointer, then load `readers`" (Dekker's pattern).
/// In the one total order, a reader that loaded the parked pointer added itself before the swap,
/// so the run's later load of `readers` sees it and waits. Acquire/Release does not order a
/// store before a later load of another location, so it is not enough here.
///
/// The wait is a spin: a read body is plain Rust that never reaches a safepoint or a lock, so a
/// reader leaves in bounded time.
pub struct AtomicBox<T> {
    ptr: AtomicPtr<T>,
    readers: AtomicU32,
    /// `Send` and `Sync` as an `RwLock<T>`: a take hands the `T` to the taking thread, and `with`
    /// hands `&T` to many threads at once, so `Sync` needs `T: Send + Sync`.
    _t: PhantomData<RwLock<T>>,
}

/// Unregisters one reader of an [`AtomicBox`] when dropped, on the unwind of a panicking read too.
struct ReadGuard<'a>(&'a AtomicU32);

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<T> AtomicBox<T> {
    pub fn new(v: T) -> Self {
        AtomicBox {
            ptr: AtomicPtr::new(Box::into_raw(Box::new(v))),
            readers: AtomicU32::new(0),
            _t: PhantomData,
        }
    }

    /// Spin until no reader holds the box (see the type doc for why the wait is bounded).
    fn wait_for_readers(&self) {
        while self.readers.load(Ordering::SeqCst) != 0 {
            std::hint::spin_loop();
        }
    }

    /// Take the box for a run, or `None` when a run holds it.
    pub fn take(&self) -> Option<Box<T>> {
        let p = self.ptr.swap(ptr::null_mut(), Ordering::SeqCst);
        if p.is_null() {
            return None;
        }
        self.wait_for_readers();
        // SAFETY: a non-null pointer came from `Box::into_raw`; the swap made this thread its
        // only owner, and every reader that loaded it has left (the type doc).
        Some(unsafe { Box::from_raw(p) })
    }

    /// Put `b` back. A box that was there (a put with no take before it) is dropped once its
    /// readers have left.
    pub fn put(&self, b: Box<T>) {
        let old = self.ptr.swap(Box::into_raw(b), Ordering::SeqCst);
        if !old.is_null() {
            self.wait_for_readers();
            // SAFETY: as for `take`.
            drop(unsafe { Box::from_raw(old) });
        }
    }

    /// Read the box (`None` while a run holds it). Many reads run at once; a panic in `f`
    /// unregisters the reader.
    pub fn with<R>(&self, f: impl FnOnce(Option<&T>) -> R) -> R {
        self.readers.fetch_add(1, Ordering::SeqCst);
        let _g = ReadGuard(&self.readers);
        let p = self.ptr.load(Ordering::SeqCst);
        // SAFETY: a non-null pointer is a parked box; this reader is registered, so no take or
        // put frees or mutates it before `_g` drops (the type doc).
        f(unsafe { p.as_ref() })
    }
}

impl<T: Clone> Clone for AtomicBox<T> {
    /// A copy of the parked box, read alongside any other reader. While a run holds the box the
    /// copy is RUNNING too and holds no state, so such a copy must stay a temporary (it never
    /// gets a put).
    fn clone(&self) -> Self {
        let p = self
            .with(|b| b.map(|v| Box::into_raw(Box::new(v.clone()))))
            .unwrap_or(ptr::null_mut());
        AtomicBox {
            ptr: AtomicPtr::new(p),
            readers: AtomicU32::new(0),
            _t: PhantomData,
        }
    }
}

impl<T> Drop for AtomicBox<T> {
    fn drop(&mut self) {
        let p = *self.ptr.get_mut();
        if !p.is_null() {
            // SAFETY: `&mut self`; the pointer came from `Box::into_raw`.
            drop(unsafe { Box::from_raw(p) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{RVec, test_counts, test_serial};
    use crate::vm::heap::Heap;
    use crate::vm::value::Value;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// M3.0b (C4) — a `Value` slot publishes a `GcRef` with Release and reads it with Acquire, in
    /// an `RVec` block, a map entry and an [`RCell`](super::RCell) alike: all go through
    /// `<Value as Elem>`. x86 orders a Relaxed store the same, so no runtime race test shows the
    /// bug there; an ARM build would. This pins the orderings and reads a stored value back.
    #[test]
    fn value_slots_publish_with_release_and_read_with_acquire() {
        use super::{Elem, RCell, VALUE_LOAD, VALUE_STORE};
        assert_eq!(VALUE_STORE, Ordering::Release);
        assert_eq!(VALUE_LOAD, Ordering::Acquire);
        let v = Value::int(7);
        let slot = <Value as Elem>::new_slot(Value::nil());
        <Value as Elem>::store(&slot, v);
        assert_eq!(<Value as Elem>::load(&slot).to_bits(), v.to_bits());
        let cell = RCell::new(Value::nil());
        cell.set(v);
        assert_eq!(cell.get().to_bits(), v.to_bits());
        assert_eq!(cell.clone().get().to_bits(), v.to_bits());
        let entry = <(u64, Value, Value) as Elem>::new_slot((1, v, v));
        let (h, k, x) = <(u64, Value, Value) as Elem>::load(&entry);
        assert_eq!((h, k.to_bits(), x.to_bits()), (1, v.to_bits(), v.to_bits()));
    }

    /// The constant slot offset is where `Layout::extend` puts the first slot, for every element
    /// type (the block layout reads `Layout`, a slot access reads the constant).
    #[test]
    fn data_offset_matches_the_block_layout() {
        use super::{Elem, Header, data_offset};
        use crate::vm::map_data::{IndexSlot, MapEntry, SetEntry};
        use std::alloc::Layout;
        fn check<T: Elem>() {
            let by_layout = Layout::new::<Header>()
                .extend(Layout::new::<T::Slot>())
                .unwrap()
                .1;
            assert_eq!(
                data_offset::<T>(),
                by_layout,
                "{}",
                std::any::type_name::<T>()
            );
        }
        check::<u8>();
        check::<u64>();
        check::<i64>();
        check::<Value>();
        check::<MapEntry>();
        check::<SetEntry>();
        check::<IndexSlot>();
    }

    /// An iterator whose size hint is exact is collected straight into one block of that size.
    /// The hint is not trusted: a short iterator publishes only what it wrote, and a long one
    /// stops at the hint.
    #[test]
    fn collect_sizes_one_block_from_an_exact_hint() {
        struct Lying(std::ops::Range<u64>, usize);
        impl Iterator for Lying {
            type Item = u64;
            fn next(&mut self) -> Option<u64> {
                self.0.next()
            }
            fn size_hint(&self) -> (usize, Option<usize>) {
                (self.1, Some(self.1))
            }
        }
        let _g = test_serial();
        let (allocs, _) = test_counts();
        let v: RVec<u64> = (0..5).collect();
        assert_eq!(test_counts().0 - allocs, 1, "one block, no temporary");
        assert_eq!(
            (v.len(), v.capacity(), v.to_vec()),
            (5, 5, vec![0, 1, 2, 3, 4])
        );
        let short: RVec<u64> = Lying(0..3, 5).collect();
        assert_eq!((short.len(), short.to_vec()), (3, vec![0, 1, 2]));
        let long: RVec<u64> = Lying(0..9, 2).collect();
        assert_eq!((long.len(), long.to_vec()), (2, vec![0, 1]));
        let copy = v.clone();
        assert_eq!(
            (copy.len(), copy.capacity(), copy.to_vec()),
            (5, 5, v.to_vec())
        );
        let empty: RVec<u64> = std::iter::empty().collect();
        assert_eq!((empty.len(), empty.capacity()), (0, 0));
    }

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

    /// A retire list counts the bytes it holds: a grow adds the old block's size to the handle's
    /// list, the hand-off moves them to the run's list, and the sweep empties both.
    #[test]
    fn retire_lists_count_their_bytes() {
        let _g = test_serial();
        let mut heap = Heap::new();
        let v: RVec<u64> = RVec::with_capacity(4);
        for i in 0..5 {
            v.push(&heap, i);
        }
        let size = super::block_layout::<u64>(4).size();
        assert_eq!(heap.retire_list().bytes(), size, "the grow");
        heap.adopt_retired();
        assert_eq!(heap.retire_list().bytes(), 0, "moved off the handle");
        assert_eq!(heap.core_retired_bytes(), size, "onto the run");
        heap.sweep();
        assert_eq!(heap.core_retired_bytes(), 0, "freed");
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
        assert_eq!(v.remove(1), Some(w.remove(1)));
        assert_eq!(v.remove(4), None, "past len");
        v.insert(&heap, 9, 5);
        w.insert(4, 5);
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

    /// Bytes pack one per byte after the 16-byte header, and edit like a `Vec<u8>`.
    #[test]
    fn bytes_pack_one_per_byte() {
        let _g = test_serial();
        let mut heap = Heap::new();
        assert_eq!(super::block_layout::<u8>(16).size(), 16 + 16);
        let v: RVec<u8> = RVec::from_slice(b"ab");
        v.extend_from_slice(&heap, b"cdef");
        v.set(0, b'z');
        v.insert(&heap, 1, b'-');
        assert_eq!(v.pop(), Some(b'f'));
        assert_eq!(v.to_vec(), b"z-bcde");
        heap.sweep();
    }

    /// A length past `u32::MAX` fits the 64-bit header (Go's slice `int`). The block is zeroed by
    /// the allocator, so only the page of the last byte is touched.
    #[test]
    fn lengths_past_u32_fit_the_header() {
        let n = u32::MAX as usize + 2;
        let v = RVec::<u8>::zeroed(n);
        v.set(n - 1, 7);
        assert_eq!((v.len(), v.capacity(), v.get(n - 1)), (n, n, 7));
    }

    /// `max_cap` is the last capacity whose block fits `isize::MAX` bytes, for a byte slot and a
    /// 24-byte map entry; a doubling clamps to it, and a doubling past `u32::MAX` is not clamped.
    #[test]
    fn growth_clamps_to_the_largest_block() {
        fn edge<T: super::Elem>() {
            let m = super::max_cap::<T>();
            assert!(super::try_block_layout::<T>(m).is_some());
            assert!(super::try_block_layout::<T>(m + 1).is_none());
            assert_eq!(super::grown_cap::<T>(m - 1, m), m);
            assert_eq!(
                RVec::<T>::try_with_capacity(m + 1).err(),
                Some(super::AllocFailed)
            );
        }
        edge::<u8>();
        edge::<(u64, Value, Value)>();
        let big = u32::MAX as usize;
        assert_eq!(super::grown_cap::<u8>(big, big + 1), 2 * big);
        assert_eq!(super::grown_cap::<u64>(4, 5), 8);
    }

    /// A size that fits a layout but not the allocator is an error, not an abort.
    #[test]
    fn a_failed_allocation_is_an_error() {
        let n = 1usize << 61;
        assert_eq!(RVec::<u8>::try_zeroed(n).err(), Some(super::AllocFailed));
        assert_eq!(RVec::<u64>::try_zeroed(2).map(|v| v.len()), Ok(2));
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

    /// M2.4 fix 1 — reads of one `AtomicBox` run at once: no read sees another read as a take.
    #[test]
    fn concurrent_reads_never_see_a_take() {
        let b = super::AtomicBox::new(7u64);
        let missed = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    for _ in 0..100_000 {
                        if b.with(|v| v.copied()) != Some(7) {
                            missed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        assert_eq!(
            missed.load(Ordering::Relaxed),
            0,
            "a read saw the box taken"
        );
    }

    /// A take waits for a read that is in progress, then owns the box; a read after the take
    /// sees it taken.
    #[test]
    fn take_waits_for_a_read_in_progress() {
        let b = super::AtomicBox::new(1u64);
        let inside = AtomicBool::new(false);
        let read_done = AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                b.with(|v| {
                    assert_eq!(v.copied(), Some(1));
                    inside.store(true, Ordering::SeqCst);
                    let t = std::time::Instant::now();
                    while t.elapsed() < std::time::Duration::from_millis(50) {
                        std::hint::spin_loop();
                    }
                    read_done.store(true, Ordering::SeqCst);
                });
            });
            while !inside.load(Ordering::SeqCst) {
                std::hint::spin_loop();
            }
            let taken = b.take().expect("the take owns the box");
            assert!(
                read_done.load(Ordering::SeqCst),
                "the take waited for the read"
            );
            assert_eq!(
                b.with(|v| v.copied()),
                None,
                "a read after the take sees it taken"
            );
            b.put(taken);
        });
        assert_eq!(b.with(|v| v.copied()), Some(1));
    }

    /// A panic inside a read leaves the box in place (the reader unregisters on the unwind).
    #[test]
    fn a_panic_inside_a_read_keeps_the_box() {
        let b = super::AtomicBox::new(3u64);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            b.with(|_| panic!("a read panics"));
        }));
        assert!(r.is_err());
        assert_eq!(b.take().map(|v| *v), Some(3), "the box is still parked");
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
