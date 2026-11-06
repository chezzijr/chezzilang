# Chezzi: open design ideas (not scheduled)

> **Status:** open ideas only. Nothing here is committed work.
> [`PROGRESS.md`](../../PROGRESS.md) is the source of truth for what is scheduled and done.
> The sections that shipped (`defer`, concurrency, the oracle-layer rebuild, eager `Executor` and
> `spawn`, the scripting features, witness-passing generics, the M20 test runner, the native-registry
> `Kind`, and the landed M19 perf levers) are kept verbatim in
> [`history/designs/future-shipped.md`](../history/designs/future-shipped.md) (§1-§3) and
> [`history/designs/future-shipped-2.md`](../history/designs/future-shipped-2.md) (§3a1-§4).
> Section numbers below match the old `future.md` numbers, so old `future.md §N` references still find
> their topic.

| § | topic | state |
|---|---|---|
| [2b](#2b-oracle-layer-the-go-paired-programs-differential-not-built) | Go paired-programs differential | not built |
| [2d](#2d-deadlock-detection-from-quiescence-counting-to-a-wait-for-graph-steps-1-4-open) | wait-for-graph deadlock detection | step 0 shipped; steps 1-4 open |
| [3](#3-language-and-tooling-gaps) | REPL, formatter, closed ideas | open / closed |
| [3b](#3b-test-system-follow-ups) | subtests, skip/xfail, timeout watchdog | open |
| [4](#4-optimizations-open-levers) | perf levers not yet landed | open |
| (shared heap) | Go shared heap: [design](go-shared-heap.md) | done (one heap per run, tasks share values by handle); merged to `main` 2026-10-31 |

---

## 2b. Oracle layer: the Go paired-programs differential (not built)

Chezzi has one engine, so every oracle compares it against a reference or a literal golden. Four oracle kinds exist and one is missing:

| bug class | oracle | state |
|---|---|---|
| sequential wrong result | CPython differential (`src/difftest/`, `tests/difftest.rs`, `src/bin/difffuzz`) | built |
| accidental schedule divergence | two worker counts over `tests/chz` (`tests/chezzi_threads_cli.rs`) | built |
| scheduler races, lost wakeups | seeded M:N (`CHEZZI_SCHED_SEED`, `src/bin/schedfuzz`) | built; limits in [`bug-discovery.md`](../contributing/bug-discovery.md) |
| task sharing and nursery semantics | hand-written known-answer tests (Go goldens in `tests/chz/spec/*_handle_test.chz` headers) | built |
| channel / `select` semantics | **Go paired programs** | **not built** |

**The open item.** Write each Go twin so that its outcome is deterministic under a nondeterministic
schedule. Sort the output lines where only the order can vary. Go's channels, `select`, close and
backpressure map almost 1:1 to `Channel[T]` and `wait:`. Use this oracle only in that range: Go has
no equivalent for structured nurseries. Nothing in the repo generates or runs Go
programs today.

**Rule for the new oracle: compare raw bytes.** `String::from_utf8_lossy` is not injective (`ff` and
`fe` both decode to U+FFFD). A decoded compare can report agreement for two runs that wrote different
bytes to fd 1. Chezzi (`io.stdout().write_bytes`) and Go (`os.Stdout.Write`) can both emit non-UTF-8.
Capture `Vec<u8>`. Use the decoded text for display and text heuristics only, never for a verdict.

The seeded oracle does not replay byte-for-byte at T>=2, and at T=1 some wait kinds replay only at a
measured rate. The open rows are in [`gaps.md`](../contributing/gaps.md) (W15-10).

---

## 2d. Deadlock detection: from quiescence counting to a wait-for graph (steps 1-4 open)

### What exists

| layer | where | question it answers | scope |
|---|---|---|---|
| per-nursery quiescence | `MnSched::is_deadlocked` (`src/vm/mod.rs`) | can anything in this nursery still move? | one nursery |
| process-wide quiescence (step 0, shipped) | `src/vm/quiesce.rs` | is every counted party registered as waiting, with no wait satisfiable? | `main` and each eager `Executor` job |
| native-callback arm | `src/vm/netio.rs` | none: it faults, because a callback cannot block | a blocked `recv`/`send`/`wait:` inside a native callback |

Step 0 counts `live = 1 + Σ ExecutorCore::outstanding`. A party registers a `PartyWait` while it is
blocked. `Vm::join_eager_jobs` registers `PartyWait::Join`, so `main` inside `shutdown()` is visible.
The design record and the incident history are in
[`future-shipped.md`](../history/designs/future-shipped.md) §2c and §2d.

### The proposal: a wait-for graph

Give every blocked party an outgoing "waits-for" edge and search for a deadlock. A channel is not an
owned resource: a blocked `recv` waits for whoever sends next, which is any party that can reach the
channel. So an edge goes from one party to a set of possible senders. The graph is an **AND-OR
graph**, and a deadlock is a **knot**: a set S from which every outgoing option leads back into S.
Knot detection is polynomial. A plain cycle search gives wrong answers here.

**Nodes** (include non-fibers, or the detector misses the `shutdown()` case):

- a fiber parked on `recv`, `send` or `wait:`;
- a worker demoted in place (`demote_recv_block`): blocked, not parked;
- a joiner: the thread inside `Vm::join_eager_jobs` or a nursery join.

**Edges:**

| blocked on | edges to |
|---|---|
| empty `recv` on `ch` | every party that could `send` on `ch` |
| full `send` on `ch` | every party that could `recv` on `ch` |
| `wait:` over N arms | OR-edges, one per arm |
| a join (`shutdown()`, nursery barrier) | every outstanding job or task it waits for |

**Approximate the sender set in the safe direction.** The exact set is undecidable. Use reachability
(who holds a handle). An over-approximation adds edges, which gives fewer knots, so the detector
misses a deadlock but never invents one. State this in the code.

**Keep every veto `is_deadlocked` has.** Each one encodes a race that cost a real bug: pending
IO/timer/blocking-pool work (`inflight`), a cancel in flight that would wake a demoted fiber, a scope
mid-teardown, and a value that races into a queue the predicate is about to read. Each veto is an
edge to the outside world, which is an escape from the knot.

**Cost control.** Run the analysis only when quiescence is suspected (a blocked count changed and
nothing is runnable). Do not run it on every poll tick.

### What the ancestors do

| language | behaviour |
|---|---|
| Go | `fatal error: all goroutines are asleep - deadlock!`; counts, builds no graph; total quiescence only; a goroutine in a syscall, netpoll or `time.Sleep` suppresses it |
| Python | no detector; user-side timeouts (`q.get(timeout=…)`) |
| Java | no auto-abort; tooling (`ThreadMXBean.findDeadlockedThreads()`, jstack) |
| Rust | none in std; `parking_lot` has an opt-in lock-cycle detector |
| Erlang/BEAM | none; `receive … after Timeout` |

`is_deadlocked` is Go's rule, scoped per nursery. The wait-for graph adds **partial** deadlock: a
subset stuck while the rest of the program runs. Go cannot report that.

### Lessons from step 0 (apply them to steps 1-4)

- **The verdict is ONE observation.** Hold the party lock across the channel reads. A first cut read
  the channels after it released the lock and reported two parties on empty channels that were never
  empty at the same instant.
- **Do not register a party across its own attempt.** `pop()` and un-registration are not one atomic
  step. Scope the registration to the wait, never to the retry.
- **Ask "is this wait satisfiable?", not "did nothing move recently?".** The second question faulted a
  healthy cap-1 pipeline 6 of 40 runs.

### Remaining steps

1. **`Arc::strong_count` sole-handle rule: not sound since M3.1/M3.2.** A strong count counts heap
   slots, not holders: two tasks that share one `Obj::Channel` slot (a module global, a value sent
   by handle, a box's value) count once, so a count of one proves nothing (the M3.2 leaf proof
   was deleted for this bug). A sound version needs a reachability walk from every non-blocked
   party's roots.
2. **Unify the blocked-party registry with the scheduler's parties** (nursery fibers and demoted
   workers, which step 0 left out). Overlaps
   [`cross-nursery-flat-scheduler.md`](../history/designs/cross-nursery-flat-scheduler.md).
3. **AND-OR knot detection** over that registry, with every veto kept, run only on suspicion. This
   adds partial-deadlock reports only.
4. **Retire the `netio.rs` native-callback fault arm**, if a callback can ever block.

**Risk.** A job that sleeps on a timer, blocks on a socket, waits on netpoll or blocking-pool work, or
races a value into a queue is not deadlocked. A false verdict faults a working program; W7-12 shipped
that three times. Before the detector, write the Go/CPython comparison programs and the looping
regression tests. Then put the change through `adversarial-review`. A full green gate caught none of
the three false positives. See [`lessons.md`](../contributing/lessons.md).

---

## 3. Language and tooling gaps

The §3 scripting features all shipped (comprehensions, `Range` slicing, the iterator protocol,
generators, concat/merge, radix literals, `?.`/`??`, tuple `for`, by-reference capture, match guards
and range patterns, `std.os.exit`, stack traces, static protocol requirements, `From[S]`).

**Open:**

- **REPL.** Large value for scripting iteration. No `chezzi repl` subcommand exists.
- **Formatter.** No `chezzi fmt` subcommand exists.

**Closed or declined (one line each; the reasoning is in the history record):**

| idea | verdict |
|---|---|
| `ref T` / `Ref[T]` | removed: scalar aliasing only; use a one-field `struct` in a task, `Shared[T]` across tasks |
| `cast[T](val: Any) -> T?` | no consumer: the owner ruled against leaning on `Any`. If it returns, only `cast[Scalar]`, `cast[str]`, a bare container kind and a named struct/enum are sound; a parameterized target needs runtime type tags |
| `Into` | done (2026-11-06): an instance `into(self) -> T` / `try_into(self) -> T!` per target, picked by `x.into[T]()` (bottom-up inference, so the target is written); no blanket `into` from a `from`; several owners of one conversion pair are allowed (no blanket impl). `Into[T]` as a bound: not yet ([`05-conversions.md`](../reference/spec/05-conversions.md)) |
| `TryFrom` | done (2026-11-05): `try_from(x: S) -> Self!`, the fallible twin of `from`, as a bound `[T: TryFrom[S]]`; a `Self!` `from` cannot meet `From[S]`. `int`/`float` witness `From[bool]` and `TryFrom[str]` ([`05-conversions.md`](../reference/spec/05-conversions.md)) |
| multi-source `From` | done (2026-11-05): one static `from` overload per source type, picked by the argument's type ([`05-conversions.md`](../reference/spec/05-conversions.md)); since 2026-11-06 any parameterized protocol the declaring module sees keys overloads ([`09-protocols.md`](../reference/syntax/09-protocols.md#a-parameterized-protocol-keys-overloads)); free overloading of plain methods stays banned |

---

## 3b. Test system follow-ups

Shipped: FAIL vs ERROR, `--max-heap`, `--timeout`, `-k`/`--filter`, `--show-output`, `-q`/`-v`,
`--errors=json`, `--color`, `--fail-fast`. The user surface is in `chezzi test --help` and the
reference docs.

**Open:**

1. **Table-driven subtests** (Go `t.Run`, Rust `rstest`, pytest `parametrize`). Today a
   `for case in cases:` loop in a `test fn` stops at the first bad case with no per-case verdict.
   Design open: a `subtest "name":` block, or a `cases`-driven helper.
2. **skip / xfail** (Go `t.Skip`, pytest `skip`/`xfail`). Needs a skip signal that the runner counts
   separately (`S skipped`).
3. **A watchdog thread for `--timeout`.** The deadline is checked only at loop back-edges and the M:N
   reds checkpoint. A test blocked in a native call (a blocking syscall, `Channel.recv` with no
   traffic) or in loop-free infinite recursion is not caught.
4. **Out of scope today:** k/m/g suffixes for `--max-heap`, and `chezzi run --max-heap` /
   `chezzi run --timeout`.
5. **Fault-path tests in Chezzi.** `r := recover: <expr>` yields `!e`, and
   `assert e.message().contains(...)` tests a fault without Rust. The Rust-only set for the
   `tests/chz/` migration is: GC-stress rooting, checker `rejects`/`ok`, parser/lexer/bytecode
   internals, and concurrency timing. Fault-path tests are a future migration cluster.

---

## 4. Optimizations: open levers

Live numbers are in [`benchmarks.md`](../contributing/benchmarks.md) (reproduce with
`benches/run.chz`). The landed M19 levers and their measured deltas are in
[`future-shipped-2.md`](../history/designs/future-shipped-2.md) §4.

**Discipline for every lever:** a failing-then-green correctness test, the suite green at both worker
counts, a measurement on `benches/run.chz`, and the delta recorded in `benchmarks.md`.

### Big (separate milestones)

| lever | notes |
|---|---|
| **Cranelift method-JIT** | the only path to match or beat CPython 3.14 on compute. Counter-triggered; JIT the hot protos. Only once the language stops moving. Adaptive quickening is the stepping stone |
| **Register VM** | fewer ops, less stack traffic; a VM rewrite. Low ROI: dispatch is near the match floor |
| **Generational / incremental GC** | the GC is stop-the-world full-heap (`next_gc = 2×live`). Cuts pause and rescan cost on allocation-heavy scripts. Low ROI: GC moves no bench |

### Interpreter (smaller)

| lever | state |
|---|---|
| more superinstructions: `GetLocal+GetField`, compare+`AsBool`, the load-store accumulator | candidates |
| general per-op type-guard cache | the only unlanded slice of arithmetic specialization; overlaps superinstructions |
| lazy `span` (load `proto_ref.lines[ip]` only on fault) | unshipped; revisit only if a profile shows it |
| split the `mn.is_some()` reduction-count branch and the cancel check off the no-scheduler back-edge | unshipped; same |
| call-site specialization for `Op::Call` | deferred, no gain measured: fib's residual is frame setup in `finish_frame` |
| float-constant interning | deferred: no measured float regression on the int-heavy set |
| frame pooling | rejected: frames already live in a capacity-reusing `Vec`; no per-call frame allocation |

### Memory layout

| lever | state |
|---|---|
| **Shrink `Obj` below 64 B** | `Module` is boxed (88 → 64 B). Below 64 B needs `MapData`/`SetData` boxed too, but SSO sized strings to fill the inline `Obj`. A trade-off: measure first |
| **HOF borrow-release clone** | `map`/`filter`/`fold` clone the list (`self.heap.get(h).clone()`) to release the heap borrow before `invoke_value`. A `Vm` split (`&mut ExecState` + `&Heap`) removes it. Structural refactor |
| `for`-loop snapshot (`ListClone`) + per-char alloc | blocked by the for-loop's observable snapshot semantics |
| value structs, packed storage | see [`value-structs.md`](value-structs.md) and [`packed-storage.md`](packed-storage.md) |

### Leads

- **Debug-build M:N stall when the pool covers every free core** (from W12-23, see
  [`history/bug-hunts/w12.md`](../history/bug-hunts/w12.md)). The 10k-fiber D3 program on the debug
  CLI finishes in 0.3 s at `CHEZZI_THREADS=8` and 1.0 s at 24, but runs past 120 s at 27/28 (= nproc)
  while other cargo builds share the box. The release binary takes 0.17 s at 28 beside 20 CPU hogs.
  Under a 4-core `CPUQuota` with 28 hogs the debug CLI did not stall (11.6-13.6 s), so the trigger is
  whole-box contention, not the quota. The probable cause is a wait that spins instead of parking
  (not measured). The test moved to a fixed pool of 8 in its own process; the engine is unchanged.
