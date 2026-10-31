# Go shared-heap model: study and mapping onto Chezzi

**Status:** done, branch `exp/go-shared-heap` merged to `main` 2026-10-31 (`be9684a4`). M1 (one
heap per run, a stop-the-world GC), M2 (memory-safe containers under a race), M3 (tasks share values
by handle; the airlock deleted) are done. M4 (2026-10-29) rewrote the docs, so the model of record is
[`../reference/concurrency/01-model.md`](../reference/concurrency/01-model.md). §1 to §6 are the study,
written at `772e0bb8` before M1: "today" in them means that commit. §7 records what the
implementation decided and where it differs from the study.

**Purpose:** describe the Go concurrency model as Go 1.27.0 implements it, then map each Go part onto
the Chezzi part that it would replace. Use this doc to plan the experiment "one heap for every task".

**Citations:**

- `go:<path>:<line>` is a path in the Go 1.27.0 source clone at `~/proj/go` (tag `go1.27.0`, commit `8af21751`).
- A bare `<path>:<line>` is a path in this repo at commit `772e0bb8`.
- Line numbers drift. Grep the quoted symbol if a line does not match.

---

## 1. The two models side by side

| Topic | Go (shared heap) | Chezzi at `772e0bb8` (shared nothing) |
|---|---|---|
| Heap | One heap for the process | One heap per fiber (`FiberCtx.heap`, `src/vm/sched_types.rs:106`) |
| Value | Raw pointer; multi-word values (string, slice, interface) | One 8-byte tagged word; objects are `GcRef(u32)` slab indices (`src/vm/value.rs:32-39`) |
| `go f(x)` / `spawn f(x)` | Copies the argument words; pointees are shared | Deep-copies the reachable graph two times (`src/vm/airlock_out.rs:85`, `src/vm/worker_vm.rs:212`) |
| Globals | One copy, shared | A snapshot per task, faulted in lazily (`src/vm/worker_vm.rs:1340`) |
| Write to a shared value | Allowed; a race is the user's bug | The checker rejects it (D4 layer A, `src/checker/writes.rs:78-92`). If the checker cannot see the write, it faults at runtime (`src/vm/stmt.rs:94-108`). |
| Channel send | Copies the element words; a direct handoff onto the receiver's stack | Serializes to `WireValue`, rebuilds in the receiver heap (`src/vm/channel.rs:51`, `:86-89`) |
| Mutex | `sync.Mutex` over any data | `Shared[T]`: a private box heap; closures cross into it (`src/vm/boxctx.rs:186-338`) |
| Atomics | `sync/atomic`, sequentially consistent | `AtomicInt` = `AtomicI64` SeqCst; `Atomic[T]` = `Mutex<WireValue>` (`src/vm/core.rs:912-952`) |
| GC | Concurrent tri-color mark-sweep, hybrid write barrier, two short STW pauses | Stop-the-world mark-sweep of one fiber heap; other fibers keep running (`src/vm/frames.rs:900-990`) |
| Allocation | Per-P `mcache`, no lock on the fast path | Per-heap slab plus free list, no lock because one owner (`src/vm/heap.rs:970`) |
| Scheduler | G/M/P, 256-slot local ring, `runnext`, steal half, global queue every 61 ticks | Same shape: `LocalQ{runnext, ring}`, `LOCAL_RING_CAP=256`, `GLOBAL_CHECK_INTERVAL=61` (`src/vm/sched_types.rs:668-710`) |
| Preemption | Prologue stack check plus signal-based async preemption, sysmon 10 ms | Reduction count, `CONTEXT_REDS = 4000` (`src/vm/mod.rs:1496`) |
| Race safety | Dynamic `-race` (ThreadSanitizer); a race on a multi-word value can corrupt memory | A race cannot be expressed |

The scheduler rows already match Go. The experiment changes the heap, value, crossing and GC rows.

---

## 2. Go memory model (`go:doc/go_mem.html`)

### 2.1 Definitions

- **Data race:** "a write to a memory location happening concurrently with another read or write to
  that same location, unless all the accesses involved are atomic data accesses" (`go:doc/go_mem.html:54-56`).
- **DRF-SC:** a program without data races behaves "as if all the goroutines were multiplexed onto a
  single processor" (`go:doc/go_mem.html:59-62`).
- **Happens-before** is the transitive closure of sequenced-before and synchronized-before
  (`go:doc/go_mem.html:141-154`).

### 2.2 Behavior under a race

| Value size | Guarantee |
|---|---|
| One machine word or smaller | A read sees a value that some write stored. No out-of-thin-air values (`go:doc/go_mem.html:68-71`, `:244-253`). |
| Larger (string, slice, interface, map, struct) | No guarantee. The implementation may split it into word operations "in an unspecified order". A torn (pointer, length) or (pointer, type) pair "can in turn lead to arbitrary memory corruption" (`go:doc/go_mem.html:256-271`). The source names interface values, maps, slices and strings. |

An implementation may also report the race and stop the program (`go:doc/go_mem.html:68`). The `-race`
build does that.

### 2.3 Synchronized-before edges

| Operation | Edge | Cite |
|---|---|---|
| `go` statement | before the goroutine starts | `go:doc/go_mem.html:308` |
| Goroutine exit | no edge | `:336` |
| Send | before the matching receive completes | `:373-374` |
| Close | before a receive that returns the zero value because of the close | `:405-406` |
| Unbuffered receive | before the matching send completes | `:416-417` |
| Buffered, capacity C | the kth receive is before the (k+C)th send completes (counting semaphore) | `:456` |
| `Mutex` | unlock n is before lock m returns, n < m | `:498-499` |
| `RWMutex` | the RLock/RUnlock rule | `:531-534` |
| `Once` | the one `f()` completes before any `Do(f)` returns | `:559-560` |
| Atomics | an observed atomic op is before its observer; all atomics have one SC order (as C++ `seq_cst`, Java `volatile`) | `:601-609` |

The compiler must not add a race to a race-free program (`go:doc/go_mem.html:801`, `:831`, `:855`).

---

## 3. Go runtime parts

### 3.1 Scheduler: G, M, P

- **G** is a goroutine. **M** is an OS thread. **P** is the right to run Go code. An M needs a P to run Go code
  (`go:src/runtime/proc.go:25-32`).
- Per-P state is the reason the common paths take no lock: the run queue, `mcache`, `gcWork` and the
  write-barrier buffer (`go:src/runtime/runtime2.go:782-884`).
- `newproc` puts the new G in the creator's `runnext` and wakes an idle P (`go:src/runtime/proc.go:5334-5347`).
- `findRunnable` order (`go:src/runtime/proc.go:3404`):
  1. GC stop.
  2. Timers.
  3. GC mark worker.
  4. The global queue on each 61st tick.
  5. The local queue.
  6. The global queue.
  7. `netpoll(0)`.
  8. Steal half of another P's queue.
  9. A blocking `netpoll`.
- **Syscalls:** the G keeps its P. Sysmon takes the P and calls `handoffp` when the syscall lasts longer than one
  sysmon tick and one of these is true (`go:src/runtime/proc.go:6743-6766`, `:3146`):
  - the P's local run queue has work;
  - no other P is idle or spinning;
  - the syscall lasts longer than 10 ms.
- **The GC and a G in a syscall:** `entersyscall` saves the G's SP for the GC (`go:src/runtime/proc.go:4677-4695`).
  The G cannot touch the heap again until `exitsyscall` gets a P, and a stop-the-world holds all Ps.
  So a stop-the-world counts a G in a syscall as stopped, and does not wait for it.
- **Preemption:**
  - Synchronous: sysmon sets `stackguard0 = stackPreempt`. The next function prologue enters the scheduler (`go:src/runtime/preempt.go:27-33`).
  - Asynchronous: sysmon sends `SIGURG`. The handler injects `asyncPreempt` at an async safe point (`go:src/runtime/signal_unix.go:342-349`).
  - Both fire after 10 ms on one G (`go:src/runtime/proc.go:6679`).

**Chezzi:** the M:N scheduler already copies this design (§1 table). Preemption by reduction count is
a safe point at each instruction boundary. A shared-heap GC can use that safe point as the place where
a worker stops for the GC. No signal-based preemption is necessary.

### 3.2 Goroutine stacks

- Start at 2 KiB. The starting size adapts to the mean scanned stack size (`go:src/runtime/stack.go:78-82`, `:1415-1458`).
- To grow, the runtime allocates a stack of two times the size and copies the old stack. It adjusts each pointer into the old stack
  with the frame pointer maps (`go:src/runtime/stack.go:929`, `:652-701`, `:1179`).
- Escape analysis guarantees this rule: "pointers to stack objects cannot be stored in the heap"
  (`go:src/cmd/compile/internal/escape/escape.go:21-47`). That rule is what lets the runtime move stacks.

**Chezzi:** a fiber stack is a `Vec<Value>` of handles, not of addresses. No heap object points into a
stack, so stack moves are free today and stay free with a shared heap.

### 3.3 Channels (`go:src/runtime/chan.go`, `select.go`)

- `hchan` has a ring buffer, `sendq` and `recvq` lists of `sudog`, and one `lock` for all fields
  (`go:src/runtime/chan.go:34-55`).
- **Send** (`chansend`, `go:src/runtime/chan.go:176`):
  1. A lock-free fast path for a non-blocking failure (`:213`).
  2. Lock. If closed, panic (`:226`).
  3. If a receiver is parked, copy the value onto the receiver's stack and make it ready (`:318-350`).
     The comment says sends and receives on unbuffered or empty-buffered channels are the only operations where one goroutine writes another goroutine's stack. A write barrier covers them (`:380-412`).
  4. Else, if the buffer has space, copy into the buffer (`:236-243`).
  5. Else, park (`:283`).
- **Close** wakes all receivers with the zero value and all senders, which then panic (`go:src/runtime/chan.go:414-476`).
- **`select`** polls the cases in a random order (`go:src/runtime/select.go:191-194`). It locks the channels in address order to prevent deadlock (`:206-240`, `:34-60`).

**Chezzi:** a channel element is one `Value` word. With a shared heap, `ChanState.queue` can hold
`Value` in place of `WireValue` (`src/vm/core.rs:437`). Send becomes "copy one word", the same as Go.
The GC must then treat the queue as a root.

### 3.4 Locks and atomics

- `sync.Mutex` is `{state int32; sema uint32}`. Lock uses a CAS fast path, then spins, then parks on a runtime semaphore
  (`go:src/internal/sync/mutex.go:20-23`, `:63`, `:104-149`).
- **Starvation mode:** a waiter that waits longer than 1 ms switches the mutex to direct FIFO handoff (`go:src/internal/sync/mutex.go:31-55`).
- The runtime semaphore is a hash table of 251 roots. Each root holds a treap of waiters keyed by address (`go:src/runtime/sema.go:28-58`).
- An atomic pointer store also runs the GC write barrier (`go:src/runtime/atomic_pointer.go:39-50`).
- `atomic.Value` uses a CAS on the type word, so a reader never sees a torn (type, data) pair (`go:src/sync/atomic/value.go:42-70`).

### 3.5 GC on one shared heap

**Cycle** (`go:src/runtime/mgc.go:5-110`):

1. STW sweep termination (`go:src/runtime/mgc.go:835`).
2. Set `_GCmark`. This turns on the write barrier (`:898`, `:259`).
3. Start the world. Workers and mark assists mark concurrently. New objects are black (`:43-44`).
4. STW mark termination (`:1066`).
5. `_GCoff`, then a concurrent sweep (`:1403`).

**Hybrid write barrier** (`go:src/runtime/mbarrier.go:24-35`):

```
writePointer(slot, ptr):
    shade(*slot)                 # Yuasa deletion half
    if current stack is grey:
        shade(ptr)               # Dijkstra insertion half
    *slot = ptr
```

- The deletion half stops a mutator from hiding an object when it moves the only pointer from the heap to its stack.
- The insertion half stops a mutator from moving a pointer from its stack into a black object (`go:src/runtime/mbarrier.go:46-53`).
- The barrier lets the GC scan each stack once, with no rescan (`:55-64`).
- The compiler emits `if writeBarrier.enabled { ... }` before each heap pointer store (`go:src/cmd/compile/internal/ssa/writebarrier.go:150-160`).
- Shades go into a 512-entry per-P buffer (`go:src/runtime/mwbbuf.go:70`).

**Stacks:** the GC suspends one G at a time, scans its stack, then resumes it (`go:src/runtime/mgcmark.go:221`, `:298`, `:904`).

**Mark assist:** a goroutine that allocates during mark pays debt in mark work (`go:src/runtime/mgcmark.go:494-660`).

**Allocator:**

- The per-P `mcache` needs no lock (`go:src/runtime/mcache.go:14-16`).
- On a refill, the `mcache` gets a span from `mcentral`, which uses lock-free span sets (`go:src/runtime/mcentral.go:22-45`).
- Only page allocation takes the global `mheap.lock` (`go:src/runtime/mheap.go:64-69`).

### 3.6 Race detector

- `-race` instruments each load and store with a ThreadSanitizer call (`go:src/cmd/compile/internal/ssagen/ssa.go:340-343`, `:1530-1550`).
- The runtime reports each sync edge to TSan, for example on channels and on the mutex (`go:src/runtime/chan.go:323-337`, `go:src/internal/sync/mutex.go:64-66`).
- The detector is dynamic. It finds only races on paths that the program runs.
- It does not instrument the runtime itself (`go:src/cmd/internal/objabi/pkgspecial.go:77-90`).

### 3.7 Payoff and cost

| Payoff | Cost |
|---|---|
| Sharing a value costs one word copy | A branch on each heap pointer store; a shade during mark |
| `go f(x)` and send are O(1) in the size of the value | CAS and acquire/release on run queues and span sets |
| One GC for all goroutines, short STW pauses | Mark assists add latency to goroutines that allocate |
| Plain `Mutex` over any data structure | The type system does not stop a race; a torn multi-word value corrupts memory |

---

## 4. Chezzi today, the parts the experiment touches

The full inventory is in the reference chapters. This list names only what a shared heap changes.

1. **Value and heap.** `Value` is one tagged word. An object is `GcRef(u32)`, an index into one heap's
   slab of `Slot { obj: Option<Obj> }` (`src/vm/value.rs:32-39`, `src/vm/heap.rs:598-600`, `:662-664`).
   The VM writes objects through `&mut Heap` (`src/vm/heap.rs:1004`). It uses no `Rc` and no `RefCell`.
2. **One heap per fiber.** `swap_ctx` swaps the heap with `out`, `stderr`, `executors`, `str_intern` and `fn_values`
   (`src/vm/exec.rs:253`, `:306-318`).
3. **GC.** Precise, non-moving, stop-the-world mark-sweep. It runs at an instruction boundary when `should_collect()` is true
   (`src/vm/frames.rs:247-252`, `:900-990`). It traces one heap. Other fibers keep running (`src/vm/core.rs:1508-1520`).
4. **Crossing.** Each value that crosses a task boundary goes through `to_wire` / `from_wire`
   (`src/vm/airlock_out.rs:193-226`, `src/vm/airlock_in.rs:19-37`). `Route` sets the D4 copy mark
   (`src/base/crossing.rs:199-231`). TICKET-295 sole-capture logic decides which closure captures skip the mark
   (`src/vm/airlock_out.rs:416-461`, `src/vm/boxctx.rs:56-93`).
5. **D4 faults.** A write to a copy-marked object faults. The checks are at:
   - `src/vm/step.rs:150-160`, `:491-494`
   - `src/vm/field_index.rs:494`, `:630`
   - `src/vm/method_call.rs:379`
6. **Box primitives.** `Shared[T]` and `RwShared[T]` own a private heap. A closure runs inside it (`src/vm/core.rs:821-905`).
7. **Checker.** These parts encode the model:
   - `sendable` (`src/checker/op_sendable.rs:149`)
   - D4 layer A (`src/checker/writes.rs:78-92`)
   - freshness (`src/checker/fresh.rs:90`, `src/checker/fn_writes.rs:1921-1962`)

### 4.1 The stated reasons for rejecting Go, checked against the code

| Reason (quoted) | Where | Check |
|---|---|---|
| "`Rc`→`Arc` across every value (atomic refcount bumps ~10–30× a normal bump, taxing single-threaded code *forever*)" | `docs/reference/concurrency/01-model.md:60-61` | **Stale.** The VM value path has no `Rc`: `grep -rn 'Rc<' src/vm src/base` finds only a comment at `src/vm/equality.rs:591`. Values are `u32` handles (`src/vm/value.rs:32-39`); a copy is a word copy. |
| "plus a thread-safe concurrent GC" | `01-model.md:61-62` | **Valid.** §5.2 below. A stop-the-world GC of all workers is the cheap first step; a concurrent GC is optional. |
| "hands users the entire data-race bug class" | `01-model.md:62-63` | **Valid.** §5.1 below. In Rust, a race on a `Vec` is undefined behavior, not only a wrong value. |
| "needs one shared thread-safe GC heap, a rewrite of the value model, GC and scheduler. Not before the JIT." | `docs/design/adr-d4-airlock.md:40-41` | **Partly stale.** The value model (one word, handle-based) needs no rewrite. The scheduler already matches Go. The heap and the GC need the rewrite. |

---

## 5. The hard problems a Rust VM has and Go does not

### 5.1 A race must not be undefined behavior

Go accepts memory corruption on a racy multi-word value (§2.2). A Rust VM cannot accept undefined
behavior. This example shows the problem in Chezzi terms:

```chezzi
xs := [1, 2, 3]
parallel:
    spawn: xs.push(4)       # Vec grows: realloc, old buffer freed
    spawn: print(xs[0])     # reads the old buffer -> use after free in Rust
```

Today the checker rejects this program (`772e0bb8` release binary):

```
type error (a.chz:3:12): 'xs' is this task's copy: a write to it would be lost at the join;
share it through Shared/Channel, or make a task-local copy with .copy()
```

With a shared heap and no new rule, the same program is undefined behavior in the Rust VM.

Each object kind needs one of these rules:

| Object kind | Rule options | What ancestors do |
|---|---|---|
| One-word slot (struct field, local cell, global) | Store the `Value` in an `AtomicU64` with a Release store and an Acquire load (M3.0b: a reader that loads a `GcRef` must see the object's contents, Go's `publicationBarrier`). On x86-64 both are plain moves; ARM64 uses `stlr`/`ldar`. A byte or an int word stays `Relaxed`. | Go: a word-sized racy read sees a value that some write stored (`go:doc/go_mem.html:244-253`). Java: the same for references. |
| Growable container (`list`, `dict`, `set`, `str` builder) | (a) A lock per object. (b) Copy-on-grow with deferred free of the old buffer (QSBR/epoch). (c) A lock for writers plus lock-free readers with deferred free. | CPython 3.13 free-threaded build (PEP 703): a lock per object for `list`/`dict`, lock-free reads with deferred free. Go: no protection (memory unsafe). Java: `ArrayList` is racy but memory safe, because the array object has a fixed length and the JVM checks bounds. |
| Immutable kind (`Str`, `Bytes`, `BigInt`, `Tuple`, `Enum`, the `fixed_after_build` list in `src/vm/boxctx.rs:56-93`) | Share with no lock after construction. Publish with `Release` and read with `Acquire`. | All three: immutable values are free to share. |

**Decision for the owner.** Pick (a), (b) or (c) for containers. Option (a) is the simplest.
It adds an uncontended lock on each container operation, so it needs a benchmark (§6, phase 0).

### 5.2 The heap must be safe for many threads

The slab is `Vec<Slot>`. A `Vec` that grows moves its buffer, so a concurrent reader can read freed memory.

| Part | Go reference | Proposed Chezzi change |
|---|---|---|
| Slab storage | Spans at fixed addresses (`go:src/runtime/mheap.go`) | Segmented slab: a fixed table of chunk pointers, chunks never move. A `GcRef` stays a `u32` (chunk index, slot index). |
| Allocation | Per-P `mcache` (`go:src/runtime/mcache.go:14-16`) | A free-list chunk per worker. A refill takes a global lock, as `mheap.lock` does. |
| Mark bits, `copied` bits, view stamps (`src/vm/heap.rs:668-682`) | Per-span mark bits, set with atomic OR | `AtomicU64` words. The `copied` bits go away with D4. The view stamps (TICKET-213 snapshot freshness, `src/vm/heap.rs:676-684`) go away with the module snapshots (§5.4). |
| Byte accounting (`since_gc_bytes`, `src/vm/heap.rs:705`) | Per-P stats, flushed | Per-worker counter, summed at the GC trigger. |
| Per-heap caches (`str_intern`, `fn_values`, `module_objs`, `src/vm/mod.rs:969-977`) | Global, with locks | One process-wide table each. `str_intern` needs a concurrent map or a lock. |

### 5.3 The GC must see every fiber

Today a GC traces one heap from one `Vm`'s roots (`src/vm/frames.rs:914-990`). A shared heap needs these roots:

1. The stack and frames of each fiber, including parked fibers. Go keeps the list `allgs` for this purpose.
2. Each channel queue, `Shared` box and `Atomic` cell, once they hold `Value` in place of `WireValue`.
3. The globals, one copy.

**Stage 1 (stop the world).** Stop all workers, mark all roots, sweep, then restart. This has the shape of
Go's sweep-termination pause, extended to the whole cycle. It needs no write barrier. The pause grows with the
live heap. Until stage 5 the stop limit is 100 ms on `benches/chz/many_list.chz` (128 MB live), on a release
build: the release bench run with `CHEZZI_HEAP_STATS=1` owns it, and `heap_stats_report_stw_pause`
checks it only in a release test build (a debug build counts and times the stops); concurrent mark brings it back to 10 ms. A running worker stops at
its next reduction check. A reduction check is not enough for a thread
that blocks in place. These waits block the OS thread and execute no instruction (`src/vm/block.rs:140-173`):

- `Stdin`, `Guard` and `Join` demote from a fiber;
- all waits demote from a fiber that is already demoted;
- `Connect` waits in place.

Example: worker A holds a `Shared` guard and starts a GC. Worker B blocks on that guard. If the GC waits for B,
A and B deadlock. Use Go's syscall rule (§3.1): before a thread blocks in place, it publishes "stopped" with its
roots on the VM stack. After the wait, it checks for a GC in progress and waits for the GC to end before it
touches the heap. The GC counts a "stopped" thread as stopped and does not wait for it.

**Stage 2 (concurrent mark, optional).** Add Go's hybrid write barrier (§3.5) on each `Value` store
into the heap. The compiler emits stores as opcodes, so the barrier lives in the VM's store paths, not in user code.
The D4 sites (§4 item 5) are not enough. A generator suspend writes a whole stack of `Value`s into
`Obj::Generator` (`src/vm/exec.rs:700-706`) with no D4 site. Two places need the barrier:

1. `Heap::write`, which is the sole write door (`Heap::init_mut` calls it, `src/vm/heap.rs`). This one place covers each
   object store, the generator suspend included.
2. Each store into a channel queue, a `Shared` box or an `Atomic` cell.

The core walk drops its guard between cores (`src/vm/core.rs:1503-1515`). That walk is safe today only because
each heap belongs to one worker. A concurrent mark breaks that rule, so the core walk needs a new proof or one
lock across the walk. Stage 2 can wait until stage 1 has numbers.

### 5.4 What a shared heap deletes

These parts exist only because the heaps are separate. A shared heap deletes them:

| Part | Sites |
|---|---|
| Deep copy at spawn | `src/vm/airlock_out.rs:85-157`, `src/vm/nursery.rs:120`, `:284`, `src/vm/worker_vm.rs:161-353` |
| Module-global snapshots | `src/vm/nursery.rs:320`, `src/vm/worker_vm.rs:738-1449`, roots at `src/vm/frames.rs:962-984` |
| D4 airlock and copy marks | `src/vm/stmt.rs:94-108`, the five fault sites, `src/base/crossing.rs:199-231`, `src/vm/airlock_in.rs:114`, `:203-213` |
| Sole-capture logic (TICKET-295) | `src/vm/airlock_out.rs:400-461`, `src/vm/airlock_in.rs:675-683` |
| `WireValue` in channel, `Atomic` and box cores | `src/vm/core.rs:437`, `:913`, `src/vm/wire.rs:279`, `src/vm/core.rs:1492-1520` |
| Private box heaps for `Shared` / `RwShared` | `src/vm/core.rs:821-905`, `src/vm/boxctx.rs:186-338` |
| Checker sendability and D4 layer A | `src/checker/op_sendable.rs:149-160`, `src/checker/writes.rs:78-136`, `src/checker/fresh.rs:90` |

The `tests/chz/spec/airlock_*` grids (since M4b `tests/chz/spec/crossing_*`) pinned the copy semantics. A shared heap breaks them by design.
Each grid needs a new golden from the Go program of the same shape, not a deletion.

### 5.5 What the user sees

| Program | Today | Go model |
|---|---|---|
| `spawn fill(xs)`, where `fill` pushes onto `xs` | Checker error: `'xs' is this task's copy: a write to it would be lost at the join` | The mutation is visible to the parent after the join |
| Two tasks push onto one list, no lock | Cannot be expressed | A data race. Option (a) in §5.1 makes it safe but the order is not determined. |
| Global counter `n += 1` in N tasks | Checker error: `'n' is this task's copy` | A lost update, as in Go. The user needs `Mutex` or `AtomicInt`. |
| Send a 1 MB list on a channel | Copies 1 MB | Copies one word; both tasks then share the list |

Go ancestor output (go1.27.0) for 1,000 goroutines that do `n++`, plus one goroutine that does
`xs = append(xs, 4)`, joined with a `WaitGroup`:

```
$ go run main.go          # run 1
940 [1 2 3 4]
$ go run main.go          # run 2
911 [1 2 3 4]
$ go run -race main.go
WARNING: DATA RACE
WARNING: DATA RACE
750 [1 2 3 4]
Found 2 data race(s)
exit status 66
```

Go loses updates on each run, and only `-race` reports the races.

Decision for the owner: if `Shared[T]` stays, it becomes a plain `Mutex` around a `Value`.
`update(fn)` then runs the closure in the caller's fiber under the lock. The other option is to replace
`Shared[T]` with a `Mutex[T]` that has Go's surface.

---

## 6. Experiment plan

Each phase ends with numbers or a pass/fail result. Stop at a phase whose numbers say no.

| Phase | Work | Exit check |
|---|---|---|
| 0. Baseline | Run `docs/contributing/benchmarks.md` single-thread and spawn/channel benches on `main`. Add a bench that sends a large list on a channel and one that spawns 10k tasks with large captures. | Numbers recorded, n ≥ 5 runs per side. |
| 1. Atomic slots | Store struct fields, cells and list elements as `AtomicU64` (a `Value` is Release/Acquire since M3.0b). No other change. This covers a one-word slot only. It does not make a `Vec` that grows safe (§5.1). | Single-thread benches within noise of phase 0. |
| 2. Shared heap, STW GC | Segmented slab, per-worker alloc chunks, one root set over all fibers, and a GC that stops each worker (§5.3 stage 1, with the "stopped" state for a thread that blocks in place). Keep the crossing deep copy, so no object is reachable from two fibers yet. | `cargo test` passes. GC pause recorded. |
| 3. Container safety | Pick option (a), (b) or (c) from §5.1 and implement it for `list`, `dict`, `set`. | A stress test with N writers and M readers on one list runs clean under Miri or TSan. |
| 4. Delete the copy machinery | Remove the parts in §5.4. Channels carry `Value`. Globals are shared. | Large-send and spawn benches beat phase 0. New goldens for the spec grids. |
| 5. Concurrent mark (optional) | Hybrid write barrier in the VM store paths; mark workers; mark assists. | GC pause on a 1 GB heap below a target that the owner sets. |
| 6. Race detection (optional) | A `--race` mode: a vector clock per slot, as in TSan. | Finds the races in a seeded test corpus. |

Build this worktree with its own target directory, for example
`CARGO_TARGET_DIR=$PWD/target cargo build --release`. If you use main's target directory, cargo links a binary from a mix of both trees.

---

## 7. Outcome (M1 to M4)

The phases ran as M1 (phase 2), M2 (phase 3) and M3 (phases 1 and 4). Phases 5 and 6 are open. The
progress lines and the gate numbers are in `PROGRESS.md`, "Go shared heap".

| Study item | Decision or result | Differs from the study |
|---|---|---|
| §5.1 containers | No lock per object (owner, Go parity). A buffer that grows, or that a sort or a remove replaces, retires; only the stop-the-world sweep frees it (`vm/rvec.rs`, M2.2 to M2.4). A racy op reads one view of the buffer and gives its own out-of-range fault. | Close to option (b), with the stop of the world in place of QSBR or an epoch. |
| §5.1 one-word slots | A `Value` slot stores with Release and loads with Acquire (M3.0b). | As planned. |
| §5.3 stage 1 | One heap per run, collected with the world stopped; a thread that blocks detaches first (M1.7, M1.8). The longest stop on `many_list` is 46 ms in release; the test bound is 100 ms until stage 2. | As planned. |
| §5.4 deletions | Deleted in M3.1 to M3.5: module snapshots, the airlock walkers, the wire form, the copy bits, the D4 layer A and layer C sites, the box heaps, the checker's `sendable`. | As planned. The spec grids got Go goldens; a test that asserted only deleted machinery was deleted. |
| §5.5 `Shared[T]` | `Shared[T]` stays: one value behind a guard; `update(f)` runs `f` in the caller's task on the live value (M3.3). `RwShared[T]` takes Go's `RWMutex` rules. | The first option; no `Mutex[T]`. |
| §6 phase 3 exit | Miri and TSan cannot run on the development box. The gates are stress tests of 10,000 rounds, also under `CHEZZI_GC_STRESS=1` (`01-model.md`, "Data races"). | No sanitizer run. |
| §6 phase 4 exit | `storm` at T=2: 36 ms (3361 ms before M3); `spawn_fresh_graph` 0.41 to 0.52 of the time before M3. `send_one_channel` at T=2 is even with the time before M3. | One bench does not beat phase 0. |

Open rows at the end of M4: `map_str` at T=1 is 1.10 to 1.13 against `main` (owner accepted
2026-10-29; the branch base is 1.13 too); a flaky `owner_fault_grid` cell under full-suite load; the
`CHEZZI_GC_STRESS=1` timeouts in `tests/chz` that also occur on the branch base.
