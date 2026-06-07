# Chezzi — Future Directions (brainstorm, NOT scheduled)

> **Status:** speculative design notes (recorded 2026-06-07). Forward-looking and opinionated.
> Nothing here is committed work — `PROGRESS.md` + `gaps.md` remain the source of truth for what's
> actually scheduled. This doc captures *what would make Chezzi an effective scripting language* and
> *how to make it faster*, with verdicts and rough implementation shape. Promote items into `gaps.md`
> when they're scheduled.

The language **core** is feature-complete (scalars, `list`/`map`/`set`/`tuple`, generic structs +
enums, `Result`/`Option` + `?`, generics + structural protocols, exhaustive `match`, closures/HOF,
modules, GC, two engines, interpolation, pipe, panic recovery via `recover:`, the `Iterator[T]`
protocol bound). What follows is the gap between "complete core" and "language you reach for to write
real scripts."

---

> **Promotion status (2026-06-07):** §1 (`defer`) and the §3 scripting features have been **promoted
> into `gaps.md` → "Open gaps"** as tracked, near-term work. They stay documented here for the design
> rationale; `gaps.md` is now the scheduling source of truth for them. §2 (concurrency) and §4
> (optimizations) remain speculative and live only here.

## 1. `defer` (cleanup on scope exit) — strong fit, recommend → **promoted to `gaps.md`**

Before M11 this was weak: no panic meant nothing to clean up after. **Now there is unwinding** —
the `recover:` boundary, `?` propagation, and runtime faults all unwind. So `defer` earns its keep
by running on **all three exit paths**: normal return, `?` short-circuit, and panic unwind. That is
exactly Go's value proposition.

**Implementation shape**
- Per-frame deferred-call stack, drained LIFO on *every* frame exit including unwind.
  - Interp: drain in the `Flow` / propagating channel path **and** the `recover` snapshot/restore path.
  - VM: drain at `Return` **and** inside the handler-stack unwind (`PushHandler`/`PopHandler` already exist).
- **Arg-evaluation timing:** evaluate `defer` arguments *at the `defer` statement* (Go semantics),
  not at exit. Less surprising; the deferred call closes over already-evaluated values.

**Alternative considered:** Python-style `with` (context-manager protocol `enter`/`exit`). More
Python-feel, but needs a new protocol + an indentation block. `defer` is simpler, adds no protocol,
and composes cleanly with `recover:`. **Recommend `defer`.**

---

## 2. Concurrency + parallelism — the shared-nothing (BEAM) model

**Chosen direction.** A shared-nothing actor model: lightweight tasks, each with its **own heap and
its own GC**, scheduled M:N over a small pool of OS threads, communicating **only** through channels
by **move/copy** — never by shared mutable memory. This delivers **both** concurrency (I/O overlap)
and **parallelism** (multicore) without the cost that sinks the Go model.

**Lineage:** this is the **Erlang/Elixir BEAM** model (shared-nothing processes, per-process heap +
GC, M:N scheduling, message passing) **plus** two borrowings — **Go's first-class `chan[T]`** (BEAM
addresses per-process mailboxes by PID instead) and **Rust's move-on-send** (BEAM only ever copies,
since it's fully immutable; we have mutation, so a move avoids the copy).

### Why not the Go memory model

Go (and Java) is **shared-memory** ("Tier D"): all goroutines share one heap, a **concurrent GC**
(tri-color + write barriers) runs alongside mutators, channels pass **pointers** (memory stays
shared), and "share by communicating" is an unenforced *convention* — the `-race` detector exists
precisely because sharing is allowed and races happen.

Porting that to Chezzi would require:
- **`Rc` → `Arc` across the entire value model** — every container, every reference value. Atomic
  refcount bumps are ~10–30× a normal bump and happen constantly, taxing **single-threaded** code
  forever, whether or not anyone spawns.
- **A thread-safe GC** — the current mark-sweep is one `Heap` owned by one `Vm`, stop-the-world,
  roots = the operand stack. Multi-thread shared heap needs per-thread root scanning, a safepoint
  handshake, and a write barrier for concurrent marking. None of that exists.

That's a runtime rewrite that permanently taxes the common case, plus it hands users the entire
data-race bug class. **Skip it.**

### The cost is sharing, not cores

The key principle: **cost scales with shared mutable memory, not with the number of cores.** Forbid
sharing — make tasks copy/move messages — and parallelism gets cheap, because no two threads ever
touch the same object:

| Tier | Model | Multicore? | `Rc`→`Arc`? | GC change? | Cost |
|------|-------|-----------|-------------|-----------|------|
| A | Cooperative fibers (1 thread) | ❌ | no | +scan suspended fiber stacks | cheap |
| B | Worker processes (`std.process`) | ✅ | no | none | ~free, but heavy IPC |
| C | **Shared-nothing threads** + channels | ✅ | no | none (per-thread heap+GC) | medium |
| D | Shared-memory threads (Go/Java) | ✅ | **yes, everywhere** | concurrent collector | huge |

**Chosen = A + C composed:** µs-cheap fibers (the goroutine *feel* — cheap spawn, no `async`
coloring) scheduled over a small pool of shared-nothing OS threads (real multicore). Because a fiber
may land on any thread, capture/send semantics are **uniform** (always move/copy, never share) —
behavior never depends on where a fiber is scheduled. Never needs atomic `Rc` or a concurrent
collector, because no two threads share an object.

> **Tier C is NOT Python multiprocessing.** Python's `multiprocessing` is heavy because of the
> **process + pickle + OS-pipe** boundary — separate interpreters, serialize through the kernel.
> Tier C stays in **one process**: real threads, each owning a heap region; a channel send is an
> in-process memcpy (or an O(1) move), not an IPC round-trip. The "heavy" part of Python is the
> process boundary, *not* the no-sharing.

### Memory picture — shared heap vs own heaps

Task: 3 workers each compute a value, parent collects.

**Tier D (Go) — one shared heap:**
```
 goroutine 1 ─┐
 goroutine 2 ─┼──►  ONE shared heap
 goroutine 3 ─┘       ┌─────────────────────┐
                      │ results: [ _, _, _ ] │ ◄── all 3 write here directly
                      └─────────────────────┘
       concurrent GC walks ALL threads' stacks while they run (write barriers)
```

**Tier A+C (Chezzi) — own heaps, channel between:**
```
 thread 1: [own heap] ──send──┐
 thread 2: [own heap] ──send──┼──►  main thread [own heap]
 thread 3: [own heap] ──send──┘        ┌──────────────────┐
                                       │ results: [...]    │
  each heap has its OWN GC             └──────────────────┘
  (no handshake, no barrier)     values MOVE across the channel, never shared
```

### The race you can't write

```
// Go — compiles, runs, WRONG: 1000 goroutines stomp one int → torn writes, data race
counter := 0
for i := 0; i < 1000; i++ { go func() { counter++ }() }
```
```
# Chezzi A+C — the bug is unrepresentable
counter := 0
spawn fn(): counter++     # ✗ checker error: captures mutable `counter`; not sendable.
                          #    captured bindings are read-only copies — use a chan.
```

Trade in one line: **D lets you share → fast sends, but races are your problem (caught at runtime,
if lucky). A+C forbids sharing → the race literally cannot be expressed.** Same price Rust `move`
closures and Erlang processes pay; for a scripting language it's a *good* trade — the #1 concurrency
bug class doesn't exist.

**Analogy.** D = one whiteboard everyone writes on at once (no copying = fast, but smudges = races;
needs a janitor cleaning *while* people write = concurrent GC). A+C = everyone has their own notebook;
to share you photocopy a page (copy) or tear it out and hand it over so your hand is empty (move);
no collisions, each cleans their own notebook (independent single-thread GC).

### Channels = a mailbox *outside* every heap

A `chan[T]` is **not** an object in any task's GC heap. It's a separate runtime structure — a
**mailbox** with its own lock + queue — that sits outside both heaps. What each task *holds* is a
lightweight **handle** (an `Arc<ChannelControl>` / id), not a GC object:

```
   task 1 heap ─┐                          ┌─ task 2 heap
                │                          │
                └──holds handle──►  ┌───────────────┐  ◄──holds handle──┘
                                    │  CHANNEL       │
                                    │  [queue]       │  ← own mutex/condvar
                                    │  lock + cond   │     (not anyone's GC)
                                    └───────────────┘
```

- The handle is the **only** shared thing, and it's cheap (created once, an atomic bump on capture).
- Values flowing through are **moved/copied** at the airlock — never live in two heaps at once.
- So you share a **send/receive capability**, not mutable data. Only the channel control block is
  synchronized (its own lock); `Rc` values and per-heap GC stay untouched. *This* is what keeps A+C
  off Tier D.

```
ch.send(x)   # x moved/copied OUT of sender's heap → channel queue
ch.recv()    # value reconstructed IN the receiver's heap
```

Move-on-send = Go's O(1) send cost without Go's sharing (sender can't touch `x` after — checker
enforces, like a Rust channel). Deep-copy is the fallback when the sender wants to keep its copy.

### Captures: by-copy, read-only inside the task

```
fn worker(id: int, prefix: str, out: chan[str]):
    out.send("{prefix}-{id}")

fn main():
    ch := chan[str]()
    label := "task"
    parallel:                         # nursery — joins all children at dedent
        spawn worker(1, label, ch)    # 1, label COPIED in; ch = shared mailbox handle
        spawn worker(2, label, ch)
    for _ in 0..2:
        print(ch.recv())              # workers' strings move into main's heap here
```

Shared: **only `ch`**. `label` is copied into each worker; results return through `ch`. Captured
bindings are **read-only inside the task** — reassigning one is a compile error — so the copy
semantics are obvious: read captured config freely, but produce output only via channels.

**Sendable** (capturable / channel-passable): scalars; containers + structs whose contents are all
sendable; **channels themselves** (pass a `chan` over a `chan` for reply-channels, like Go). **Not
sendable:** closures (bound to a heap), native handles (file/regex/Level-3 userdata). The checker
gates capture and `send` on sendability, with the fix in the error message.

### Surface syntax (no closure dependency)

Closures are single-expression only, and **stay that way** — the concurrency surface doesn't need
multiline closures. `spawn` is a **statement** in two forms, both sidestepping the closure limit:

```
spawn worker(1, ch)        # form 1: spawn a named call (Go's `go f(x)` — the common, good case)

spawn:                     # form 2: spawn an anonymous indented block (a statement, not an expr)
    x := heavy(1)
    ch.send(x)
```

Wrapped in a **nursery** for structured concurrency:

```
parallel:                  # all spawned children join at the dedent
    spawn worker(1, ch)
    spawn worker(2, ch)
# reaching here ⇒ all children finished. No WaitGroup, no leaks.
```

A fiber can't outlive its `parallel:` block (no goroutine leaks — Go's #1 footgun), child errors
propagate to the block (composes with `recover:` + `defer`). This is *nicer* than Go, not a
compromise. `chan[T]` is the sync primitive; blocking stdlib calls (`std.request`, `std.fs`) become
scheduler yield points so I/O overlaps within a thread.

### One-sentence mental model

> A spawned task gets its **own copies** of what it captures (read-only), holds **shared handles** to
> channels, and talks **only** through those channels — which move/copy values between isolated heaps.

### Implementation cost (honest)

- **Scheduler** — M:N fiber scheduler over an OS-thread pool (work-stealing); stackful fibers
  (cheap spawn, no coloring).
- **GC roots** — must scan every *suspended* fiber's stack, not just the running one (additive to
  the current collector; no rewrite).
- **Channel runtime** — mailbox control block with a lock/condvar (cross-thread) or pure
  scheduler park/unpark (same-thread); move/copy of values across heaps.
- **Checker** — sendability analysis (capture + `send`), read-only captured bindings, the move
  (affine) check so a sent value isn't reused.
- **Both engines** — interp and VM each need the fiber + channel plumbing in lockstep (the project's
  standing parity invariant).

Large but **bounded**, and crucially it leaves the single-threaded fast path (`Rc`, per-heap
stop-the-world GC) **untouched** — the whole point of choosing A+C over D.

---

## 3. Missing features (ranked by leverage for scripting) → **all promoted to `gaps.md`**

1. **Comprehensions** — `[x*2 for x in xs if x>0]` (+ dict/set). A Python-feel language without
   these feels broken. Pure parse-time desugar to loop + push. Cheap, large UX win.
2. **Slicing** — `xs[1:3]`, `s[2:]`, `xs[::-1]`. Scripting essential, fully missing. Lexer has `..`;
   needs `:` inside an index expression.
3. ~~**Iterator protocol + generators (`yield`)**~~ — **DONE + descoped.** The `Iterator[T]`
   parameterized protocol shipped (M13): user structs usable in `for`, generic `[S: Iterator[T], T]`
   bounds, and lazy `map`/`filter`/`take` written as **adapter structs** over it (Rust `std::iter`
   model — `examples/iter_adapters.chz`). `yield`/generators are a **deliberate non-goal**: they would
   need coroutine/continuation support in both engines, and the adapter-struct pattern covers lazy
   streaming without it. (If the §2 fiber scheduler ever lands, `yield` could return as sugar — not
   planned.)
4. **Spread / unpack** — `[*a, *b]`, `{**m}`, `f(*args)`. (No **variadic params** — `fn log(*args)` —
   decided against: pass an explicit `list` instead. Default + named args shipped for functions and
   struct constructors; defaults/named on **methods** still open. See `gaps.md`.)
5. **Hex / binary / octal literals** — `0xFF`, `0b1010`, `0o17`. Bitwise ops shipped but (likely) no
   non-decimal literals — awkward for bit work. Lexer-only.
6. **Optional chaining + null-coalescing** — `x?.field`, `a ?? default` on `Option`. `if/else`
   expression + `?` exist; these cut `Option` boilerplate.
7. **`enumerate` / `zip` builtins** — `for i, x in enumerate(xs)`. Daily-driver scripting.
8. **Mutable closure capture** — currently snapshot-by-value, so closure counters / accumulators
   don't work. Real functional gap. Decide: keep intentional (document loudly) or fix (capture cell).
9. **Match guards + range patterns** — `n if n>0:`, `1..10:`. Roadmap. Guards subsume the rest.
10. **`std.os.exit(code)` + real exit codes** — currently deferred, but scripts *must* signal
    failure. Needs an exit-code channel threaded through both run drivers + the CLI.
11. **String formatting** — width / precision / radix: `"{x:08.2f}"`, `"{n:x}"`. Interpolation
    exists; a format spec does not.
12. **Runtime stack traces** — error + call chain + line numbers. Debuggability is a scripting
    feature.

**Ecosystem (Tier 4, separate track):** REPL (huge for scripting iteration), formatter, `assert` +
built-in test runner, LSP.

---

## 4. Optimizations (ranked effort → payoff)

Current: ~4–6.5× over the tree-walker, near the safe-match-dispatch floor. The two real costs are
**dispatch count** and **name lookup**.

**Cheap — do first:**
- **Peephole + constant folding (compiler)** — fold `2+3`, fold constant interpolation chunks, drop
  dead code. Free wins, no runtime change.
- **Superinstructions** — fuse hot op pairs (`GetLocal+GetField`, `PushConst+Add`,
  `GetLocal+GetLocal+BinOp`). Cuts dispatch count directly — the bottleneck.
- **Inline caching for name lookup** — globals / builtins / struct fields resolve *by name at
  runtime* today. Cache the resolved slot/index on first hit (monomorphic IC). Field access, method
  dispatch, and global reads all benefit.

**Medium:**
- **NaN-boxing the `Value`** — pack into 8 bytes (vs the current ~16-byte enum). Better cache density
  across the whole operand stack. Touches every `Value` site.
- **Specialize arithmetic** — binary ops re-dispatch on type every iteration; type-guard a hot loop
  to a monomorphic int path. Big on numeric loops (the current weak case).
- **Frame pooling** — reuse call frames instead of allocating per call. Helps recursion (`fib`).
- **String interning + cached hash** — intern keys / short strings → pointer-compare equality + free
  map hash. Map hashes are already cached; interning extends it.
- **Reduce string-op allocations** — concat / `split` / `+` build a fresh `Rc<str>` each time; a
  builder / rope helps hot concatenation.

**Big (separate milestones):**
- **Register VM** instead of stack — fewer ops, less stack traffic. Effectively a VM rewrite; only
  if dispatch count is still the wall after superinstructions.
- **Generational / incremental GC** — current is stop-the-world full-heap (`next_gc = 2×live`).
  Generational cuts pause + rescan cost on allocation-heavy scripts.
- **Cranelift AOT/JIT** — already the stretch goal. Near-native, but a whole backend. Only after the
  language stops moving.

**Highest payoff-per-effort:** superinstructions + inline caching + peephole/const-fold. They attack
dispatch count and name lookup — the two actual costs — without touching the value model or the GC.
