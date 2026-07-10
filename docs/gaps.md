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

### 2. Closures as data — **RUNTIME (B3.3) + checker gate (Task 2a) landed; struct-field/opaque storage of a ref-capturing closure deferred to a runtime backstop (Task 2b)**

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

**Still pending (Task 2b — runtime backstop):** the bare `fn` type cannot carry its captures, so a
closure whose captures include a `ref` that reaches the airlock **indirectly** — inside a struct field
(`Channel[Holder]` where `Holder` has a `fn` field), or through a channel/opaque value — is no longer
caught at check (it type-checks). A small **runtime** guard on the channel/opaque/struct-field paths is
the remaining work to fault such a case deterministically on both engines.

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
