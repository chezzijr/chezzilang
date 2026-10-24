## 6. `Shared[T]` — the cross-task mutable box

> **Members of `std.concurrency`.** `Shared`, `RwShared`, `Atomic`, `AtomicInt` and `Executor` are
> ordinary members of `std.concurrency`: `import Shared from std.concurrency` (an alias works) binds the
> bare name, and `import std.concurrency` gives `concurrency.Shared(0)`. A whole-module import binds no
> bare name. `Channel` is global; `timer(ms)` is an ordinary `std.time` function
> ([04-timer-wait.md](04-timer-wait.md)). The constructors lower to dedicated opcodes, so the import
> costs nothing at run time.

Captured locals **cross into a task as copies** ([§7](07-sendability.md#7-sendability)) — same-task
capture is by reference, but the task boundary deep-copies — so a task can't mutate the parent's
state. For shared mutable state *across* tasks, use **`Shared[T]`** — a box whose writes are
serialised by a single owner (Elixir's `Agent` trick), so the torn-write race is unrepresentable.

```chezzi
import Shared from std.concurrency

s := Shared(0)             # one owner holds the value
parallel:
    spawn bump(s)          # the HANDLE is copied in — both reach the same owner
    spawn bump(s)
print(s.get())             # 2

fn bump(s: Shared[int]):
    s.update(fn(x): x + 1)
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `get`    | `get(self) -> T` | **snapshot copy out** |
| `set`    | `set(self, v: T) -> None` | overwrite |
| `update` | `update(self, f: fn(T) -> T) -> None` | read-modify-write, serialised by the owner; `f` runs in the box heap on the live value |
| `read`   | `read(self, f: fn(T) -> R) -> R` | run `f` in the box heap on the live value; only `f`'s result is copied out |

> **Gotcha — `get()` returns a copy, not the box.** The value lives in the **box's own private
> heap**, not in any task's heap, so `get` deep-copies it *out* into a fresh value each call.
> Mutating that value — `s.get().push(x)`, `s.get().value = 1` — changes a throwaway, **not** the box:
> the write is silently lost. Mutate only via `update` (or `set` a whole new value). Same for
> `RwShared` (`read`/`write`) and `Atomic` (`load`/`store`). A plain in-task `struct`/collection is
> different: its reads alias the live value (push-through works), but it can't cross the airlock.
>
> The checker **warns** on the bare-statement shape: a mutating builtin call (`s.get().push(1)`,
> `.insert`, `.remove`, `.pop`, …) or a field/index assign (`b.get().v = 9`, `s.get()[0] = 9`) on a
> `Shared`/`RwShared` `get()` or `Atomic` `load()` read temporary. The message names the working
> spelling — `s.update(…)`, `r.write(…)` or `a.store(…)` — and the exit code is unchanged. The
> checker cannot tell the write is lost, so it stays silent on four shapes that DO lose it (each
> prints `0`):
>
> | shape | example |
> |---|---|
> | a bound temporary | `v := s.get()` then `v.push(1)` |
> | a user struct method | `b.get().bump()` |
> | a `RwShared.read` closure | `RwShared.read(fn(xs): xs.push(1))` |
> | a deferred call | `defer s.get().push(1)` |
>
> A `get()` whose value is used (`print(s.get().len())`) never warns. Only a bare-name box receiver
> matches: `boxes[0].get().push(1)` stays silent. The box read is one row of a wider rule: a mutating
> call or a store on ANY fresh value that no name holds (`g[0..2].push(1)`, `xs.copy().push(1)`,
> `(xs + ys).sort()`) changes a dropped copy; [`syntax.md`](../syntax/01-lexical-and-literals.md) (slices) lists the rows. A
> field or index store through a box read (`s.get().xs.push(1)`) warns for the same reason.

**The ladder:** bare value (copied) → a mutable in-task `struct`/collection → `Shared[T]` (cross-task
box). An in-task mutable value (a one-field `struct`, a `List`, …) is a shared reference *within one
task* but is **not** sendable (copy-on-spawn gives each task its own copy); `Shared[T]` **is** sendable
(the handle is copied, the value isn't).

### Where the value lives

A `Shared[T]` owns its value: the value lives in the run's heap, and only the box reaches it.
`update(f)` and `read(f)` run `f` inside the box, on the live value:

```chezzi
s := Shared(Table(keys, 0))            # keys: 100000 items
s.update(fn(t): hit(t))                # hit: t.hits += 1; return t   -- in place, no copy
n := s.read(fn(t): t.hits)             # only the int is copied out
```

Measured (release, one caller, struct with a 100000-item list):

| operation | cost |
|---|---|
| `update` (in place) | 3 to 8 us per call (2000 calls inside 1000 ms, `tests/chz/stdlib/box_in_place_perf_test.chz`) |
| `RwShared.write`, 8 callers | up to 25 us |
| `get()` (whole-value copy) | 4086-4912 us |
| `Shared.read(f)` of a part | about 2 us at any size |
| `update` on an `int` with `fn(x): x + 1` | 1.32-1.54 us |
| creating a box | about 4 us |

Read a part of a big value with `Shared.read(f)`. Every cell:
[`benchmarks.md`](../../contributing/benchmarks.md).

**The closure runs like the body of a spawned task.** Its captures are copies, so a write to one is
the D4 error or fault, never a silent lost write. It sees module globals as the caller has them **at
the call**. Reading a scalar, a `str`, a `fn` or a box handle from a global costs nothing extra.
Reading a mutable aggregate global (`List`, `Map`, a struct) takes the caller's module snapshot, and
when the caller also mutates such a global between calls the snapshot is rebuilt per call: 13493 ms
for 2000 calls on a 100000-item global, about 6.7 ms per call, the price `spawn` pays (15 ms per call
when the read sits two boxes deep). Capture what the closure needs instead (`n := G.len()` outside,
`fn(x): x + n` inside): 8 ms for the same 2000 calls.

A box closure never reads a stale global. Measured at `CHEZZI_THREADS` 1, 2 and default, with the
closure reading `G.len()` of a mutable `List` global while the caller pushes to `G` between calls or
does not: every position reads the current value — a literal closure, a named `fn`, a method called
from the closure, a read beside a qualified module call, a closure nested in another box's closure, a
`RwShared.write` closure and a `Shared.read` closure. A "stale global" checker warning would be false
wherever it fired, so it is declined.

**Re-entry and faults.**

- `get` / `read` / `set` / `update` / `print` of a `Shared` from inside that box's own `update` or
  `read` closure fault with `already holds the update guard`; the closure holds the value, so no
  earlier copy exists.
- A fault inside the closure is raised in the caller, `recover:` catches it, the box keeps serving
  and the state is what the closure left.
- A slow closure blocks every user of the box and holds its worker (the Elixir `Agent` warning
  applies: keep it short). A child of a `parallel:` inside the closure that reads or writes the
  **same** box can never be served (the closure holds the box until the nursery joins), so the run
  stops with the `update guard wait: deadlock` fault at the child's own `get`; it does not hang. Pass
  the child the value it needs instead of the box.
- A task's open nurseries stay its own inside a box closure: when a child of an enclosing `parallel:`
  faults, an owner that waits inside the closure is cut there, and a `recover:` inside the closure
  does not catch the child's fault. A `parallel:` opened inside the closure joins the scheduler of the
  task's enclosing `parallel:`, so a nursery deadlock there is reported the same way
  (`deadlock: every task in this parallel: block is blocked …`, exit 1).
- One shape declines the verdict and hangs: under an enclosing `parallel:` whose sibling is blocked,
  a closure that itself waits on a channel nobody can feed.
- A reader of a `RwShared` whose writer is blocked for good gets the `update guard wait: deadlock`
  fault at its own `read` / `get`, as a `Shared` user does.

`Shared.read(f)` runs `f` on the live value, so a mutation `f` makes persists. `RwShared` (§6f) is the
same box core behind a read-write lock.

**When to pick which box.**

| | `Shared[T]` | `RwShared[T]` |
|---|---|---|
| lock | one user at a time | many readers or one writer |
| `update` / `write` | closure runs in the box heap, in place | the same, exclusive |
| `read(f)` | `f` runs in the box heap; only its result is copied; readers queue | the reader gets a COPY and `f` runs in the reader's heap; readers run in parallel |
| `get()` | whole-value copy out | whole-value copy out |
| pick it when | you mostly write, or read a small part of a big value (`read`) | reads dominate and readers must not queue |

**Display rule.** `print`, `str` and interpolation of a `Shared` / `RwShared` / `Atomic` render what
the box holds structurally, with no user `str` hook: `str(Shared(P(1)))` is `Shared(P(x=1))` even when
`P` defines `str`. A box that holds itself, directly or through a container or a struct, prints the
cycle marker like any other container cycle:

```
b: Shared[N?] = Shared(None)
n := N(1, b)
b.set(?n)
print(n)        # N(id=1, box=Shared(N(id=1, box=Shared(...))))
```

The renderer takes a snapshot of the box, drops the lock, then renders. A fault message that shows a
box never waits for it: when the state is busy the message shows `Shared(<locked>)`. User output from
`print` / `str` never shows `<locked>`.

---

## 6b. `Atomic[T]` — the cross-task atomic box

`Atomic[T]` is `Shared[T]`'s sibling for **atomic operation primitives** (compare-and-swap, exchange,
fetch-add) rather than `Shared`'s closure-based `update`. Same shape: one box, many tasks; the
**handle is sendable**, the value is copied in/out under a lock; constructed value-first (`Atomic(v)`,
`T` inferred — an optional `Atomic[T](v)` turbofish pins it and is checked against the value). Generic
over any `T`; the arithmetic methods are restricted to numeric `T`.

```chezzi
a := Atomic(0)
parallel:
    for _ in 0..100:
        spawn bump(a)
print(a.load())            # 100 — every add is atomic, no lost update

fn bump(a: Atomic[int]):
    a.add(1)
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `load`     | `load(self) -> T` | read out |
| `store`    | `store(self, v: T) -> None` | overwrite |
| `exchange` | `exchange(self, v: T) -> T` | swap; returns the **old** value |
| `cas`      | `cas(self, expected: T, new: T) -> bool` | compare-and-swap; swaps iff the box equals `expected`; returns whether it did |
| `add`      | `add(self, x: T) -> T` | numeric `T` only; returns the **new** value |
| `sub`      | `sub(self, x: T) -> T` | numeric `T` only; returns the **new** value |

`add`/`sub` use the language's checked integer arithmetic (an overflow faults, exactly like `+`/`-`)
and plain float arithmetic. Each method is a single lock-op-unlock, so the read-modify-write is atomic
across threads with no separate update lock. Reach for `Atomic` when a counter/flag/CAS-loop is
clearer than `update(closure)`; reach for `Shared` when the update is an arbitrary transformation.

**`cas` compares *structurally* — it never calls a user `eq`.** The compare happens under the value's
lock, and re-entering user code there could deadlock. So that `a == b` and `atom.cas(a, …)` never
disagree, the checker rejects a payload type that **reaches** a user `eq`:

- its own: `Atomic[K] payload defines its own 'eq' …`;
- one on any element, entry, tuple slot, struct field or enum payload the structural compare would
  recurse into: `Atomic[List[K]] payload reaches 'K', which defines its own 'eq' …`.

Use `Shared[K]`, which has no `cas`. "Defines its own `eq`" means the type has the `Eq` hook: a method
named `eq` that is static, misnames its parameter or returns another type is an ordinary method, `==`
on that type stays structural, and the type is a legal payload.

**A type parameter as the payload must carry the sealed bound `StructuralEq`.**
`fn mk[T](v: T) -> Atomic[T]` is rejected
(`Atomic[T] payload 'T' must carry the bound StructuralEq (import it from std.concurrency)`), and so is
`Atomic(v)` on a `v: T` inside a generic body. Write `fn mk[T: StructuralEq](v: T) -> Atomic[T]`;
each call then checks its own `T`, and `mk(K(1))` for a `K` with the `eq` hook reports
`type K does not satisfy StructuralEq (it has the 'eq' hook)`. `StructuralEq` is declared in
`std/concurrency.chz` and imported like any member: `import Atomic, StructuralEq from std.concurrency`,
or `concurrency.StructuralEq` after `import std.concurrency`. Without it the bound reports
``unknown name 'StructuralEq' in bound on 'T'; import it from std.concurrency (`import StructuralEq from std.concurrency`)``.
A module's own `protocol StructuralEq` is its own protocol. No type can implement the bound: the
checker answers it from the payload's shape. No checker walk can see through a protocol existential
payload, so the VM also switches the `eq` hook off for the compare.

**`cas` on a payload that holds a `fn` value is rejected** (`Atomic[fn() -> int]`, a struct field or
`List` element of `fn` type). Every `load()` returns a fresh copy of the closure and closures compare
by identity, so `cas(a.load(), new)` could never succeed and the standard CAS retry loop would spin
forever (Go's `atomic.Value.CompareAndSwap` panics `comparing uncomparable type` on the same shape).

- The error is at the `cas` call, so `load`/`store`/`exchange` of a fn stay legal.
- A named fn, a builtin fn (`ord`), a std native fn (`math.sqrt`) and an `extern` fn are fn values
  like a closure, so `cas` rejects them too.
- A generic `fn f[T](a: Atomic[T], …)` hides the fn from the checker, so the runtime `cas` faults
  `Atomic.cas: the payload holds a function value, which cas cannot compare` instead of answering
  `false` forever. A builtin fn (`ord`) faults the same way. An iterator payload faults the same way
  (`Atomic.cas: the payload holds an iterator, which cas cannot compare`).
- A handle payload (`Atomic(ch)`) compares by its core, so `cas(a.load(), …)` on it succeeds.

### `AtomicInt` — the monomorphic **lock-free** int atomic

`AtomicInt` is `Atomic[T]`'s monomorphic-int sibling. It is statically `int`, so it is backed by a
lock-free `std::sync::atomic::AtomicI64` — no `Mutex`, no runtime type-sniffing — like Rust's
`AtomicI64`, Java's `AtomicInteger`, or Go's `atomic.Int64`. A `std.concurrency` member like
`Atomic`; constructed `AtomicInt(v)` with one int arg. The method surface is `Atomic`'s, all
int-typed, and `add`/`sub` are **always** available:

```chezzi
import AtomicInt from std.concurrency
a := AtomicInt(0)
fn main():
    parallel:
        for _ in 0..8:
            spawn:
                for _ in 0..100000:
                    a.add(1)
    print(a.load())            # 800000 — lock-free, no lost update
main()
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `load`     | `load(self) -> int` | read out |
| `store`    | `store(self, v: int) -> None` | overwrite |
| `exchange` | `exchange(self, v: int) -> int` | swap; returns the **old** value |
| `cas`      | `cas(self, expected: int, new: int) -> bool` | compare-and-swap; swaps iff the box equals `expected` |
| `add`      | `add(self, x: int) -> int` | returns the **new** value; overflow **faults** |
| `sub`      | `sub(self, x: int) -> int` | returns the **new** value; overflow **faults** |

Every op uses `SeqCst` ordering (the same sequential consistency `Atomic`'s Mutex gives).
`add`/`sub` keep the i64-overflow fault via a checked `compare_exchange` CAS-loop (not a
silently-wrapping `fetch_add`). Reach for `AtomicInt` over `Atomic(0)` for a hot int counter/flag
under contention — it measured **~2.7× faster** than the Mutex-backed `Atomic` on an 8-way counter
(see [`benchmarks.md`](../../contributing/benchmarks.md)); uncontended it is a wash.

---

## 6f. `RwShared[T]` — the cross-task read-write box

`RwShared[T]` is `Shared[T]`'s read-write counterpart: **MANY concurrent readers OR one exclusive
writer**. Same shape as `Shared` — one box, many tasks; the **handle is sendable**; constructed
value-first (`RwShared(v)`, `T` inferred — an optional `RwShared[T](v)` turbofish pins it and is
checked against the value) — but the lock is a `RwLock` instead of a `Mutex`. **Reach for `RwShared`
over `Shared` when reads dominate** (a shared config/registry/map read on every request): read guards
don't exclude each other. Reach for `Shared` when access is write-heavy (one lock is simpler).

```chezzi
fn put(r: RwShared[Map[str, int]], k: str, v: int):
    r.write(fn(m): m.merge({k: v}))      # exclusive write lock

fn total(r: RwShared[Map[str, int]]) -> int:
    return r.read(fn(m): m.values().sum())  # shared read lock (concurrent with other readers)
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `get`   | `get(self) -> T` | shared read guard; **snapshot copy out** (== `read(identity)`) — mutating it is a no-op (see the `Shared` `get()` gotcha above); mutate via `write` |
| `set`   | `set(self, v: T) -> None` | exclusive write guard; overwrite |
| `read`  | `read(self, f: fn(T) -> R) -> R` | **shared** read guard: run `f` against the current value, return `f`'s result; **no** write-back. Many `read`s run concurrently |
| `write` | `write(self, f: fn(T) -> T) -> None` | **exclusive** write guard: `Shared.update` under the write lock — read-modify-write, store `f`'s return |

`write`'s read-modify-write is atomic across threads, exactly like `Shared.update`: the whole
`write`/`set` is serialised by the box's **update guard** (a process-wide wait-for graph, not a bare
lock), so concurrent writers can't lose each other's updates — a `set` racing an in-flight `write`
**blocks** behind the guard and lands; it is never silently overwritten.

**Where the value lives.** As with `Shared`, the box owns its value. `write(f)`
runs `f` **inside** the box on the live value, under the exclusive lock: no copy out and back, the
closure rule of §6 applies (captures are copies, globals are the caller's as of the call), and a fault
inside `f` leaves the state as `f` left it. A reader (`get`, `read`, a collection read, `print`)
**copies** what it returns out of the live heap under the shared lock, drops the lock, and only then
decodes it; `read`'s closure runs on that copy in the **reader's** heap. So no user code runs under
the read lock, readers run in parallel, and a mutation inside a `read` closure does not reach the
box. A reader waits only while a writer holds or waits for the box.

> **Reentrancy fault (same class as `Shared.update`):** a closure passed to `write` (or `set`) that
> re-acquires the **same** `RwShared`'s update guard — `write` or `set` inside `write` — is a
> length-1 wait-for cycle and **faults** (`"already holds the update guard"`), instead of hanging or
> silently losing the inner write. A cross-box wait-for cycle (two boxes each written from inside the
> other's closure) faults the same way; two tasks that merely contend for different boxes are not a
> cycle and complete normally. `get` / `read` / `print` of the box from inside its **own `write`**
> closure fault the same way. `write` (or `get` / `read`) nested in **`read`** is legal: the reader
> closure runs on its own copy after the lock dropped.

> **A guard wait does not cost an OS thread unless it is long.** A contended `set`/`update`/`write`
> waits in place on its worker for up to 5 ms (`GUARD_DEMOTE_BUDGET`) before it demotes the worker
> and spawns one replacement OS thread, so almost no wait pays for a thread. Details:
> [`concurrency-runtime.md`](../../design/concurrency-runtime.md#guard-waits).

### Fan out big shared data — reduce in place, don't `get`/`read` it

`get`/`read` copy the **whole** value out of the lock into the caller's heap. Fanning a 1M-element
list out to 8 workers, each calling `get()`/`read()`, materializes the entire list eight times.
`RwShared[T]` itself has exactly four methods for every `T` — `get`, `set`, `read`, `write` — so for a
shared **container** use a collection from `std.concurrency.collection`:

| collection | methods |
|---|---|
| `ConcurrentList[E]` | `len`/`at`/`slice`/`for_each`/`fold`/`push` |
| `ConcurrentMap[K, V]` | `get`/`contains`/`len`/`for_each_entry`/`fold_entries`/`insert`/`remove`/`get_or_insert` |
| `ConcurrentSet[E]` | `contains`/`len`/`for_each`/`fold`/`insert`/`remove` |
| `ConcurrentCounter[K]` | `increment`/`add`/`count`/`total` |

The container is a live `List` / `Map` / `Set` in the box heap. The single-entry readers (`len`,
`at`, `slice`, `get`, `contains`) copy only what they return. The writers (`push`, `insert`, `remove`,
`get_or_insert`) are the plain container operation under the box's update guard: they copy
their arguments in and nothing else. `get`/`contains`, `insert` and `get_or_insert` are O(1)
expected.

**The walk rule.** `for_each`, `fold`, `for_each_entry` and `fold_entries` copy the collection once
(`inner.get()`) and loop over the copy. A walk therefore visits **exactly the entries present when it
starts, each once**, whatever the callback or another task removes or adds meanwhile, and the
callback may call any method of the collection it walks. The price is one copy of the collection per
walk; to reduce a big container without copying it, do the reduction inside the box
(`xs.inner.read(..)` still copies; `Shared.read(f)` does not).

```chezzi
import ConcurrentList, ConcurrentMap from std.concurrency.collection

# Each spawned worker reduces its own view of the SAME shared container — no per-worker copy of the inner.
fn sum_shard(xs: ConcurrentList[int]) -> int:
    return xs.fold(0, fn(a: int, x: int) -> int: a + x)   # one snapshot of the list, then a loop

fn sum_values(m: ConcurrentMap[str, int]) -> int:
    return m.fold_entries(0, fn(a: int, k: str, v: int) -> int: a + v)   # per-entry reduce over a shared map

big := ConcurrentList(RwShared(List(0..1000000)))   # wrap existing data; `big.inner` is the box
```

Reduce into a **different** box — an `AtomicInt` counter or a local accumulator — which is the
fan-out pattern anyway. Calling a container method on the box itself (`r.fold(...)`, `r.at(i)`) is a
checker "no method" error whose help line names the collection
(`use ConcurrentMap from std.concurrency.collection`).

> **GC cost of holding a big container in a box.** Each box caches whether its payload can root a
> heap object at all; a pure-data payload is **skipped** by the collector, so holding a big container
> in a `Shared`/`RwShared`/`Channel` costs the same per GC pass as holding it in a plain `List`.
>
> **Reads are O(1); each *store* pays one walk.** `set`/`write`/`send`/`store` summarises the new
> payload once (O(payload), ~+20% on a 100k-element `RwShared.set` — measured in
> [`benchmarks.md`](../../contributing/benchmarks.md)), so a rebind into a box is *not* as cheap as
> rebinding a plain `List`. The walk runs **before** the value lock is taken, so it never lengthens the
> window in which a writer blocks the readers.

**The collections — `std.concurrency.collection`.** A raw `RwShared[Map[...]]` box is the primitive
for a shared table, but the `read`/`write` closures are verbose and the *compound* mutations
(insert-if-absent, increment a count) must be done inside a **single** `write` lock or they race. The
pure-Chezzi `std.concurrency.collection` module bakes the single-lock idiom into four generic
structs — **`ConcurrentList[E]`**, **`ConcurrentMap[K, V]`**, **`ConcurrentSet[E]`** and
**`ConcurrentCounter[K]`** — each over one `RwShared` box (`c.inner`), built with `X.new()` or
`X(RwShared(data))`. A struct of all-sendable fields crosses the airlock as a **shared handle** (same
as `RwShared` itself), so the wrappers keep the cross-task sharing; `get`/`len`/`count`/`snapshot` are
concurrent reads and `insert`/`push`/`add`/`increment`/`get_or_insert` are exclusive writes (the compound
ones atomic in one lock). See [`stdlib.md`](../stdlib/01-builtins.md) §5 → `std.concurrency.collection` and
`examples/concurrent_collection.chz`.
