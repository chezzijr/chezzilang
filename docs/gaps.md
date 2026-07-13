# Chezzi — gap backlog

Catch-all backlog of missing / shallow surface. **Not a commitment** — draw from it when a feature
earns its own milestone. Currently all entries are **stdlib + deps** (first audit 2026-07-07); add
other categories (language, tooling, perf) here as they surface.

## Language / concurrency

### 1. Spawn-callee sendability gate — **RESOLVED at check for spawn callee/arg sites** (Task 2a, 2026-07-10)

Spawned tasks **are** usable today: a nested `fn` or closure works as the direct callee of `spawn f()`
(the task runs it; its captured cells are **deep-copied** to isolate them — see
[`concurrency.md §7`](concurrency.md)), and it may capture anything **sendable**: scalars, `str`,
`List`/`Map`/`Set`/`tuple`/structs of sendables, `Channel`/`Shared`/`RwShared`/`Atomic` handles, a
`std.cancel` `Token`, a `.iter()` cursor, and (read-only) module globals. Verified: a task capturing a
`List` or a `Shared` runs fine.

**Was the gap:** the checker's spawn-sendability gate covered `spawn:` / `parallel:` **block** bodies but
**NOT the free captures of a `spawn f()` callee** (closure or nested fn). A callee capturing a `ref T` /
`Ref[T]` and mutating it checked OK yet **ran and silently isolated the write** (a stale-value soundness
bug), contradicting `concurrency.md §7`.

**Fixed (Task 2a):** the checker now records each closure/nested-fn value's non-sendable **local**
captures at its decl site (keyed by binding, using the same `free_names_*` over-approximation the runtime
uses to build captures) and, at a `spawn <name>()` **callee** or `spawn f(<name>)` **arg** site, emits
the verbatim block-form error per captured non-sendable local. A captured **`ref`** is now a clean
compile error at both the callee and the arg site, consistent with the block form. A **module-global**
`ref` is a read-only global (scope-0 exclusion), **not** a capture — never gated. Paired with the
permissive `sendable(Func)` flip (#2), closures-as-data type-check while a captured `ref` is rejected.
An **indirectly**-crossing ref-capture (inside a struct field / `Channel[fn]` value) slips this
check-site gate but is caught by the Task-2b runtime backstop (#2) — no silent `ref` path remains.

### 2. Closures as data — **RESOLVED: RUNTIME (B3.3) + checker gate (Task 2a) + indirect ref-capture runtime backstop (Task 2b, 2026-07-11) all landed**

**Runtime (DONE):** the airlock lowers a closure/bare-`fn` **by value** everywhere — its `proto`
(immutable → shared) + its captures deep-copied recursively into fresh per-task cells + its home-module
index, never a by-reference heap handle — on **both** engines identically (`WireValue::Closure`/
`WireValue::Func`, kept distinct so `str` still renders `<fn NAME>` vs `<closure>`). So a `spawn f()`
callee whose captured environment contains a **nested** closure/`fn` (or is itself a bare `fn`) now runs
cleanly instead of faulting at the airlock.

**Checker (DONE — Task 2a):** `sendable(Ty::Func)` is now **permissive** (a closure crosses by value),
so a **`Channel`/`Shared` element type** of `fn(...)->...` type-checks (`Channel[fn(int)->int]` is
accepted; `channel_of_closures` and a factory closure sent over a channel both run). The per-closure
capture check moved to the airlock **sites** (#1). `ref T`/`Ref[T]` stays non-sendable regardless (use
`Shared[T]`/`Atomic`/`Channel` for cross-task shared mutation).

**Runtime backstop (DONE — Task 2b):** the bare `fn` type cannot carry its captures, so a closure
whose captures include a `ref`/`Ref` that reaches the airlock **indirectly** — inside a struct field
(`Channel[Holder]` where `Holder` has a `fn` field), or through a `Channel[fn]` value — type-checks and
used to **silently deep-copy** the ref (the write vanished). The airlock's two closure-serialization
arms (`to_wire_depth` for `Channel.send`/spawn args, `to_snap_depth` for the M:N snapshot) now scan a
crossing closure's **entire capture graph** (top-level or nested inside a captured
`List`/`Tuple`/`Map`/`Set`/struct/enum/newtype/`Cell`/nested closure), and a `Ref` anywhere in it
raises the **recoverable** runtime error `cannot send a non-sendable ref/Ref captured by a closure
across tasks — use Shared/Atomic/Channel` — **byte-identical on both engines**. Scoped to the closure
arms ONLY: a **module-global** `ref` crosses via the module-globals snapshot (not a closure capture), so
it is never scanned and continues to deep-copy. Together with the Task-2a checker gate, **no silent
`ref` path remains**.

### 3. `Executor.submit` coop-vs-M:N capture-sharing divergence — **RESOLVED (2026-07-11)**

**Was the gap (B3.3 follow-up):** on the cooperative engine `Executor.submit` queued the submitted
closure's own heap `Handle` (captures **shared by reference**, same heap, bypassing `to_wire`), while
`--parallel` wired it **by value** (`WireValue::Closure`). This broke the sacred serial==M:N invariant:
a submitted closure capturing a non-sendable `ref`/`Ref` (directly or via a nested closure) or a live
generator ran silently on serial but faulted on M:N, and a submitted closure mutating a captured
collection observed the mutation on serial but was isolated on M:N (a silent value divergence). The
by-handle branch had been kept deliberately to mirror the tree-walk `interp` oracle.

**Fixed:** `src/vm/netio.rs` now routes **both** engines through `wire_callable` → `to_wire`, exactly
like plain `spawn`. The submitted closure crosses **by value** on the cooperative engine too — captures
deep-copied + isolated at submit time, and the ref/Ref + generator airlock enforcement runs — so serial
and M:N behave identically for every submitted closure. The `interp` oracle was removed, so the by-handle
preservation was pure divergence and is retired. The submit-time generator reach-gate and the drain-time
re-gate (`gate_executor_queue`) are unchanged (reachability is proto-based over the shared `Arc<Program>`,
so switching the queued kind `Handle`→`Closure` leaves verdicts unchanged). Tests:
`executor_submit_{ref,generator}_capturing_closure_faults_both_engines`,
`executor_submit_mutating_closure_isolated_parity`, `executor_submit_sendable_closure_runs_parity`
(`src/vm/parity_tests.rs`), and the rewritten `executor_cooperative_submit_isolates_captures_by_value`.

## Stdlib

Coverage today is *broad* (math, fs, os, time,
datetime, process, rand, regex, request, net, ffi, encoding, crypto, uuid, json, collections, iter,
cmp, string, path, ref, cancel, concurrency); the gaps below are **depth / ergonomics**, not missing
domains. Canonical surface: [`docs/stdlib.md`](stdlib.md).

Discipline reminder (from `CLAUDE.md`): new builtin types/ctors/fns go in their owning `std.*` module
(import-gated), NOT the global reserved namespace. Each item here is its own milestone with a
failing-then-green test + two-engine (serial + M:N) runtime verify.

## Ranked by hit-rate (most-used script surface first)

### 1. String formatting — **highest payoff, most "Python-feel"**
- **Number format-spec in interpolation.** `"{3.14159}"` prints all digits — no `{x:.2f}`, no width/
  fill/align, no thousands-sep, no `int`→hex/oct/bin. This is the single biggest ergonomic gap. Touches
  the lexer/interpolation path + a format-spec mini-grammar. (Python `f"{x:.2f}"` / `format()` has no equal.)
- `str.pad_right` / `center` (only `pad_left` exists today). No `ljust`/`rjust`/`zfill`.
- `str.capitalize` / `title` / `swapcase`. No `rsplit`, no `split` with a limit, no split-on-whitespace-run.
- `str.find(sub, from_index)` (only `index_of` from 0).

### 2. List / iter ergonomics — many small additive holes
- `List.min` / `max` / `min_by` / `max_by`; `iter.min` / `max` (neither exists — only `cmp.min/max` of two).
- `List.first` / `last`; non-mutating `reversed()` (only in-place `reverse`); `insert(i,x)` / `remove_at(i)`.
- `unique` / `dedup`, `chunk(n)` / `windows(n)`, `group_by`, `partition`, `flat_map`, `take_while` /
  `drop_while`, `count(pred)`, `position(pred)`.
- Map: `get_or(k, default)` / `setdefault`, `items()`, `map_values`, `filter`. Set: `is_subset` /
  `is_superset` / `is_disjoint`.

### 3. Lazy iterators (itertools) — builds on generator inference (shipped 2026-07-07)
- No lazy adapters: `count` / `cycle` / `repeat` / `chain` / `islice` / lazy `map`/`filter`/`take` as
  `Iterator[T]`. `std.iter` is all-eager `List`. Natural follow-up now that generators infer their
  element type (`-> Iterator[T]` optional).

### 4. IO / files
- **Interactive CLI — SHIPPED** (see *Interactive CLI* below): `chezzi run` streams stdout, `io.flush()`
  and `io.input(prompt)` exist, and a prompt appears before its blocking read.
- Read-all-stdin; char read.
- Files are **whole-string ≤64 MB only** — no file handles, no line-streaming, no `read_bytes` /
  `write_bytes` (binary). New native userdata type = larger change.

### 5. Numbers / math
- `divmod`, `gcd`, `lcm`, `sign`, `trunc`, `hypot`, `cbrt`. `math.inf` / `math.nan` constants (only
  `pi`/`e` today). Int-from-base (parse a hex/bin string). `factorial` / `comb` / `perm`.

### 6. OS / system
- os: `setenv`, `chdir`, `getpid`, `platform` / `os_name`, `hostname`, `environ()` (all vars),
  `home_dir` / `temp_dir`. No signal handling / `atexit`.
- fs: recursive `walk`, `remove_dir_all` (intentionally omitted today — see `stdlib.md §std.fs`),
  metadata (mtime / permissions / size-struct), temp-file creation.

### 7. Crypto / encoding
- crypto: only `sha256` / `md5`. Missing `sha1` / `sha512`, **`hmac`**, secure-random-bytes / token,
  password hashing (bcrypt/argon2). All hand-rolled zero-dep today, so each is real work.
- encoding: no gzip / zlib, no CSV. No arbitrary-**bytes** round-trip (documented native-seam limit —
  needs a bytes-arg/bytes-return seam expansion).

### 8. Net
- TCP (`std.net`) + HTTP-client (`std.request`) only. No UDP, no HTTP **server**, no DNS-resolve
  exposed, no raw TLS socket (`request` does HTTPS internally via ureq).

## Type-system / construction (adjacent, tracked in `docs/future.md §15`)
- **Definable conversion constructors already exist** as named **static factory methods** (`fn
  Type.from_x(...) -> Type`, `Type.origin()`) — the Rust `T::from` / `T::new` idiom. No Python
  `__init__`-style overridable primary ctor is planned: `Type(...)` stays "set the fields, positionally"
  by design (`spec.md`: conversion is always visible).
- **`Convert[S]` protocol** (bound-only, partial — Phases 0–1 landed, paused) is the principled
  generalization for generic-over-conversion (`[T: Convert[S]]`). Value-position conversion + generic
  construction over the bound are deferred pending demand.
- **`FromIterable` / `Collect`** (not started): let a *user* collection plug into the `List(xs)`-style
  iterable-conversion surface so `MyColl(xs)` works like `List(xs)`. The one genuine "special ctor" gap —
  worth it only when a user collection type needs it.

## Interactive CLI — SHIPPED (the CLI streams; the buffered sink is a test harness)

**Landed.** `chezzi run` now writes each `print` straight to the process's real stdout as it happens.
A prompt appears before its `read_line`, a long-running program prints incrementally, a killed/hung
program retains what it already produced, and a spawned task's log is visible before its nursery joins
(which for a server is never). `std.io` gained `flush()` and `input(prompt)`.

**How the parity oracle survives.** The stdout sink is selected by `HostConfig::stream` (default
`false`): the lib helpers (`run_capture`/`run_file`/… and every golden + parity test) keep the BUFFERED
sink — per-task buffers, task-order flush at join, byte-identical serial-VM == M:N-VM. Only
`src/main.rs`'s `chezzi run` sets `stream = true`, and in that mode the per-task buffers simply stay
empty (the whole buffer/flush machinery degenerates to a no-op with zero scheduler edits).

**The design previously prescribed here — "stream while one task is live, buffer inside a nursery,
flush at join" — is REJECTED.** A server's nursery never joins, so its task logs would buffer for the
life of the process: the exact programs that need live logs are the ones it excludes. The deeper point
is that the task-order flush was never a *user* guarantee: the "order" is task-completion order, a
scheduler detail no correct program can lean on. Python, Go and Rust all interleave concurrent prints
nondeterministically and line-atomically, and nobody minds. A concurrent program that wants ordered
output joins and prints the collected results itself.

**The user-facing contract** (also in `stdlib.md §std.io`): one `print(...)` = one locked write →
**line-atomic** (two tasks can never garble a line; `end=""` fragments *can* interleave mid-line, like
Python); cross-task print order is **nondeterministic** on both engines; stdout and stderr are
separately locked, so a task's `print` and `eprint` may reorder relative to each other.

## Audited residuals — pre-JIT hunt wave 5 (2026-07-13)

Everything below was **found, reproduced on both engines, and deliberately NOT fixed** in the wave-5
sweep (13 bugs fixed, main `0741a0b`). Each is either an accepted design consequence, a
documented-but-unusable surface, or a safe over-rejection. Recorded so they are decisions, not
surprises — **re-read this before the JIT freeze**, since a JIT bakes in whatever is true at freeze time.

### 3. Three over-rejections introduced by the Go-model int→float fix
The wave-5 widening fix (untyped **constant** adapts; a typed int **value** never does) rejects three
constructs that are *not* unsound — it errs safe, but it errs:
- an aliased-collection annotation,
- a generic-erased method param,
- a fn-typed-field call.

All three **reject valid code rather than accept invalid code**, and have **zero in-repo users**. Upgrade
path recorded in the test doc-comments. Revisit only if a real program hits one.

### 4. A module bind shadows a same-named USER ctor — DIAGNOSED, alias is the cure (downgraded)
The wave-5 reserved-module-bind gate (`module name 'int' is reserved (builtin) — alias it: …`) covers
the **34 reserved/builtin** names. It does **not** cover a *user* `struct`/`enum` ctor: a module named
`Point` still wins over a user `struct Point` in expression position (same root cause as the fixed
`import std.str` bug — the bind lands in the VALUE namespace).

**But the blast radius is far smaller than first recorded, and this is now a closed decision.** Unlike a
reserved name — which the module bind *silently destroyed* — a shadowed user ctor is a **hard type error
at the call**, so no program can run wrong; and `import lib.Point as pt` is the cure, which is exactly
what Python does. That is normal shadowing with a diagnostic Python doesn't even give you. The only real
defect was the *message*: the bare `module Point is not callable` never said where your ctor went. Fixed
— the not-callable arm now names the collision (`module bind 'Point' shadows the same-named type
'Point' — alias the import: …`); test `module_bind_shadowing_user_type_names_the_collision`.

A separate **module namespace** (module names legal only in field position) remains the principled fix
and would remove the collision entirely, but it is a resolver change and buys only the loss of an alias
keystroke. Not planned.

### 5. Never-hunted surfaces (the two biggest remaining pre-JIT risks)
Five hunt waves have now swept the typed feature surface, the stdlib, concurrency, and the front-end.
**Two surfaces have never been audited at all**, and they are the memory-fragile ones:
- **GC + `unsafe` under Miri / ASan / TSan** — Tier-1 lever #3 in [`bug-discovery.md`](bug-discovery.md),
  still **unbuilt**. The GC and the OS-thread engine are the most `unsafe`-dense code in the repo.
- **FFI** — zero adversarial coverage. Precedent exists: a libffi `Cif` heap-pin bug already caused a
  **SIGSEGV** (FFI UB is layout-dependent, so it is invisible to the value-level oracles).

Neither is reachable by the panic-fuzzer, the CPython differential, the DSA judge, or two-engine parity
— all four are *value*-level oracles and cannot see UB or a data race. **This is where I would look next
before freezing.**

## Dependency versions (as of 2026-07-07)
All four are **major (semver-incompatible)** bumps — cargo shows them but won't auto-take. `cargo audit`
(2026-07-07, 152 deps) = **0 vulnerabilities, 0 warnings** → no security driver; do NOT bump
speculatively during the perf milestone.
- **libffi** 3→5 — **do not** bump speculatively (FFI UB is layout-dependent; the Cif heap-pin caused a
  SIGSEGV before). Highest risk, ~zero payoff.
- **ureq** 2→3 — a real API rewrite of `std.request`; do as its own task when 2.x nears EOL, with
  request tests + `--parallel` verify.
- **socket2** 0.5→0.6, **libloading** 0.8→0.9 — skip until a needed feature forces it.
