## 7. Sendability

**The model — spawning a task copies its environment (fork-like).** A `spawn`ed task does not share the
parent's locals. It receives its **own isolated copy** of everything it captures — captured locals
are deep-copied. Module globals are NOT copied (M3.1): every task reads and writes the run's one
copy, as a goroutine does a package `var` ([§2](01-model.md#2-why-shared-nothing-and-why-not-the-go-memory-model)),
and a **closure** or generator reads the run's globals wherever it runs. Two deliberate differences
from a real `fork`:

1. It copies only the **reachable captured environment**, not the whole heap.
2. **Explicit concurrency handles cross by SHARED reference, not by copy** — `Channel`, `Shared`,
   `RwShared`, `Atomic`, `Executor`, and the socket/reader/writer handles carry their one underlying
   core across, so all tasks reach the *same* mailbox/box/queue/fd. These handles are the *only* way
   tasks share mutable state; a captured value is copied. A plain captured value mutated in a task
   changes only that task's copy, and (D4) that write faults there, except the ceilings below.
   M3.2: a value SENT over a `Channel`, stored in an `Atomic` or returned through a `Task` is not
   copied: the receiver gets the sender's object itself (Go), so a write by either side is seen by
   the other.

> **One-sentence mental model:** a spawned task gets its **own copies** of what it captures
> (read-only), holds **shared handles** to channels and `Shared` boxes, and talks **only** through
> those — a channel and an `Atomic` hand over the value itself (M3.2).

```chezzi
fn worker(id: int, prefix: str, out: Channel[str]):
    out.send("{prefix}-{id}")   # ✓ id/prefix copied & read; result leaves via the Channel
```

### What is sendable

**Almost everything is sendable**: scalars, `str`, containers/structs of sendable contents, closures &
bare/`fn` and **recursive** local fns, **protocol existentials**, `.iter()` cursors, and **live
generators** — including one suspended mid-`recover:`. The residual **NOT-sendable** set is small and
each case is rejected cleanly (a recoverable fault or compile error, **never UB**):

- **[fundamental]** a **module namespace** itself: it is not a value, and the checker's `sendable`
  answers `false` for it alone. The I/O handles `Writer`, `Reader`, `Socket` and `Listener` DO cross,
  as shared handles (the file or socket lives in a core outside every heap); a regex `Match` and a
  `request.Response` are plain data structs and cross by deep copy; an FFI `ptr` crosses by value.
- **[not a real limit]** two **checker-unreachable** suspended-generator shapes (multi-frame,
  pending-`defer`), kept only as defensive guards.

| kind | crosses as |
|---|---|
| `int`/`float`/`bool`, `str` | copy |
| containers + structs whose contents are all sendable | deep copy |
| `Channel` (reply channels), `Atomic[T]`, `AtomicInt`, `Shared[T]`, `RwShared[T]` handles | shared handle |
| `Writer`, `Reader`, `Socket`, `Listener` handles | shared handle |
| a regex `Match`, a `request.Response` | deep copy, like a struct |
| a `std.cancel` `Token` (a struct over the above) | shared handles inside a copied struct |
| an `.iter()` snapshot cursor (a frozen data snapshot + read position) | deep copy, like a `list` |
| a user protocol existential (`Channel[P]`, protocol-typed spawn args) | deep copy of the erased witness |
| a closure or bare `fn` | by value: proto + deep-copied captures |
| a live generator | by value: an independent deep copy of its frozen frame |

**Not sendable (checker):** a **module namespace**. Capturing or passing it across the airlock is a
**compile error** at a direct spawn/`Channel` site — whether captured directly by a `spawn:` block,
or by a closure/nested-fn used as a `spawn f()` **callee**/**arg**. A **module-global** non-sendable value, by contrast, is a
**read-only global** resolvable in every task (like a free fn), **not** a per-task capture — reading it
inside a task is fine and it is never gated.

### Writes to a task's copy fault (D4)

Reassigning — or mutating in place (`.push`/`.insert`/`m[k]=v`/`s.field=x`) — a captured **local**
inside a task is a recoverable runtime fault (a **module global** is not a copy, M3.1, so a task's
write to it lands):

```
'<name>' is this task's copy: a write to it would be lost at the join; share it through Shared/Channel, or make a task-local copy with .copy()
```

The write to a captured local would never be seen by the parent or by sibling tasks. To produce output
visible to the parent, use a `Channel`, a `Shared` or a module global. Reads are always fine. The
checker rejects the provable subset at compile time ([§2](01-model.md#the-race-you-cant-write)).
Decision record: [`adr-d4-airlock.md`](../../design/adr-d4-airlock.md).

**Write sites the mark is checked at:**

- a struct field store (`p.f = v`);
- an index store on a `List`/`Map`/`bytearray` (`xs[i] = v`). A struct's own `set_index` is NOT
  checked — whatever it writes through is checked at its own call sites, so writing through a `Shared`
  field stays silent as real sharing;
- a captured-local cell store;
- `+=`-style in-place arithmetic on a container;
- every mutating native call: List `push pop reverse sort sort_by sort_by_key extend insert remove`,
  Map `insert remove update`, Set `insert remove`, bytearray `push pop extend`.

Reads never check the mark. A write through a `Shared`/`RwShared`/`Atomic*`/`Channel`/`Socket`/
`Listener` handle is real sharing and is never marked, so it is never checked either.

**Two shapes still lose the write silently** (measured on the release binary):

1. M3.2 closed the channel shape of this ceiling: a tuple `(xs, f)` or `(f, xs)`,
   `f := fn() -> None: xs.push(2)`, sent over a `Channel` to a spawned task is the sender's tuple,
   so both writes run and land on the one `xs` in either order (Go: both writes are seen;
   `airlock_task_local_fault_test.chz` and `airlock_task_local_silent_test.chz` pin one order each).
2. **Captured iterator/generator cursors.** Advancing a captured `Iterator` cursor and resuming a
   captured generator are not checked. This is the deep-copy semantics, not a lost write: the task's
   copy is independent, and the parent's own `g.next()` after the join returns its own next value.
   What the resumed generator writes inside its frame IS decided: a frame-local value the parent never
   saw is writable at every depth it built itself, and a list the generator yielded, stored or passed
   on faults. An Executor job, a spawn and a task copy's `Task.get()` /
   `memoize1` read all apply it; see [`adr-d4-airlock.md`](../../design/adr-d4-airlock.md) rule 2.
**A same-task round trip is a crossing** (owner decision 2026-10-22, TICKET-291, reversing the
2026-09-23 ruling). A closure holds its captures by reference only in the heap that built it. Every
rebuild of a closure -- a `Shared`/`RwShared` store read back (`get`, `read`, a concurrent
collection's `at`/`get`/`remove`), a relay task that stores it again -- rebuilds its captures as
copies, so a write through one faults, the task that stored it included. M3.2: a `Channel` send, an
`Atomic` load and a `Task` result rebuild nothing: they hand over the closure itself, so its
captures are the sender's bindings (Go: a closure sent over a channel writes its captured
variables). A `Task` result closure still faults when its capture is a job's copy (a spawn crossing,
until M3.4). Two rebuilds prove a capture is the only live copy, so its write runs:
- A closure that a `Shared.read` closure builds and returns. Its captures may reach an object
  the box keeps only when no write can change that object or anything below it: `[s[0]]` over
  `Shared([(1, 2)])` runs; over `Shared([(1, [2])])` or `Shared([[1]])` it faults, because the
  box keeps the list.
- A closure an `Executor` job returns (`t.get()`). The job's heap ends with the job, so the write
  faults only when a capture reaches a copy: a value the job received, or a box's state.

A closure over the box's own state, or over fresh state that holds a mutable part of it, faults.
`RwShared.read` runs its closure in the reader's heap over the reader's copy of the state
(`03-shared.md`), so `rw.read(fn(s): fn(): s.push(9))` returns a closure that writes the reader's
copy: no fault, and the box stays `[1]`. A plain
(non-closure) value received over a `Channel` (the sender's object) or read back from a
`Shared`/`RwShared` is never marked.

### Closures and functions cross by value

The airlock lowers a closure or bare `fn` **by value** — its `proto` (shared, read-only) + its
captures deep-copied recursively + its home module index. **A closure carries captures only — never
module globals.** A closure or generator, wherever it was created, reads and writes the run's one copy
of each module global (M3.1), so a receiver sees the sender's global writes:

```
g: List[int] = [1]
fn main():
    c := Channel[fn() -> str](1)
    parallel:
        spawn:
            g.push(2)                    # lands: g is the run's one list
            c.send(fn() -> str: "{g}")
    f := c.recv()
    print("{f()} {g}")                   # [1, 2] [1, 2], as in Go and CPython
main()
```

- A module-level `let` binding is a late load in every task (matching CPython and Go), so a write to
  it AFTER a closure is created is visible to a later call. Top-level `fn`s, imports, `native fn`s and
  `extern` fns are unaffected.
- A closure's own **captures** cross by value: a captured local reads as the sender left it. An alias
  of a module global that the PARENT made and the task captures (`inner := gl[0]` before the `spawn:`)
  is such a capture, so it is a crossing copy until M3.4; an alias the task makes itself is the
  global's own object.
- **The checker rule: a closure crosses iff its captures are sendable.** The function type is
  sendable, so `Channel[fn(int)->int]` type-checks and a closure sent over a channel or returned from a
  factory runs. The bare `fn` type cannot carry its captures, so the per-closure check runs at the
  airlock **sites**: a closure/nested-fn value whose captures include a **non-sendable local** (a native
  handle) at a `spawn f()` **callee** or `spawn f(g)` **arg** is a **compile error**, matching the
  `spawn:` block form. A protocol existential is sendable, so capturing one is fine; a witness that
  carries a non-sendable handle is caught at the runtime airlock. A native/FFI *fn value* —
  `math.sqrt`, an `extern` fn — is pure code and crosses by value.

### Identity across a crossing

**One source object produces one copy per crossing.** Every container, closure and cell earns a
per-crossing id, and a second reach is a back-reference to the first copy. So:

- **Self-referential DATA is sendable.** A struct/list/map/set/tuple/enum/cursor that points back at
  itself (`a.next = b; b.next = a`, a list holding itself, a map whose value refers to the map) crosses
  **any** airlock (`spawn:` block, `spawn` arg, `Channel.send`, `Shared`) and
  **round-trips** as an independent cyclic value with the same shape (like Python's `deepcopy`).
  `Map`/`Set` reuse the carried hash, so a cyclic key is never re-hashed.
- **A DAG alias stays one object.** The same node appearing twice crosses as one copy: mutating through
  one alias in a task is visible through the other, matching same-task reference semantics (`b := a`)
  and CPython's `copy.deepcopy`.
- **An `RwShared` store keeps aliases too.** A DAG alias stored in an `RwShared` crosses as ONE object,
  and `r.get()` on `outer = [inner, inner]` reads `2 2` like CPython.
- **A captured BINDING keeps its identity across the whole crossing.** Two sibling closures over one
  local still share one cell after crossing — one per **binding**, not one per reference — so the
  language's capture rule ([`syntax.md`](../syntax/01-lexical-and-literals.md): a write through a capture is visible in the
  defining scope **and across sibling closures**) survives inside the task (Go behaves the same):

  ```
  struct Ctr:
      inc: fn() -> None
      get: fn() -> int
  fn make() -> Ctr:
      n := 0
      fn inc():
          n = n + 1
      fn get() -> int:
          return n
      return Ctr(inc, get)
  ch := Channel[Ctr]()
  ch.send(make())
  d := ch.recv()
  d.inc()
  print(d.get())          # 2 after two incs — ONE binding, not one per reference
  ```

  One crossing spans everything that crosses together — a `spawn`'s callee/receiver + all args, a
  `spawn:` block's captures.
- **A recursive *local* `fn` is sendable.** A nested `fn` that calls itself captures its own name, so
  the closure's capture graph is cyclic. A recursive local `fn` — and a **mutually-recursive closure
  pair** — crosses **any** airlock (`spawn:` block, `spawn f()` callee, `spawn f(g)` arg,
  `Channel[fn].send`) and computes correctly. A recursive closure that also reads an outer local
  carries the outer local as an independent deep copy. A **mixed** struct+closure cycle (a
  self-capturing closure held inside a struct/list/map) round-trips too.

**Known ceilings** — each of the shape "two *independent* crossings reach the same cell":

- **The per-entry readers of a collection over an `RwShared` box.**
  `at`/`get`/`contains` rebuild ONE piece of the stored container per call, so each piece is an
  independent copy — two `at()` calls are two crossings and can never share. A whole-container
  `get()`/`read()`, and `slice` (one call returning a container), are one crossing and DO share. One
  shape cannot be split into pieces: an element whose cycle closes through the ROOT container
  (`a.back = xs; ConcurrentList(RwShared(xs)).at(0)`). There the view rebuilds the **whole** container
  and hands back the piece out of it, so the cycle survives (CPython's `copy.deepcopy(xs[0])` copies the
  container too). Such a view costs O(container) *on cyclic data only*, and the second and later
  occurrences of an alias pay a whole-root rebuild (measured in
  [`benchmarks.md`](../../contributing/benchmarks.md)).

**The depth cap.** The only value cycle that still rejects is one threaded through a live
**generator's parked frame** (below). A genuinely >10000-deep **acyclic** nest hits the depth guard
(`MAX_STRUCTURAL_DEPTH = 10000`, the same bound the display / `==` paths use): a **recoverable**
`maximum structural depth (10000) exceeded (cyclic data structure?)` fault, stamped with the airlock
site and catchable by `recover:`. A spawn copies no module global (M3.1), so a deep global never
reaches it.
Large-but-**shallow** data (e.g. a 100k-element list) crosses fine — the counter measures nesting
depth, not element count.

### Generators

A live generator crosses **any** task airlock **as data** (passed/captured into a `spawn`, or stored
in a `Channel`/`Shared`/`RwShared`/`Atomic`) as an **independent deep copy** of its `proto`, backing
closure and parked operand-stack/args, rebuilt fresh on the receiver, so advancing one copy never
affects the other.

- **One generator, one copy per crossing.** A live generator reached TWICE in one crossing (`a := g`
  then `spawn: a.next(); g.next()`, or a generator nested in
  another generator's parked frame passed alongside it) crosses as ONE copy, so `a` and `g` still
  denote one generator in the task (CPython prints `t 2 3` for `g1 := nums(); g2 := g1` read from a
  thread; Chezzi's task copy does too).
- **A suspension inside a `recover:`** crosses too; the recover boundary resumes intact.
- **A module-global generator** does not cross (M3.1): a task that reaches it drives the run's one
  generator, as a CPython thread does. Two tasks that drive it at once race, and the loser faults
  `generator already running` (CPython: `ValueError: generator already executing`).
- Every parked slot is wired recursively, so a **non-sendable parked slot** rejects at the crossing.

Reject shapes:

| shape | result |
|---|---|
| a cycle that re-enters the generator on the serialize stack | fault `a generator cannot be sent across tasks as part of a reference cycle` |
| a generator crossed **while it is running** (its own body sends a value holding it) | fault `a running generator cannot be sent across tasks` (CPython: `TypeError: cannot pickle 'generator' object`) |
| a >10000-deep acyclic nest held live across a `yield` | the depth-cap fault |
| a suspension with a pending `defer`, or a multi-frame suspension | `... cannot be sent across tasks` (checker-unreachable) |

A cycle whose root is a container the generator's frame merely points at
(`xs.push(gen_over_xs)` then `c.send(xs)`) round-trips correctly, the copied frame aliasing the copied
container. A parked recursive local `fn` or a parked self-referential struct/list/map round-trips like
any other capture.
