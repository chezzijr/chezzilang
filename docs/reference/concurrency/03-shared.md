## 6. `Shared[T]` — the cross-task mutable box

> **Members of `std.concurrency`.** `Shared`, `RwShared`, `Atomic`, `AtomicInt` and `Executor` are
> ordinary members of `std.concurrency`: `import Shared from std.concurrency` (an alias works) binds the
> bare name, and `import std.concurrency` gives `concurrency.Shared(0)`. A whole-module import binds no
> bare name. `Channel` is global; `timer(ms)` is an ordinary `std.time` function
> ([04-timer-wait.md](04-timer-wait.md)). The constructors lower to dedicated opcodes, so the import
> costs nothing at run time.

A task shares the parent's captured locals and arguments by handle ([§7](07-sendability.md#7-what-crosses-a-task-boundary),
M3.4, the Go rule), so two tasks that write one value without a lock race. For mutable state that
several tasks write, use **`Shared[T]`** — a box whose writes run under its guard (Go's
`mu.Lock(); f(v); mu.Unlock()`).

```chezzi
import Shared from std.concurrency

s := Shared(0)             # one owner holds the value
parallel:
    spawn bump(s)          # each task gets the box by handle: one box, one guard
    spawn bump(s)
print(s.get())             # 2

fn bump(s: Shared[int]):
    s.update(fn(x): x + 1)
```

| Method | Signature | Notes |
|--------|-----------|-------|
| `get`    | `get(self) -> T` | the live value, by handle (takes the guard for the read) |
| `set`    | `set(self, v: T) -> None` | store `v` (by handle) |
| `update` | `update(self, f: fn(T) -> T) -> None` | read-modify-write: `f` runs in the caller's task on the live value while the task holds the guard; `f`'s result is the new value |
| `read`   | `read(self, f: fn(T) -> R) -> R` | run `f` on the live value under the guard; return `f`'s result |

> **`get()` returns the live value; write it only inside `update`.** M3.3: a box holds ONE value by
> handle (Go `struct { mu sync.Mutex; v T }`). `x := s.get()` then `x["z"] = 1` changes the box's
> value, outside the guard: a race with another task's `update` is the user's bug, as in Go. Write
> under the guard with `update` (or `set` a whole new value).

**The ladder:** a value one task writes → a value tasks share and order by a join or a `Channel` →
`Shared[T]` (a cross-task box). A mutable value (a one-field `struct`, a `List`, …) is one object
in every task that holds it (M3.4); `Shared[T]` adds the guard that orders the writes.

### Where the value lives

A `Shared[T]` holds its value by handle, in the run's one heap. `update(f)` and `read(f)` run `f` in
the caller's task, on the live value, while the task holds the box's guard:

```chezzi
s := Shared(Table(keys, 0))            # keys: 100000 items
s.update(fn(t): hit(t))                # hit: t.hits += 1; return t   -- in place, no copy
n := s.read(fn(t): t.hits)             # f's result, no copy
```

**The closure is an ordinary call in the caller's task.** Its captures are the caller's bindings, so a
write to one lands (Go: a closure under `mu.Lock()`). It reads and writes the run's one copy of each
module global (M3.1). A closure that stores a global or a capture into the state (`s.update(fn(_):
G)`) makes the state that object; store `G.copy()` to keep the box's own value. A closure may block
(a channel wait, a sleep): the guard belongs to the task and stays with it, and every other user of
the box waits.

**Re-entry and faults.**

- `get` / `read` / `set` / `update` of a `Shared` from inside that box's own `update` or `read`
  closure fault with `already holds the update guard` (Go: `Lock` inside `Lock` is `fatal error: all
  goroutines are asleep - deadlock!`). A `print` of the box takes no guard (Go's `fmt` takes no lock):
  it shows the box's value as it is.
- A fault inside the closure is raised in the caller, `recover:` catches it, the box keeps serving
  and the state is what the closure left.
- A slow closure blocks every user of the box and holds its worker (the Elixir `Agent` warning
  applies: keep it short). A child of a `parallel:` inside the closure that reads or writes the
  **same** box can never be served (the closure holds the box until the nursery joins), so the run
  stops with a `deadlock` that lists the child as `waiting for the Shared guard` at its own `get`; it
  does not hang. Pass
  the child the value it needs instead of the box.
- A task's open nurseries stay its own inside a box closure: when a child of an enclosing `parallel:`
  faults, an owner that waits inside the closure is cut there, and a `recover:` inside the closure
  does not catch the child's fault. A `parallel:` opened inside the closure joins the scheduler of the
  task's enclosing `parallel:`, so a nursery deadlock there is reported the same way
  (`deadlock: N tasks are blocked and none can proceed`, then each task, exit 1).
- One shape declines the verdict and hangs: under an enclosing `parallel:` whose sibling is blocked,
  a closure that itself waits on a channel nobody can feed.
- A reader of a `RwShared` whose writer is blocked for good is listed as `waiting for the RwShared
  guard` at its own `read` / `get`, as a `Shared` user is.

`Shared.read(f)` runs `f` on the live value, so a mutation `f` makes persists. `RwShared` (§6f) is the
same one-value box behind a read-write guard.

**When to pick which box.**

| | `Shared[T]` | `RwShared[T]` |
|---|---|---|
| lock | one user at a time | many readers or one writer |
| `update` / `write` | closure runs on the live value, in place | the same, exclusive |
| `read(f)` | `f` runs on the live value under the guard; readers queue | `f` runs on the live value under the read guard; readers run in parallel; a `write` inside it faults |
| `get()` | the live value, by handle | the live value, by handle |
| pick it when | you mostly write, or read a small part of a big value (`read`) | reads dominate and readers must not queue |

**Display rule.** `print` and interpolation of a `Shared` / `RwShared` / `Atomic` render what
the box holds structurally, with no user `str` hook: `"{Shared(P(1))}"` is `Shared(P(x=1))` even when
`P` defines `str`. A box that holds itself, directly or through a container or a struct, prints the
cycle marker like any other container cycle:

```
b: Shared[N?] = Shared(None)
n := N(1, b)
b.set(?n)
print(n)        # N(id=1, box=Shared(N(id=1, box=Shared(...))))
```

The renderer takes no lock and no guard: it reads the box's one slot and renders that value, as
Go's `fmt` reads a struct's fields. A box that holds one list twice renders it twice
(`Shared([[1], [1]])`, as CPython prints `[[1], [1]]`). A fault message renders a box the same way,
so it never waits for a box that another task holds.

---

## 6b. `Atomic[T]` — the cross-task atomic box

`Atomic[T]` is `Shared[T]`'s sibling for **atomic operation primitives** (compare-and-swap, exchange,
fetch-add) rather than `Shared`'s closure-based `update`. Same shape: one box, many tasks, each with
the box by handle; the box holds the value itself, so `store` and `load` copy one word
under a lock and every task shares the stored object (Go `atomic.Value`); constructed value-first (`Atomic(v)`,
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

**`cas` compares as `==` does, and rejects a payload that holds a `fn` value** (`Atomic[fn() -> int]`,
a struct field or `List` element of `fn` type). Go's func is uncomparable, so Go's
`atomic.Value.CompareAndSwap` panics `comparing uncomparable type` on the same shape.

- The error is at the `cas` call, so `load`/`store`/`exchange` of a fn stay legal.
- A named fn, a builtin fn (`ord`), a std native fn (`math.sqrt`) and an `extern` fn are fn values
  like a closure, so `cas` rejects them too.
- A generic `fn f[T](a: Atomic[T], …)` hides the fn from the checker, so the runtime `cas` faults
  `Atomic.cas: the payload holds a function value, which cas cannot compare`. A builtin fn (`ord`)
  faults the same way.
- A handle payload (`Atomic(ch)`) compares by its core, so `cas(a.load(), …)` on it succeeds.
- An iterator or a generator payload compares by its object, as `==` does (Go: a pointer compares by
  address): `load()` returns the stored one, so `cas(a.load(), …)` on it succeeds, and `cas` with
  another iterator answers `false`.

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
writer**. Same shape as `Shared` — one box, many tasks, each with the box by handle; constructed
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
| `get`   | `get(self) -> T` | shared read guard; the live value, by handle (== `read(identity)`); write it only inside `write` |
| `set`   | `set(self, v: T) -> None` | exclusive write guard; overwrite |
| `read`  | `read(self, f: fn(T) -> R) -> R` | **shared** read guard: run `f` against the current value, return `f`'s result; **no** write-back. Many `read`s run concurrently |
| `write` | `write(self, f: fn(T) -> T) -> None` | **exclusive** write guard: `Shared.update` under the write lock — read-modify-write, store `f`'s return |

`write`'s read-modify-write is atomic across threads, exactly like `Shared.update`: the whole
`write`/`set` is serialised by the box's **update guard** (a process-wide wait-for graph, not a bare
lock), so concurrent writers can't lose each other's updates — a `set` racing an in-flight `write`
**blocks** behind the guard and lands; it is never silently overwritten.

**Where the value lives.** As with `Shared`, the box holds one value by handle (Go `sync.RWMutex`).
`write(f)` runs `f` in the caller's task on the live value, under the write guard; the closure rule of
§6 applies, and a fault inside `f` leaves the value as `f` left it. A reader (`get`, `read`, a
collection read) takes the read guard; `read`'s closure, and a key's `eq`, run on the live value under
it, so readers run in parallel and a mutation inside a `read` closure reaches the box (a race with the
other readers is the user's bug). A reader waits while a writer holds the box, or waits for it (Go
writer preference: a writer counts as waiting from its first failed acquire to its take, so no
reader slips in at the handoff). A write release admits every reader that waited during the write,
ahead of the writers that wait too (Go `sync.RWMutex.Unlock`), so back-to-back writers cannot
starve a reader. A nested `read` by a reader is granted while no writer waits; with
a writer waiting for the outer read it is a certain cycle (Go deadlocks) and faults with the lock
cycle text. A `print` takes no guard. The fast path is one atomic operation on the box's lock word;
the wait-for graph is read only on a contended acquire, so a cycle that passes through a read guard
(readers are anonymous in the lock word) is left to the run's deadlock verdict.

> **Reentrancy fault (same class as `Shared.update`):** a closure passed to `write` (or `set`) that
> re-acquires the **same** `RwShared`'s update guard — `write` or `set` inside `write` — is a
> length-1 wait-for cycle and **faults** (`"already holds the update guard"`), instead of hanging or
> silently losing the inner write. A cross-box wait-for cycle (two boxes each written from inside the
> other's closure) faults the same way; two tasks that merely contend for different boxes are not a
> cycle and complete normally. `get` / `read` of the box from inside its **own `write`** closure fault
> the same way. M3.3 (Q2): `write` or `set` nested in the same task's **`read`** waits for that read,
> a certain deadlock (Go `RLock(); Lock()`), so it faults with `already holds the read guard on this
> RwShared box`. `get` / `read` nested in `read` are legal while no writer waits; past a waiting
> writer they fault (`a lock cycle`). The re-entry text names the box kind (`Shared` or `RwShared`).

> **A guard wait in a task parks, as a goroutine's `sync.Mutex` wait does.** A contended
> `set`/`update`/`write` in a spawned task or an Executor job retries for a few microseconds, then
> parks the task: no OS thread per waiter, and a task that holds the guard can be preempted. `main`
> waits in place on its own thread. Details:
> [`concurrency-runtime.md`](../../design/concurrency-runtime.md#guard-waits).

### Fan out big shared data — use a collection

`get`/`read` hand out the live value (M3.3), so a reader that walks it outside the guard races the
writers. `RwShared[T]` itself has exactly four methods for every `T` — `get`, `set`, `read`, `write`
— so for a shared **container** use a collection from `std.concurrency.collection`:

| collection | methods |
|---|---|
| `ConcurrentList[E]` | `len`/`at`/`slice`/`for_each`/`fold`/`push` |
| `ConcurrentMap[K, V]` | `get`/`contains`/`len`/`for_each_entry`/`fold_entries`/`insert`/`remove`/`get_or_insert` |
| `ConcurrentSet[E]` | `contains`/`len`/`for_each`/`fold`/`insert`/`remove` |
| `ConcurrentCounter[K]` | `increment`/`add`/`count`/`total` |

The container is the box's one live `List` / `Map` / `Set`. The single-entry readers (`len`, `at`,
`slice`, `get`, `contains`) take the read guard and return elements by handle. The writers (`push`,
`insert`, `remove`, `get_or_insert`) are the plain container operation under the write guard; they
store their arguments by handle. `get`/`contains`, `insert` and `get_or_insert` are O(1) expected.

**The walk rule.** `for_each`, `fold`, `for_each_entry` and `fold_entries` take a shallow copy of the
collection under the read guard (`inner.read(fn(c): c.copy())`) and loop over the copy with no guard
held. A walk therefore visits **exactly the entries present when it starts, each once**, whatever the
callback or another task removes or adds meanwhile, and the callback may call any method of the
collection it walks. The elements are the live ones, by handle. To reduce a big container without
the shallow copy, reduce inside `xs.inner.read(..)`.

```chezzi
import ConcurrentList, ConcurrentMap from std.concurrency.collection
import RwShared from std.concurrency

# Each spawned worker reduces its own view of the SAME shared container — no per-worker copy of the inner.
fn sum_shard(xs: ConcurrentList[int]) -> int:
    return xs.fold(0, fn(a: int, x: int) -> int: a + x)   # one shallow copy of the list, then a loop

fn sum_values(m: ConcurrentMap[str, int]) -> int:
    return m.fold_entries(0, fn(a: int, k: str, v: int) -> int: a + v)   # per-entry reduce over a shared map

big := ConcurrentList(RwShared(List.from_iter(0..1000000)))   # wrap existing data; `big.inner` is the box
```

Reduce into a **different** box — an `AtomicInt` counter or a local accumulator — which is the
fan-out pattern anyway. Calling a container method on the box itself (`r.fold(...)`, `r.at(i)`) is a
checker "no method" error whose help line names the collection
(`use ConcurrentMap from std.concurrency.collection`).

> **GC cost of holding a big container in a box.** M3.2 / M3.3: a box and a channel hold one word
> per value, so a store is O(1) and the collector traces the value like any other heap value.

**The collections — `std.concurrency.collection`.** A raw `RwShared[Map[...]]` box is the primitive
for a shared table, but the `read`/`write` closures are verbose and the *compound* mutations
(insert-if-absent, increment a count) must be done inside a **single** `write` lock or they race. The
pure-Chezzi `std.concurrency.collection` module bakes the single-lock idiom into four generic
structs — **`ConcurrentList[E]`**, **`ConcurrentMap[K, V]`**, **`ConcurrentSet[E]`** and
**`ConcurrentCounter[K]`** — each over one `RwShared` box (`c.inner`), built with `X.new()` or
`X(RwShared(data))`. A task gets the wrapper struct by handle, like
every value (M3.4), so the wrappers keep the cross-task sharing; `get`/`len`/`count`/`snapshot` are
concurrent reads and `insert`/`push`/`add`/`increment`/`get_or_insert` are exclusive writes (the compound
ones atomic in one lock). See [`stdlib.md`](../stdlib/01-builtins.md) §5 → `std.concurrency.collection` and
`examples/concurrent_collection.chz`.
