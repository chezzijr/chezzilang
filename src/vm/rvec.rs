//! A growable vector whose old buffers are retired and freed only at a stop-the-world GC.
//!
//! M2 (Go parity): two tasks may touch one container with no lock, and a race is the user's bug.
//! A racy reader must still never read freed memory. So an [`RVec`] that grows does not free its
//! old buffer: it RETIRES it, the way Go keeps the old `append` array alive until no one can
//! reach it. Only the stop-the-world collector frees a retired buffer.
//!
//! - An `RVec<T>` is one `AtomicPtr` to a [`Header`] block: one allocation, the header and then
//!   `cap` inline slots. A null pointer is the empty vec with no block.
//! - A read loads the pointer (Acquire), then that block's `len` (Acquire), then a slot below
//!   `len`. An index at or past `len` panics; the VM's `catch_unwind` turns a racy index into a
//!   task fault.
//! - A push into spare capacity writes the slot, then stores `len + 1` (Release). A grow
//!   allocates a block of twice the capacity, copies the elements, and swaps the pointer with a
//!   `compare_exchange` (AcqRel). The winner retires the old block; a loser frees its own new
//!   block (no one else saw it) and retries.
//! - A shrink frees nothing: `truncate` lowers `len`. `T: Copy`, so no element has a drop. The
//!   bound is also what makes a racy read sound: a reader copies the bits of a slot, and a copied
//!   `T` with a drop would drop twice.
//! - Slot access: a `T` of 8 bytes and 8-byte alignment (the VM's `Value`) goes through a
//!   `Relaxed` `AtomicU64`, so a racing read and write on one slot is no data race and never
//!   tears. A `T` of another size uses plain reads and writes: such an `RVec` must have one
//!   accessor at a time.
//!
//! Retire lists: a retired block goes on a thread-local list ([`retire`]). [`RetireList::adopt_local`]
//! moves that list to the run's `HeapCore.retired` when the thread stops for a GC
//! (`Vm::gc_park`), detaches (`Vm::gc_blocking`, `Vm::gc_reenter`), leaves (`Vm::gc_leave`), or
//! sweeps (`Heap::sweep`). The sweep frees every block on the core's list with the world still
//! stopped ([`RetireList::free_all`]). That is sound because a reader holds a block pointer only
//! between two safepoints: an [`Iter`] or a value read never lives across a poll or a door.
//! A list a thread still holds when it exits is freed by the thread-local destructor: a thread
//! that exits holds no block pointer, and every runner thread hands its list to the core when it
//! leaves the heap.
//!
//! Pacing: a retired block's bytes count as allocated bytes. They reach the run's byte count at
//! the handle's next slow trigger check (`Heap::should_collect`, [`take_local_bytes`]) or at the
//! hand-off ([`RetireList::adopt_local`]), whichever comes first. The sweep that frees the block
//! also zeroes the run's byte count, which uncharges them.
//!
//! No `as_slice`: a `&[T]` over a block that another thread can write with `set` is not sound.
//! No API hands out `&mut [T]`. Miri and TSan are not available on this box, so the races here
//! are tested only by the stress tests below.

use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};
use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::mem::{align_of, size_of};
use std::ptr::{self, NonNull};
use std::sync::Mutex;
use std::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

/// The head of one block. The slots follow it in the same allocation, at `data_offset::<T>()`.
#[repr(C)]
pub(super) struct Header {
    cap: usize,
    len: AtomicUsize,
}

/// The offset of slot 0 from the block's start (the same for every `cap`).
fn data_offset<T>() -> usize {
    Layout::new::<Header>()
        .extend(Layout::new::<T>())
        .expect("an RVec block layout")
        .1
}

fn block_layout<T>(cap: usize) -> Layout {
    let data = Layout::array::<T>(cap).expect("an RVec capacity overflows");
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

/// Blocks allocated and blocks freed so far, process-wide (tests only).
#[cfg(test)]
pub(super) fn test_counts() -> (usize, usize) {
    (ALLOCS.load(Ordering::SeqCst), FREES.load(Ordering::SeqCst))
}

/// A new block of `cap` slots, `len` 0.
fn alloc_block<T>(cap: usize) -> *mut Header {
    let layout = block_layout::<T>(cap);
    // SAFETY: `layout` has a non-zero size (the header is two words).
    let p = unsafe { alloc(layout) }.cast::<Header>();
    if p.is_null() {
        handle_alloc_error(layout);
    }
    // SAFETY: `p` is a fresh allocation with room and alignment for a `Header`.
    unsafe {
        p.write(Header {
            cap,
            len: AtomicUsize::new(0),
        })
    };
    #[cfg(test)]
    ALLOCS.fetch_add(1, Ordering::SeqCst);
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
    FREES.fetch_add(1, Ordering::SeqCst);
}

/// Slot `i` of block `h`.
///
/// # Safety
///
/// `h` is a live block of `T` slots and `i < cap`.
unsafe fn slot<T>(h: *mut Header, i: usize) -> *mut T {
    // SAFETY: the caller's contract: the slot lies inside the block's allocation.
    unsafe { h.cast::<u8>().add(data_offset::<T>()).cast::<T>().add(i) }
}

/// Whether `T` goes through an `AtomicU64` (see the module doc).
const fn word_sized<T>() -> bool {
    size_of::<T>() == 8 && align_of::<T>() == 8
}

/// Read slot `i` of `h`.
///
/// # Safety
///
/// `h` is a live block of `T` slots, and slot `i` was written (`i` is below a `len` that this
/// thread read with Acquire, or that it wrote).
unsafe fn load<T: Copy>(h: *mut Header, i: usize) -> T {
    // SAFETY: the caller's contract.
    let p = unsafe { slot::<T>(h, i) };
    if word_sized::<T>() {
        // SAFETY: `p` is valid and 8-byte aligned; every access to a word-sized slot is atomic.
        let bits = unsafe { AtomicU64::from_ptr(p.cast::<u64>()) }.load(Ordering::Relaxed);
        // SAFETY: the bits are a whole `T` that a `store` wrote, and `T: Copy`.
        unsafe { std::mem::transmute_copy::<u64, T>(&bits) }
    } else {
        // SAFETY: `p` is valid and written; a non-word `T` has one accessor at a time.
        unsafe { p.read() }
    }
}

/// Write `v` into slot `i` of `h`.
///
/// # Safety
///
/// `h` is a live block of `T` slots and `i < cap`.
unsafe fn store<T: Copy>(h: *mut Header, i: usize, v: T) {
    // SAFETY: the caller's contract.
    let p = unsafe { slot::<T>(h, i) };
    if word_sized::<T>() {
        // SAFETY: `T` is 8 bytes, so its bits fit a `u64`.
        let bits = unsafe { std::mem::transmute_copy::<T, u64>(&v) };
        // SAFETY: `p` is valid and 8-byte aligned; every access to a word-sized slot is atomic.
        unsafe { AtomicU64::from_ptr(p.cast::<u64>()) }.store(bits, Ordering::Relaxed);
    } else {
        // SAFETY: `p` is valid; a non-word `T` has one accessor at a time.
        unsafe { p.write(v) };
    }
}

/// A block retired by a grow, type-erased: freeing it needs only its layout (`T: Copy`).
struct RetiredBlock {
    ptr: NonNull<u8>,
    layout: Layout,
}

// SAFETY: a retired block is plain memory that no `RVec` points to; the list that holds it is
// the only thing that frees it, on whichever thread drains the list.
unsafe impl Send for RetiredBlock {}

/// This thread's retired blocks, not yet handed to a core. Its drop (thread exit) frees them.
struct LocalList(Vec<RetiredBlock>);

impl Drop for LocalList {
    fn drop(&mut self) {
        for b in self.0.drain(..) {
            // SAFETY: the thread exits, so it holds no block pointer (see the module doc).
            unsafe { free_block(b.ptr, b.layout) };
        }
    }
}

thread_local! {
    static LOCAL: RefCell<LocalList> = const { RefCell::new(LocalList(Vec::new())) };
    /// Bytes of this thread's retired blocks not yet added to a run's byte count.
    static LOCAL_BYTES: Cell<usize> = const { Cell::new(0) };
}

/// Put block `h` (of `T` slots) on this thread's retire list.
fn retire<T>(h: *mut Header) {
    // SAFETY: `h` is a live block; its `cap` is never written after `alloc_block`.
    let layout = block_layout::<T>(unsafe { (*h).cap });
    let ptr = NonNull::new(h.cast::<u8>()).expect("a retired block is not null");
    LOCAL.with_borrow_mut(|l| l.0.push(RetiredBlock { ptr, layout }));
    LOCAL_BYTES.set(LOCAL_BYTES.get().saturating_add(layout.size()));
}

/// Take this thread's retired bytes that no run's byte count holds yet (see the module doc).
pub(super) fn take_local_bytes() -> usize {
    LOCAL_BYTES.take()
}

/// How many blocks this thread has retired and not yet handed off (tests only).
#[cfg(test)]
pub(super) fn local_retired() -> usize {
    LOCAL.with_borrow(|l| l.0.len())
}

/// A run's retired blocks (`HeapCore.retired`): the stop-the-world collector frees them.
#[derive(Default)]
pub(super) struct RetireList(Mutex<Vec<RetiredBlock>>);

impl std::fmt::Debug for RetireList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RetireList({})", self.len())
    }
}

impl RetireList {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<RetiredBlock>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Move this thread's retired blocks onto this list. Returns their bytes that no run's byte
    /// count holds yet ([`take_local_bytes`]), for the caller to charge.
    pub(super) fn adopt_local(&self) -> usize {
        let blocks = LOCAL.with_borrow_mut(|l| std::mem::take(&mut l.0));
        if !blocks.is_empty() {
            self.lock().extend(blocks);
        }
        take_local_bytes()
    }

    /// Blocks on this list.
    pub(super) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Free every block on this list. Call it only with the world stopped (the sweep of
    /// `HeapCore::sweep_slots`), or when no thread of the run is left (the core's drop).
    pub(super) fn free_all(&self) {
        let blocks = std::mem::take(&mut *self.lock());
        for b in blocks {
            // SAFETY (gc_coord invariant: every other handle is stopped at a safepoint or
            // detached, `vm/gc_coord.rs`): a reader holds a block pointer only between two
            // safepoints, and no `RVec` points to a retired block.
            unsafe { free_block(b.ptr, b.layout) };
        }
    }
}

impl Drop for RetireList {
    fn drop(&mut self) {
        self.free_all();
    }
}

/// A growable vector of `T: Copy` whose grows retire the old block (see the module doc).
// M2.2: `Obj::List` moves onto `RVec`; until then only the tests use it.
#[cfg_attr(not(test), allow(dead_code))]
pub struct RVec<T: Copy> {
    ptr: AtomicPtr<Header>,
    _t: PhantomData<T>,
}

impl<T: Copy> Default for RVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl<T: Copy> RVec<T> {
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
    pub(super) fn block(&self) -> *mut Header {
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
        (h, hr.len.load(Ordering::Acquire), hr.cap)
    }

    pub fn len(&self) -> usize {
        self.snapshot().1
    }

    /// Element `i`. Panics when `i` is at or past `len`.
    pub fn get(&self, i: usize) -> T {
        let (h, len, _) = self.snapshot();
        assert!(i < len, "RVec index out of range: {i} >= {len}");
        // SAFETY: `i < len`, read with Acquire from the live block `h`.
        unsafe { load(h, i) }
    }

    /// Replace element `i`. Panics when `i` is at or past `len`.
    pub fn set(&self, i: usize, v: T) {
        let (h, len, _) = self.snapshot();
        assert!(i < len, "RVec index out of range: {i} >= {len}");
        // SAFETY: `i < len <= cap` in the live block `h`.
        unsafe { store(h, i, v) };
    }

    /// Replace the block `old` with one of at least `min_cap` slots that holds `old`'s
    /// elements. Only the thread whose swap wins retires `old`; a loser frees its own block.
    /// Either way the caller reloads the pointer.
    pub(super) fn grow(&self, old: *mut Header, min_cap: usize) {
        let (len, cap) = if old.is_null() {
            (0, 0)
        } else {
            // SAFETY: `old` is a live block (the caller loaded it before its next safepoint).
            let o = unsafe { &*old };
            (o.len.load(Ordering::Acquire), o.cap)
        };
        let new_cap = (cap * 2).max(min_cap).max(4);
        let new = alloc_block::<T>(new_cap);
        for i in 0..len {
            // SAFETY: `i < len` of the live block `old`, and `len <= cap < new_cap`.
            unsafe { store(new, i, load::<T>(old, i)) };
        }
        // SAFETY: `new` is this thread's own block; no other thread has seen it.
        unsafe { (*new).len.store(len, Ordering::Relaxed) };
        match self
            .ptr
            .compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                if !old.is_null() {
                    retire::<T>(old);
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
    fn reserve(&self, extra: usize) -> (*mut Header, usize) {
        loop {
            let (h, len, cap) = self.snapshot();
            let need = len.checked_add(extra).expect("an RVec capacity overflows");
            if need <= cap {
                return (h, len);
            }
            self.grow(h, need);
        }
    }

    pub fn push(&self, v: T) {
        let (h, len) = self.reserve(1);
        // SAFETY: `len < cap` in the live block `h`; the Release store publishes the slot.
        unsafe {
            store(h, len, v);
            (*h).len.store(len + 1, Ordering::Release);
        }
    }

    pub fn pop(&self) -> Option<T> {
        let (h, len, _) = self.snapshot();
        if len == 0 {
            return None;
        }
        // SAFETY: `len - 1 < len` in the live block `h`.
        unsafe {
            let v = load(h, len - 1);
            (*h).len.store(len - 1, Ordering::Release);
            Some(v)
        }
    }

    /// Insert `v` at `i`, shifting the tail right. Panics when `i > len`.
    pub fn insert(&self, i: usize, v: T) {
        let (h, len) = self.reserve(1);
        assert!(i <= len, "RVec insert index out of range: {i} > {len}");
        // SAFETY: every index is at most `len < cap` in the live block `h`, and the slots
        // below `len` were written.
        unsafe {
            for k in (i..len).rev() {
                store(h, k + 1, load::<T>(h, k));
            }
            store(h, i, v);
            (*h).len.store(len + 1, Ordering::Release);
        }
    }

    /// Remove and return element `i`, shifting the tail left. Panics when `i >= len`.
    pub fn remove(&self, i: usize) -> T {
        let (h, len, _) = self.snapshot();
        assert!(i < len, "RVec index out of range: {i} >= {len}");
        // SAFETY: every index is below `len` in the live block `h`.
        unsafe {
            let v = load(h, i);
            for k in i + 1..len {
                store(h, k - 1, load::<T>(h, k));
            }
            (*h).len.store(len - 1, Ordering::Release);
            v
        }
    }

    /// Lower `len` to `n`; frees nothing (`T: Copy` has no drop).
    pub fn truncate(&self, n: usize) {
        let (h, len, _) = self.snapshot();
        if n < len {
            // SAFETY: `n < len`, so `h` is a live block.
            unsafe { (*h).len.store(n, Ordering::Release) };
        }
    }

    pub fn clear(&self) {
        self.truncate(0);
    }

    pub fn extend_from_slice(&self, xs: &[T]) {
        if xs.is_empty() {
            return;
        }
        let (h, len) = self.reserve(xs.len());
        // SAFETY: `len + xs.len() <= cap` in the live block `h`; the Release store publishes
        // the slots.
        unsafe {
            for (k, &x) in xs.iter().enumerate() {
                store(h, len + k, x);
            }
            (*h).len.store(len + xs.len(), Ordering::Release);
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

impl<T: Copy> Drop for RVec<T> {
    /// The owning heap object is swept (world stopped) or dropped by its only owner: free the
    /// current block at once.
    fn drop(&mut self) {
        let h = *self.ptr.get_mut();
        if let Some(p) = NonNull::new(h.cast::<u8>()) {
            // SAFETY: `&mut self`: no reader of this vec is left, and `h` came from
            // `alloc_block`; its `cap` is never written after that.
            let layout = block_layout::<T>(unsafe { (*h).cap });
            // SAFETY: as above.
            unsafe { free_block(p, layout) };
        }
    }
}

/// See [`RVec::iter`].
pub struct Iter<'a, T: Copy> {
    h: *mut Header,
    len: usize,
    i: usize,
    _v: PhantomData<&'a RVec<T>>,
}

impl<T: Copy> Iterator for Iter<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.i >= self.len {
            return None;
        }
        // SAFETY: `i < len` of the snapshot block, live until the next safepoint.
        let v = unsafe { load(self.h, self.i) };
        self.i += 1;
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::{RVec, RetireList, local_retired, test_counts};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Barrier, Mutex, MutexGuard};

    /// The block counters are process-wide: the tests that read them run one at a time.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A push past the capacity retires the old block on this thread's list. Nothing frees it
    /// until a stop moves the list to the core and frees it there.
    #[test]
    fn grow_retires_old_block_until_stw() {
        let _g = serial();
        let list = RetireList::default();
        let v: RVec<u64> = RVec::with_capacity(4);
        for i in 0..5 {
            v.push(i);
        }
        let (_, freed) = test_counts();
        assert_eq!(local_retired(), 1, "the 4-slot block is retired");
        assert_eq!(
            (0..5).map(|i| v.get(i)).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        let bytes = list.adopt_local();
        assert!(bytes > 0, "the retired bytes are charged at the hand-off");
        assert_eq!(local_retired(), 0);
        assert_eq!(test_counts().1, freed, "the hand-off frees nothing");
        list.free_all();
        assert_eq!(
            test_counts().1,
            freed + 1,
            "the stop frees the retired block"
        );
    }

    /// One thread pushes 100k values while another reads `len` and `get`. Every read sees the
    /// value the writer stored at that index. The retired blocks are freed only after both join.
    #[test]
    fn racing_push_and_read() {
        const N: u64 = 100_000;
        let _g = serial();
        let list = RetireList::default();
        let v: RVec<u64> = RVec::new();
        let done = AtomicBool::new(false);
        let (_, freed) = test_counts();
        let reads = std::thread::scope(|s| {
            s.spawn(|| {
                for i in 0..N {
                    v.push(i);
                }
                list.adopt_local();
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
        list.free_all();
        assert!(
            test_counts().1 > freed + 10,
            "each growth retired one block"
        );
    }

    /// Two threads grow from one full block at once. Exactly one installs its block and retires
    /// the old one; the other frees its own block. Every block allocated is freed.
    #[test]
    fn loser_frees_its_own_block() {
        let _g = serial();
        let (alloc0, freed0) = test_counts();
        let list = RetireList::default();
        for _ in 0..200 {
            let v: RVec<u64> = RVec::with_capacity(2);
            v.push(1);
            v.push(2);
            let old = v.block() as usize;
            let go = Barrier::new(2);
            std::thread::scope(|s| {
                for _ in 0..2 {
                    s.spawn(|| {
                        go.wait();
                        v.grow(old as *mut super::Header, 3);
                        list.adopt_local();
                    });
                }
            });
            assert_eq!(list.len(), 1, "exactly one thread retires the old block");
            assert_eq!(v.iter().collect::<Vec<_>>(), [1, 2]);
            drop(v);
            list.free_all();
        }
        let (alloc, freed) = test_counts();
        assert_eq!(
            alloc - alloc0,
            3 * 200,
            "the first block and one new block per thread"
        );
        assert_eq!(freed - freed0, alloc - alloc0, "no block leaks");
    }

    /// A heap sweep (the stop) takes this thread's retired blocks and frees them.
    #[test]
    fn heap_sweep_frees_retired_blocks() {
        let _g = serial();
        let mut heap = crate::vm::heap::Heap::new();
        let v: RVec<u64> = RVec::with_capacity(1);
        v.push(1);
        v.push(2);
        let (_, freed) = test_counts();
        heap.sweep();
        assert_eq!(local_retired(), 0, "the sweep took the thread's list");
        assert_eq!(
            test_counts().1,
            freed + 1,
            "the sweep freed the retired block"
        );
    }

    /// The edit operations keep the order of a `Vec`.
    #[test]
    fn edits_match_vec() {
        let _g = serial();
        let list = RetireList::default();
        let v: RVec<u64> = RVec::new();
        let mut w: Vec<u64> = Vec::new();
        v.extend_from_slice(&[1, 2, 3]);
        w.extend_from_slice(&[1, 2, 3]);
        v.insert(0, 9);
        w.insert(0, 9);
        v.insert(4, 8);
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
        list.adopt_local();
        list.free_all();
    }

    #[test]
    #[should_panic(expected = "index out of range")]
    fn get_past_len_panics() {
        let _g = serial();
        let v: RVec<u64> = RVec::with_capacity(4);
        v.push(1);
        v.get(1);
    }
}
