### `std.concurrency.collection` — thread-safe collections over `RwShared`
Pure-Chezzi generic structs, each wrapping one `RwShared` box (many concurrent readers **or** one
exclusive writer): **`ConcurrentList[E]`**, **`ConcurrentMap[K, V]`**, **`ConcurrentSet[E]`**,
**`ConcurrentCounter[K]`**. They are the user-facing surface of a shared container: the box itself has
only `get`/`set`/`read`/`write`, and the per-entry primitives these methods call are `_` names internal
to std. `import std.concurrency.collection` (or `as col`), or
`import ConcurrentList from std.concurrency.collection`. The dotted path resolves to
`std/concurrency/collection.chz` (a nested std module, with no special-casing).

**Why over raw `RwShared`:** raw `read`/`write` closures are verbose, a walk of `get()`'s live value
races the writers, and the **compound** mutations (insert-if-absent, increment a count) MUST happen
inside a **single** `write` lock or they race. The collections' keyed reads and writes touch ONE entry
and build in the correct single-lock idiom. A walk (`for_each_entry`, `fold_entries`) takes one
shallow copy under the read guard.

**Sharing:** a task gets the wrapper struct by handle, like every value (M3.4), so the parent sees a
spawned task's mutation after the join.

**Construction:** call the static `new()` with the type arguments (**`ConcurrentList[int].new()`**,
**`ConcurrentMap[str, int].new()`**, **`ConcurrentSet[int].new()`**, **`ConcurrentCounter[str].new()`**)
or let an annotation give them (`m: ConcurrentMap[str, int] = ConcurrentMap.new()`). To wrap existing
data use the field constructor over a box: `ConcurrentList(RwShared(xs))`, `ConcurrentMap(RwShared({}))`;
`c.inner` is the box, so `c.inner.get()` returns the live value and `c.inner.set(v)` /
`c.inner.write(f)` replace it. With an empty literal and no annotation, `K`/`V` are open and the binding is an error
on that line (``cannot infer the type arguments of `m` ``); prefer `new()`. A use-site `RwShared({})`
means user code also imports `RwShared` from `std.concurrency` (`import RwShared from std.concurrency`);
`import std.concurrency.collection` binds only that submodule.

**Reentrancy:** like raw `RwShared`, a `write` closure that re-enters the **same** box's update guard
FAULTS instead of hanging or losing the inner write. Every collection method is flat (no nested
locking), so user code hits this only if it calls a writer from inside another writer's closure over
the same box. The looping readers (`for_each`, `fold`, `for_each_entry`, `fold_entries`) never hold the
lock across the callback, so a callback may read or write the same collection; the walk is not one
atomic snapshot.

**`ConcurrentList[E]`**: thread-safe list over `RwShared[List[E]]`.

| member | signature | concurrency / semantics |
| --- | --- | --- |
| `.push(item)` | `(E) -> None` | **write guard**. Appends `item` by handle; a plain list push on the box's live list. |
| `.len()` | `() -> int` | **concurrent read**. |
| `.at(i)` | `(int) -> E?` | **concurrent read**, constant time. Out of range is `None`, never a fault; a negative `i` counts from the end, as `xs[i]`. |
| `.slice(r)` / `c[a..b]` | `(Range) -> List[E]` | **concurrent read**; a new list of the elements (by handle) the `Range` selects, resolved as `xs[r]` resolves it on a `List` (negative bound, inclusive end, clamping, step). |
| `.for_each(f)` | `(fn(E) -> R) -> None` | Calls `f` once per element, in order, one element in the caller's heap at a time. |
| `.fold(init, f)` | `(R, fn(R, E) -> R) -> R` | Reduces in order; `R` is inferred from `init`. |
| `.snapshot()` | `() -> List[E]` | **concurrent read** returning a shallow **copy** of the whole list (the elements by handle). |

**`ConcurrentMap[K: Hashable, V]`**: thread-safe map over `RwShared[Map[K, V]]`. No method copies
the whole map except `snapshot`. `get`/`contains`/`len`/`snapshot` are **concurrent reads**;
`set`/`remove`/`get_or_insert` are single-entry writers under the box's **one update guard**. Lookups
and inserts are O(1) expected (the stored map keeps the heap map's hash index). `remove` is O(n), as
`Map.remove`.

| member | signature | concurrency / semantics |
| --- | --- | --- |
| `.get(key)` | `(K) -> V?` | **concurrent read**. `?v` / `None`. |
| `.insert(key, val)` | `(K, V) -> V?` | **update guard**. Insert or overwrite; returns the replaced value, or `None` for a new key. An overwrite keeps the stored key, as `d[k] = v`. A bare statement warns; write `_ := m.insert(k, v)`. |
| `.remove(key)` | `(K) -> V?` | **update guard**. Returns the removed value, or `None` if absent. A bare statement warns; write `_ := m.remove(k)`. |
| `.contains(key)` | `(K) -> bool` | **concurrent read**. |
| `.len()` | `() -> int` | **concurrent read**. |
| `.get_or_insert(key, default)` | `(K, V) -> V` | **COMPOUND-ATOMIC**: the probe and the insert run under ONE update guard with no user closure, so a concurrent `remove` cannot slip between them. Returns the existing value, or `default` if it was absent. |
| `.for_each_entry(f)` | `(fn(K, V) -> R) -> None` | Calls `f` once per entry present when the walk starts. The walk copies the whole map once (one snapshot), so a change `f` makes is not visited. |
| `.fold_entries(init, f)` | `(R, fn(R, K, V) -> R) -> R` | Reduces the entries; `R` is inferred from `init`. |
| `.snapshot()` | `() -> Map[K, V]` | **concurrent read** returning a shallow **copy**: later inserts and removes do not change it; its values are the live ones. |

**`ConcurrentSet[E: Hashable]`**: thread-safe set over `RwShared[Set[E]]`.

| member | signature | concurrency / semantics |
| --- | --- | --- |
| `.insert(item)` | `(E) -> bool` | **write guard**. Inserts one element without copying the set; `true` when new, `false` if present. O(1) expected. |
| `.remove(item)` | `(E) -> bool` | **update guard**. `true` when the element was present. O(n), as `Set.remove`. |
| `.contains(item)` | `(E) -> bool` | **concurrent read**, O(1) expected. |
| `.len()` | `() -> int` | **concurrent read**. |
| `.for_each(f)` | `(fn(E) -> R) -> None` | Calls `f` once per element, in insertion order. |
| `.fold(init, f)` | `(R, fn(R, E) -> R) -> R` | Reduces in insertion order; `R` is inferred from `init`. |
| `.snapshot()` | `() -> Set[E]` | **concurrent read** returning a shallow **copy** of the whole set. |

A writer called from a user `hash`/`eq` that re-enters the same box faults like `write` does: each
writer takes the update guard that `set`/`write` take.

**`ConcurrentCounter[K: Hashable]`**: thread-safe frequency table over `RwShared[Map[K, int]]`.
`count`/`total` are **concurrent reads**; `increment`/`add` take the **exclusive write lock** and do
their read-modify-write inside **one** closure, so N tasks that each increment the same key produce an
**exact** final count (no lost updates).

| member | signature | concurrency / semantics |
| --- | --- | --- |
| `.increment(key)` | `(K) -> None` | **exclusive write**, atomic RMW `+1` (created at 1 if absent). |
| `.add(key, n)` | `(K, int) -> None` | **exclusive write**, atomic RMW `+n` (`n` may be negative; created at `n` if absent). |
| `.count(key)` | `(K) -> int` | **concurrent read**, **0 if absent**. |
| `.total()` | `() -> int` | **concurrent read**, sum of all counts. |

**Not provided:** a concurrent **queue** is `Channel[T]`; an **atomic scalar** is `Atomic`. There is no
`ConcurrentQueue`.

### `std.concurrency.pmap` — scoped parallel map
`import pmap from std.concurrency.pmap` (or `import std.concurrency.pmap`). Pure Chezzi over a
`parallel:` nursery + `Channel[T]`.

| function | signature | semantics |
| --- | --- | --- |
| `pmap` | `pmap[T, U](xs: List[T], f: fn(T) -> U) -> List[U]` | spawn one task per element, run `f` on each in parallel, return the results in **submission order** (`[f(xs[0]), f(xs[1]), …]`). |
| `pmap_limited` | `pmap_limited[T, U](xs: List[T], f: fn(T) -> U, limit: int) -> List[U]` | same, but at most `limit` tasks run `f` at once (a channel-as-semaphore token bucket). `limit > 0` required: `limit <= 0` faults with a message naming `limit`, before the nursery is entered. |

The result order is deterministic: the helper reassembles results by submission index (a `sort_by_key`
on the tagged results), **never** by completion order. The nursery lives inside the helper and joins
before the collect, so a task never outlives the call (structured concurrency); each task gets `f` by
handle. `pmap_limited` is also the standard concurrency limiter: use it to cap
parallel calls into a rate-limited resource.

### `std.concurrency.Task` — result handles for `Executor` work
`ex.submit(f)` returns a `Task[T]` (after `import Executor from std.concurrency`). It is pure Chezzi over
the sealed cap-1 channel of the job's outcome. Every copy of the handle reads that channel, so
a spawned task or an Executor job holding a copy gets the same answer as the parent (CPython `Future`).
`ex.submit` is the one submit form.

| item | signature | semantics |
| --- | --- | --- |
| `Executor.submit` | `submit[T](self, f: fn() -> T) -> Task[T]` | Submit `f` for detached execution and get a handle for its result. The work STARTS at the submit; `shutdown()` (or the program-exit join) waits for it. |
| `Task.get` | `get(self) -> T!` | Block until the outcome is available, then return it: `?value`, or `!message` when the job faulted or `cancel()` cancelled it (CPython `Future.result()`, a `T!E` in place of a raise). A job fault appears only here. Idempotent, and the same answer in every task. See the identity rules below. |
| `Task.done` | `done(self) -> bool` | `true` once the job has finished, faulted, or been cancelled by `cancel()`, in every task holding a copy of the handle. Never blocks, and never reads `false` after `true` (it reads `concurrency.is_settled`). A faulted job is done; `get()` then returns its `!e`. |
| `Task.cancel` | `cancel(self) -> None` | Drop the job when it is held, cut it at its next cancellation point when it runs. A job that already returned keeps its value. |

`Task.get` identity:
- Every call, in every task that holds the handle, returns the same object (M3.3: the sealed channel
  holds the outcome by handle), so `a := t.get(); a.push(3)` shows in the next `t.get()`, in any task
  (CPython `fut.result() is fut.result()`).
- A task that `cancel()` cancelled and a held job that `cancel()` dropped return `task cancelled: cancel() stopped it before it finished` at
  once (CPython: `CancelledError`).

Canonical shape: submit every task, `shutdown()`, then `.get()` each. **Determinism rule:** a `Task`'s
value is deterministic (it is `f()`); only *when* it runs varies (the OS-thread workers race), so
`.get()` is identical across runs **as long as you await in a fixed (e.g. submission) order**. There is
no `join_next()`/select-on-completion API: completion order is nondeterministic.

### `std.concurrency.broadcast` — pub/sub over `Channel`

**`Broadcast[T]`**, **`Receiver[T]`** and **`Lagged`**: the tokio `broadcast` contract in pure
Chezzi. `import Broadcast, Receiver, Lagged from std.concurrency.broadcast`. Construct with
**`Broadcast[T].new(cap)`**. `b.subscribe()` gives a `Receiver[T]`, `b.send(v)` does not wait for receivers, and
`b.close()` ends every receiver's `for`. `rx.recv() -> T!Lagged`, `rx.try_recv() -> (T!Lagged)?`,
and `for r in rx:` gives `T!Lagged`. A receiver that fell more than `cap` values behind gets
`!Lagged(missed)` once, then the oldest value still held. The full contract is in
[`02-channel.md` §5c](../concurrency/02-channel.md).

### `std.cmp` — ordering generics (`Comparable`)
`max[T: Comparable](a, b) -> T` · `min[T: Comparable](a, b) -> T` ·
`clamp[T: Comparable](x, lo, hi) -> T`.

- `Comparable` embeds `Eq`, but a struct/enum satisfies `Eq` **structurally**, so defining
  `compare(self, other: Self) -> int` is enough. An `eq` is optional and only needed to *override* `==`.
  int/float/str satisfy `Comparable` intrinsically, with no method to write.
- Its `compare` is **total on floats**: a `NaN` operand returns an ordering int, never a fault. It uses
  the total order of `List.sort()`/`sort_by_key`/`min`/`max` (`f64::total_cmp`, `NaN` to one end;
  `+0.0` and `-0.0` are Equal, so sort is stable across them and `min`/`max` keep the first). The
  intrinsic `compare` is reachable only through a `[T: Comparable]` bound: `1.5.compare(2.0)` on a
  concrete scalar is `has no method 'compare'`, like `(5).str()`.
- The `<`/`<=`/`>`/`>=` *operators* stay IEEE (`false` for every `NaN` comparison): that is the one
  divergence.
- `min` and `max` are written with `<`, so they obey the **operator** rule, not the total order: on a
  non-`<` tie both return the **first** argument (Python's rule), so a `NaN` operand propagates from the
  left (`min(nan, 1.0)` and `max(nan, 1.0)` are both `nan`; `min(1.0, nan)` and `max(1.0, nan)` are both
  `1.0`). Filter `NaN` first if that matters.
- `clamp` **faults** (`cmp.clamp: inverted bounds (lo > hi)`) when `lo > hi`, as Rust `clamp` panics;
  `lo == hi` is not inverted and clamps to that single point.

### `std.bisect` — binary search & sorted-insert (Python `bisect`)
Over an ascending-sorted `List[T: Comparable]` (compares with `<`, which dispatches through
`Comparable`). `bisect_left(xs, x) -> int` returns the leftmost insertion index (before equal elements);
`bisect_right(xs, x) -> int` (also `bisect(xs, x)`) the rightmost (after equal elements).
`insort_left(xs, x)` / `insort_right(xs, x)` insert `x` in place and keep `xs` sorted (O(n)
grow-then-shift, the cost of Python's `insort`).
**Limits:** no `key: fn(T) -> K` variant and no bare `insort` alias. `xs` MUST already be sorted
ascending; otherwise the results are undefined.

### `std.memoize` — result caching (`functools.cache`)
`memoize1(f: fn(K) -> V) -> fn(K) -> V` wraps `f` so each distinct argument is computed once and the
result cached (`K: Hashable`; `Hashable` embeds `Eq`, so it is a map key).

- The cache is an `RwShared[Map[K, V]]`, so the wrapper works from any task: a call in a spawned task or
  an Executor job reads and fills the same cache as its creator, as CPython `functools.cache` does from
  any thread. Lookups and inserts are O(1) expected.
- The cache holds each result by handle (M3.3), so every call with an equal argument, in every task,
  returns the **same object** (CPython `f(1) is f(1)`; a mutation of the result is visible to every
  caller).
- Two tasks that miss one key at the same moment may both run `f`, as with CPython's `functools.cache`;
  the first insert wins and both get it.

**Limit:** single-argument only. A tuple of Hashable args is a valid map key, so you can write an N-arg
wrapper that keys a `Map[(A, B), V]`; or curry, or pack the args into a struct with `hash` and `eq` and memoize
the single-arg wrapper.

### `std.duration` — Go-like first-class time spans
Pure Chezzi (no native seam). `import std.duration`. `Duration` (access as `duration.Duration`) is a
plain struct over a single int of **milliseconds**.
- **Constructors** (free fns): `millis(n)`, `seconds(n)`, `minutes(n)`, `hours(n)` → `Duration`.
- **Accessors** (methods): `d.as_millis() -> int`, `d.as_seconds()/as_minutes()/as_hours() -> float`.
- **Arithmetic** (methods): `d.add(o)`, `d.sub(o)`, `d.scale(k: int)` → `Duration`.
- **`d.to_string() -> str`**: Go `time.Duration.String()` decimal-seconds shape: `"0s"`, `"250ms"`,
  `"1.5s"`, `"1m30s"`, `"1h0m0s"`, negatives prefixed `"-"` (`"-1.5s"`).
- **`parse(s: str) -> Duration!`**: inverse of `to_string`; also accepts Go's looser forms: optional
  leading `+`/`-`, one or more `<number><unit>` groups (units `h`/`m`/`s`/`ms`, unordered and summed),
  decimal magnitudes (`"1.5h"`, `".5s"`, `"0.25s"`), and a bare `"0"`. Malformed input (empty, no unit,
  unknown unit, multiple dots, trailing dot, oversized magnitude) is a **clean `!e`**, never a fault. A
  magnitude is accepted iff the WHOLE duration fits `i64` milliseconds, checked per unit, so
  `"2562047788015ms"` (2.5e12 ms) and `"100000000000h"` (3.6e17 ms) are both `?v`, while
  `"2562047788016h"` (9223372036857600000 ms, above `i64::MAX`) is `!"duration out of range"`.
  Round-trips exactly (`parse(d.to_string())` ⇒ `d`) for every magnitude the parser accepts, both `int`
  extremes included: `parse(millis(i64::MAX).to_string())` and `parse(millis(i64::MIN).to_string())`
  both round-trip. The negative side reaches one further, as in Go: `parse("-9223372036854775808ms")`
  is `?v` (`i64::MIN`) while `"9223372036854775808ms"` is `!e`.
- **`since(start: float) -> Duration`**: elapsed time since a `time.monotonic()` reading (imports native
  `std.time`; floors to whole ms). **`sleep(d: Duration)`**: delegates to native `sleep_ms`.

**Why milliseconds:** ms matches `sleep_ms`/`timer(ms)` and overflows an i64 only at ~292 **million**
years (a Go nanos i64 caps at ~292 years). Microseconds/nanoseconds are **unrepresentable**:
`parse("1us")`/`parse("1ns")`/`parse("1µs")` are a clean `!e`, and a fractional literal below 1ms
(e.g. `"0.0005s"`) floors to `0ms`.
