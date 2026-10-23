## 7. Sendability

**The model — spawning a task copies its environment (fork-like).** A `spawn`ed task does not share the
parent's heap. It receives its **own isolated copy** of everything it captures — captured locals are
deep-copied, module globals are snapshot-copied per task (fresh at its `spawn`,
[§2](01-model.md#2-why-shared-nothing-and-why-not-the-go-memory-model)) — and a **closure** or generator
reads the module globals of the task that RUNS it, never the sender's. Two deliberate differences from
a real `fork`:

1. It copies only the **reachable captured environment**, not the whole heap.
2. **Explicit concurrency handles cross by SHARED reference, not by copy** — `Channel`, `Shared`,
   `RwShared`, `Atomic`, `Executor`, and the socket/reader/writer handles carry their one underlying
   core across, so all tasks reach the *same* mailbox/box/queue/fd. These handles are the *only* way
   tasks share mutable state; everything else is copied. A plain captured value mutated in a task
   changes only that task's copy, and (D4) that write faults there, except the ceilings below.

> **One-sentence mental model:** a spawned task gets its **own copies** of what it captures
> (read-only), holds **shared handles** to channels and `Shared` boxes, and talks **only** through
> those — which move/copy values between isolated heaps.

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

Reassigning — or mutating in place (`.push`/`.insert`/`m[k]=v`/`s.field=x`) — a captured **local** or a
**module global** inside a task is a recoverable runtime fault:

```
'<name>' is this task's copy: a write to it would be lost at the join; share it through Shared/Channel, or make a task-local copy with .copy()
```

The write would never be seen by the parent, by sibling tasks, or by whoever later receives a closure
over that global. To produce output visible to the parent, use a `Channel` or a `Shared`. Reads are
always fine. A received closure or generator READS the module globals of the task that RUNS it; it may
NOT write one — a module-global slot store faults whenever the running task holds a module-global
snapshot. The checker rejects the provable subset at compile time
([§2](01-model.md#the-race-you-cant-write)). Go and CPython each have exactly one global object, so the
same write is always visible there; the divergence is deliberate. Decision record:
[`adr-d4-airlock.md`](../../design/adr-d4-airlock.md).

**Write sites the mark is checked at:**

- a struct field store (`p.f = v`);
- an index store on a `List`/`Map`/`bytearray` (`xs[i] = v`). A struct's own `set_index` is NOT
  checked — whatever it writes through is checked at its own call sites, so writing through a `Shared`
  field stays silent as real sharing;
- a captured-local cell store, a module-global slot store;
- `+=`-style in-place arithmetic on a container;
- every mutating native call: List `push pop reverse sort sort_by sort_by_key extend insert remove`,
  Map `insert remove update`, Set `insert remove`, bytearray `push pop extend`.

Reads never check the mark. A write through a `Shared`/`RwShared`/`Atomic*`/`Channel`/`Socket`/
`Listener` handle is real sharing and is never marked, so it is never checked either.

**Two shapes still lose the write silently** (measured on the release binary):

1. **Closure capture vs. a sibling reference — order-dependent, one shared memo per send.** A tuple
   `(xs, f)`, `f := fn() -> None: xs.push(2)`, sent in one crossing over a `Channel` to a different
   spawned task, both writes done under `recover:`:
   - `ch.send((xs, f))` (`xs` first): `xf: f() ok` then `xf: xs.push ok` — NEITHER write faults; the
     push is silently lost.
   - `ch.send((f, xs))` (`f` first): `fx: f() FAULT this value is this task's copy: a write to it
     would be lost at the join; share it through Shared/Channel, or make a task-local copy with
     .copy()` then `fx: xs.push FAULT` — BOTH writes fault.

   Whichever element the walk visits first decides whether the closure's captures are rebuilt as
   marked copies or aliased to an already-visited (unmarked) object.
   `tests/chz/spec/airlock_task_local_fault_test.chz` and `airlock_task_local_silent_test.chz` each
   pin one row for this shape.
2. **Captured iterator/generator cursors.** Advancing a captured `Iterator` cursor and resuming a
   captured generator are not checked. This is the deep-copy semantics, not a lost write: the task's
   copy is independent, and the parent's own `g.next()` after the join returns its own next value.
   What the resumed generator writes inside its frame IS decided: a frame-local value the parent never
   saw is writable at every depth it built itself, and a list the generator yielded, stored or passed
   on faults. An Executor job, a spawn, the module snapshot and a task copy's `Task.get()` /
   `memoize1` read all apply it; see [`adr-d4-airlock.md`](../../design/adr-d4-airlock.md) rule 2.
**A same-task round trip is a crossing** (owner decision 2026-10-22, TICKET-291, reversing the
2026-09-23 ruling). A closure holds its captures by reference only in the heap that built it. Every
rebuild of a closure -- a `Channel` send, a `Shared`/`RwShared` store read back (`get`, `read`, a
concurrent collection's `at`/`get`/`remove`), an `Atomic` load, a `Task` result, a relay task that
stores it again -- rebuilds its captures as copies, so a write through one faults, the task that sent
or stored it included. Two rebuilds prove a capture is the only live copy, so its write runs:
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
(non-closure) value received over a `Channel` or read back from a `Shared`/`RwShared` is never marked:
it belongs to the receiver.

### Closures and functions cross by value

The airlock lowers a closure or bare `fn` **by value** — its `proto` (shared, read-only) + its
captures deep-copied recursively + its home module index. **A closure carries captures only — never
module globals.** A closure or generator, wherever it was created, READS the module globals of the
task that RUNS it, but may not WRITE one. Each task owns a deep copy of every module global (taken at
its `spawn`); a crossing installs nothing into it and replaces no slot, so a receiver's aliases of its
own globals (`a := g`) stay attached.

```
g: List[int] = [1]
fn main():
    c := Channel[fn() -> str](1)
    parallel:
        spawn:
            recover: g.push(2)           # FAULTS (D4): g is the task's own copy
            c.send(fn() -> str: "{g}")
    f := c.recv()
    print("{f()} {g}")                   # [1] [1] — f runs in main, against main's copy
main()
```

Go and CPython have ONE global object, so `g.push(2)` there mutates it in place and both prints see
`[1, 2] [1, 2]`. A module-scope closure shows the same divergence: `n := 1`, a task writes `n = 100`,
sends `fn(x): x * n`, the receiver calls `f(3)` — Go and CPython print `300`; in Chezzi the task
write faults. The migration path is to make the shared value a `Shared`:

```
n := Shared(1)
ch := Channel[fn(int) -> int]()
parallel:
    spawn:
        n.set(100)
        ch.send(fn(x: int) -> int: x * n.get())
f := ch.recv()
print(f(3))                              # 300 — the Shared crosses by handle
```

- A module-level `let` binding is otherwise a late load in-task (matching CPython), so a write to it
  AFTER a closure is created is visible to a later same-task call, including through a same-task
  `Channel.send`/`.recv()` round trip. Top-level `fn`s, imports, `native fn`s and `extern` fns are
  unaffected.
- A closure passed as a spawn ARG after an in-nursery in-place push reads the task's in-nursery view,
  exactly as a `spawn:` block does. A closure's own **captures** cross by value: a captured local reads
  as the sender left it.
- **A spawn-crossed alias of a module global is adopted as the global's own object.**
  `inner := gl[0]` crossing in the SAME closure as `gl` itself — or as a local, a spawn arg, the whole
  global, a struct inside a global `Map`, or a generator — gives the receiver one object, matching
  CPython and Go. Residuals: a node aliased by globals of two DIFFERENT modules, a container on the slow
  snapshot path (holds a closure or handle), and `bytearray`. The receiver's own push to that copy
  faults (D4).
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
  **any** airlock (`spawn:` block, `spawn` arg, `Channel.send`, `Shared`, module-global snapshot) and
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
  `spawn:` block's captures, and **the whole module snapshot** (every module).
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
- **Handle-bearing cell.** A cell whose inner value carries a residual module/native/FFI handle falls
  to the snapshot's slow path, which has no back-reference encoding.

**The depth cap.** The only value cycle that still rejects is one threaded through a live
**generator's parked frame** (below). A genuinely >10000-deep **acyclic** nest hits the depth guard
(`MAX_STRUCTURAL_DEPTH = 10000`, the same bound the display / `==` paths use): a **recoverable**
`maximum structural depth (10000) exceeded (cyclic data structure?)` fault, stamped with the airlock
site and catchable by `recover:`. A deep module global reaches that fault in time linear in the walk.
Large-but-**shallow** data (e.g. a 100k-element list) crosses fine — the counter measures nesting
depth, not element count.

### Generators

A live generator crosses **any** task airlock **as data** (passed/captured into a `spawn`, or stored
in a `Channel`/`Shared`/`RwShared`/`Atomic`) as an **independent deep copy** of its `proto`, backing
closure and parked operand-stack/args, rebuilt fresh on the receiver, so advancing one copy never
affects the other.

- **One generator, one copy per crossing.** A live generator reached TWICE in one crossing (`a := g`
  then `spawn: a.next(); g.next()`, two module globals aliasing one generator, or a generator nested in
  another generator's parked frame passed alongside it) crosses as ONE copy, so `a` and `g` still
  denote one generator in the task (CPython prints `t 2 3` for `g1 := nums(); g2 := g1` read from a
  thread; Chezzi's task copy does too).
- **A suspension inside a `recover:`** crosses too; the recover boundary resumes intact.
- **A module-global generator** crosses by value through the per-task module snapshot, so two tasks
  reaching it each drive their **own** copy (and the parent keeps its own).
- Every parked slot is wired recursively, so a **non-sendable parked slot** rejects at the crossing.

Reject shapes:

| shape | result |
|---|---|
| a cycle that re-enters the generator on the serialize stack | fault `a generator cannot be sent across tasks as part of a reference cycle` |
| a generator crossed **while it is running** (its own body sends a value holding it) | fault `a running generator cannot be sent across tasks` (CPython: `TypeError: cannot pickle 'generator' object`) |
| a >10000-deep acyclic nest held live across a `yield` | the depth-cap fault |
| a suspension with a pending `defer`, or a multi-frame suspension | `... cannot be sent across tasks` (checker-unreachable) |
| a module-global generator that was RUNNING when the task was spawned, or holds a value that cannot cross | an inert placeholder: a task that never drives it runs clean; driving it faults `a module-global generator that was running when this task was spawned has no copy in the task; send its values through a Channel` (or `... holding a value that cannot cross tasks ...`) |

A cycle whose root is a container the generator's frame merely points at
(`xs.push(gen_over_xs)` then `c.send(xs)`) round-trips correctly, the copied frame aliasing the copied
container. A parked recursive local `fn` or a parked self-referential struct/list/map round-trips like
any other capture.
