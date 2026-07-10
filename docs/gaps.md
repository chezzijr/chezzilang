# Chezzi — gap backlog

Catch-all backlog of missing / shallow surface. **Not a commitment** — draw from it when a feature
earns its own milestone. Currently all entries are **stdlib + deps** (first audit 2026-07-07); add
other categories (language, tooling, perf) here as they surface.

## Language / concurrency

### 1. Spawn-callee sendability gate is incomplete — `spawn f()` callee captures are checked at RUNTIME, not compile time (soundness)

Spawned tasks **are** usable today: a nested `fn` or closure works as the direct callee of `spawn f()`
(the task runs it; its captured cells are **deep-copied** to isolate them — see
[`concurrency.md §7`](concurrency.md)), and it may capture anything **sendable**: scalars, `str`,
`List`/`Map`/`Set`/`tuple`/structs of sendables, `Channel`/`Shared`/`RwShared`/`Atomic` handles, a
`std.cancel` `Token`, a `.iter()` cursor, and (read-only) module globals. Verified: a task capturing a
`List` or a `Shared` runs fine.

The **gap**: the checker's spawn-sendability gate covers `spawn:` / `parallel:` **block** bodies but
**NOT the free captures of a `spawn f()` callee** (closure or nested fn). So when such a callee
captures a **non-sendable** binding, the block form rejects it at **check** time but the callee form
is not caught until run time — or worse, not at all:

- callee captures a **closure-as-data** (another closure value) → as of the B3.3 runtime (see #2) this
  now **crosses by value and runs** on both engines (was previously a runtime airlock fault). So this is
  no longer a soundness *hole* — the runtime copy is correct — but the checker asymmetry with the
  `spawn:` block form (which still rejects it at `check`) remains a consistency wart to reconcile.
- callee captures a **`ref T` / `Ref[T]`** and mutates it → `check` OK, **runs and silently isolates
  the write** (prints the stale value), directly contradicting `concurrency.md §7` ("capturing/passing
  a `ref` across the airlock is a **compile error, not a silent copy**"). The `spawn:` block form
  rejects it at `check` (`2 type errors`).

Exposed by the first-class-nested-fn work (2026-07-10, merge `f8c3c60`) — nested fns made `spawn f()`
callees common. **Fix direction:** run the same check-time captured-non-sendable analysis the `spawn:`
block form uses over a spawned callee closure/nested-fn's **free** captures (the free set is already
computed at each MakeClosure site by the free-variable-capture work, merge `0d40fca`), so a captured
closure/`ref`/generator in a `spawn f()` callee is a **clean compile error**, consistent with blocks.
Checker-only; no new feature. This makes the *existing* limit sound; it does **not** make closures
sendable-as-data.

### 2. Closures as data — **RUNTIME half landed (B3.3); checker gate still pending**

**Runtime (DONE):** the airlock lowers a closure/bare-`fn` **by value** everywhere — its `proto`
(immutable → shared) + its captures deep-copied recursively into fresh per-task cells + its home-module
index, never a by-reference heap handle — on **both** engines identically (`WireValue::Closure`/
`WireValue::Func`, kept distinct so `str` still renders `<fn NAME>` vs `<closure>`). So a `spawn f()`
callee whose captured environment contains a **nested** closure/`fn` (or is itself a bare `fn`) now runs
cleanly instead of faulting at the airlock.

**Checker (STILL PENDING — follow-up):** the type checker still treats a function type as *non-sendable*
for a **`Channel`/`Shared` element type** (`Channel[fn(int)->int]` is rejected at check), so *storing* a
closure in a channel/box is not yet reachable — only the runtime half exists. Lifting that gate (and
proving the by-value copy is safe in the checker) is the remaining work. `ref T`/`Ref[T]` stays
non-sendable regardless (use `Shared[T]`/`Atomic`/`Channel` for cross-task shared mutation) — a separate
checker follow-up handles `ref` safety.

## Stdlib

Coverage today is *broad* (math, fs, os, time,
datetime, process, rand, regex, request, net, ffi, encoding, crypto, uuid, json, collections, iter,
cmp, str, path, ref, cancel, concurrency); the gaps below are **depth / ergonomics**, not missing
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
- `input(prompt)` (print + `read_line`). Read-all-stdin; char read.
- Files are **whole-string ≤64 MB only** — no file handles, no line-streaming, no `read_bytes` /
  `write_bytes` (binary). New native userdata type = larger change.
- `flush`: **likely a permanent non-goal** — the VM buffers per-task stdout and flushes in deterministic
  task-order at join (the serial-VM == M:N-VM parity guarantee). A user-triggered mid-run flush has no
  clean semantics in that model. (`print(..., end="")` already covers newline-less incremental output.)

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

## Dependency versions (as of 2026-07-07)
All four are **major (semver-incompatible)** bumps — cargo shows them but won't auto-take. `cargo audit`
(2026-07-07, 152 deps) = **0 vulnerabilities, 0 warnings** → no security driver; do NOT bump
speculatively during the perf milestone.
- **libffi** 3→5 — **do not** bump speculatively (FFI UB is layout-dependent; the Cif heap-pin caused a
  SIGSEGV before). Highest risk, ~zero payoff.
- **ureq** 2→3 — a real API rewrite of `std.request`; do as its own task when 2.x nears EOL, with
  request tests + `--parallel` verify.
- **socket2** 0.5→0.6, **libloading** 0.8→0.9 — skip until a needed feature forces it.
