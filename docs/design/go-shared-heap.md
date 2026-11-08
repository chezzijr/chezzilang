# Go shared-heap model

**Status:** done, merged to `main` 2026-10-31 (`be9684a4`). Tasks share one heap and get values by
handle, as Go goroutines do. The model of record is
[`../reference/concurrency/01-model.md`](../reference/concurrency/01-model.md). This file keeps the Go
facts the design rests on, the open work and the outcome. The pre-M1 study is in git.

**Citations:** `go:<path>:<line>` is a path in the Go 1.27.0 source (tag `go1.27.0`, commit
`8af21751`). Line numbers drift; grep the quoted symbol if a line does not match.

---

## 1. Go memory model (`go:doc/go_mem.html`)

### 1.1 Definitions

- **Data race:** "a write to a memory location happening concurrently with another read or write to
  that same location, unless all the accesses involved are atomic data accesses" (`go:doc/go_mem.html:54-56`).
- **DRF-SC:** a program without data races behaves "as if all the goroutines were multiplexed onto a
  single processor" (`go:doc/go_mem.html:59-62`).
- **Happens-before** is the transitive closure of sequenced-before and synchronized-before
  (`go:doc/go_mem.html:141-154`).

### 1.2 Behavior under a race

| Value size | Guarantee |
|---|---|
| One machine word or smaller | A read sees a value that some write stored. No out-of-thin-air values (`go:doc/go_mem.html:68-71`, `:244-253`). |
| Larger (string, slice, interface, map, struct) | No guarantee. The implementation may split it into word operations "in an unspecified order". A torn (pointer, length) or (pointer, type) pair "can in turn lead to arbitrary memory corruption" (`go:doc/go_mem.html:256-271`). The source names interface values, maps, slices and strings. |

An implementation may also report the race and stop the program (`go:doc/go_mem.html:68`). The `-race`
build does that.

### 1.3 Synchronized-before edges

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

## 2. Go's GC on one shared heap

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

## 3. Open work

| Phase | Work | Exit check |
|---|---|---|
| Concurrent mark (optional) | Hybrid write barrier in the VM store paths; mark workers; mark assists. | GC pause on a 1 GB heap below a target that the owner sets. Until then the stop limit is 100 ms on `benches/chz/many_list.chz` (`heap_stats_report_stw_pause`). |
| Race detection (optional) | A `--race` mode: a vector clock per slot, as in TSan. | Finds the races in a seeded test corpus. |

**Concurrent mark.** Add Go's hybrid write barrier (§2) on each `Value` store
into the heap. The compiler emits stores as opcodes, so the barrier lives in the VM's store paths, not in user code.
Two places need the barrier:

1. `Heap::write`, the sole write door (`src/vm/heap.rs`). It covers each object store, a generator
   suspend included.
2. Each store into a channel queue, a `Shared` box or an `Atomic` cell.

The core walk drops its guard between cores. A concurrent mark needs a new proof for that walk, or one
lock across it.

---

## 4. Outcome (M1 to M4)

M1 built one heap with a stop-the-world GC, M2 made containers memory-safe under a race, M3 made
tasks share values by handle. Concurrent mark (§3) and a `--race` mode are open.

| Study item | Decision or result | Differs from the study |
|---|---|---|
| containers | No lock per object (owner, Go parity). A buffer that grows, or that a sort or a remove replaces, retires; only the stop-the-world sweep frees it (`vm/rvec.rs`, M2.2 to M2.4). A racy op reads one view of the buffer and gives its own out-of-range fault. | Retire-then-sweep, with the stop of the world in place of QSBR or an epoch. |
| one-word slots | A `Value` slot stores with Release and loads with Acquire (M3.0b). | As planned. |
| stop-the-world GC | One heap per run, collected with the world stopped; a thread that blocks detaches first (M1.7, M1.8). The longest stop on `many_list` is 46 ms in release; the test bound is 100 ms until concurrent mark. | As planned. |
| deletions | Deleted in M3.1 to M3.5: module snapshots, the per-task copy machinery, the wire form, the box heaps, the checker's `sendable`. | As planned. The spec grids got Go goldens. |
| `Shared[T]` | `Shared[T]` stays: one value behind a guard; `update(f)` runs `f` in the caller's task on the live value (M3.3). `RwShared[T]` takes Go's `RWMutex` rules. | The first option; no `Mutex[T]`. |
| container safety gate | Miri and TSan cannot run on the development box. The gates are stress tests of 10,000 rounds, also under `CHEZZI_GC_STRESS=1` (`01-model.md`, "Data races"). | No sanitizer run. |
| perf gate | `storm` at T=2: 36 ms (3361 ms before M3); `spawn_fresh_graph` 0.41 to 0.52 of the time before M3. `send_one_channel` at T=2 is even with the time before M3. | One bench does not beat phase 0. |

Open rows at the end of M4: `map_str` at T=1 is 1.10 to 1.13 against `main` (owner accepted
2026-10-29; the branch base is 1.13 too); a flaky `owner_fault_grid` cell under full-suite load; the
`CHEZZI_GC_STRESS=1` timeouts in `tests/chz` that also occur on the branch base.
