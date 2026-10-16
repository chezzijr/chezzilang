# Gaps archive: category sections (language, stdlib, tooling, CLI) (moved verbatim from `docs/history/bug-hunts/gaps-archive.md` lines 4029-4811)

## Language / concurrency

### 1. Spawn-callee sendability gate — **RESOLVED at check for spawn callee/arg sites** (Task 2a, 2026-07-10)

Spawned tasks **are** usable today: a nested `fn` or closure works as the direct callee of `spawn f()`
(the task runs it; its captured cells are **deep-copied** to isolate them — see
[`concurrency.md §7`](../../reference/concurrency/01-model.md)), and it may capture anything **sendable**: scalars, `str`,
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
domains. Canonical surface: [`docs/reference/stdlib.md`](../../reference/stdlib/01-builtins.md).

Discipline reminder (from `CLAUDE.md`): new builtin types/ctors/fns go in their owning `std.*` module
(import-gated), NOT the global reserved namespace. Each item here is its own milestone with a
failing-then-green test + two-engine (serial + M:N) runtime verify.

## Ranked by hit-rate (most-used script surface first)

### 1. String formatting
- ~~Number format-spec in interpolation~~ — **SHIPPED** (`src/fmtspec.rs`, `Op::ToStrFmt`,
  `docs/reference/syntax.md §10`): the full Python mini-language, `{x:.2f}` / fill / align / width / `d f x X b o
  e %`. This entry sat here as "the single biggest ergonomic gap" long after it landed — the audit's
  cautionary tale. It also largely **obsoletes** the next bullet (`"{s:^10}"` is `center`).
- `str.pad_right` / `center` / `ljust` / `rjust` / `zfill` — now only *method spellings* of what format
  specs already do. Downgraded: alias sugar, not a gap.
- ~~`str.capitalize` / `title` / `swapcase`. No `rsplit`, no `split` with a limit, no split-on-whitespace-run.~~
  **SHIPPED** as `std.string` free fns (`std/string.chz`): `capitalize` / `title` / `swapcase` /
  `rsplit` / `split(s, sep, maxsplit=-1)` / `split_whitespace` — Python semantics, free-fn-only.
- ~~`str.find(sub, from_index)` (only `index_of` from 0).~~ **SHIPPED** as `std.string.find(s, sub,
  from_index)`; `index_of` is now `find(s, sub, 0)`.

### 2. List / iter ergonomics — many small additive holes
- ~~`List.min` / `max` / `min_by` / `max_by`~~ **SHIPPED** (methods on `List[T]`, `where T: Comparable`;
  `min_by`/`max_by` take a `fn(T) -> K` key; all four return `Option[T]` — empty is `None`, not a fault,
  since 2026-08-17). `iter.min` / `max` still open (only `cmp.min/max` of two) — separate wave.
- ~~`List.first` / `last`; non-mutating `reversed()` (only in-place `reverse`); `insert(i,x)` /
  `remove_at(i)`~~ **SHIPPED** (`first`/`last` → `Option[T]`; `reversed()` returns a NEW list;
  `insert` Python-clamps; `remove_at` returns the element, faults OOB).
- ~~`unique` / `dedup`, `chunk(n)` / `windows(n)`, `take_while` / `drop_while`, `count(pred)`,
  `position(pred)`~~ **SHIPPED** (`unique`/`dedup` return a NEW list — first-occurrence dedup vs
  consecutive-run collapse; `chunk`/`windows` return `List[List[T]]`, `n<=0` faults, `windows` `n>len`
  empty; `take_while`/`drop_while`/`count`/`position` are predicate methods that snapshot the receiver).
  Still open: `group_by`, `partition`, `flat_map` (need method-own type args / Map / tuple returns — a
  separate higher-risk wave).
- Map: `get_or(k, default)` / `setdefault`, `items()`, `map_values`, `filter`. Set: `is_subset` /
  `is_superset` / `is_disjoint`.

### 3. Lazy iterators (itertools) — **SHIPPED (2026-07-16)**
- ~~No lazy adapters: `count` / `cycle` / `repeat` / `chain` / `islice` / lazy `map`/`filter`/`take` as
  `Iterator[T]`. `std.iter` is all-eager `List`.~~ **SHIPPED** as pure-Chezzi generators in `std.iter`:
  `count(start=0, step=1)`, `repeat(x, n=-1)`, `cycle(xs)`, `chain(a, b)`, `islice(it, stop)`, and the
  lazy `imap`/`ifilter` (named to avoid the eager `map`/`filter` — Chezzi has no overloading). Infinite
  sources (`count`/`repeat`/`cycle`) terminate under `islice`. Follow-ups: `chain` is two-arg only
  (varargs / list-of-iters later); `take(it, n)` alias dropped (collides with eager `take`; `islice`
  covers it).
- **OPEN — lazy `itakewhile`/`idropwhile` over `Iterator[T]`.** The eager `take_while`/`drop_while`
  shipped as `List[T]` methods (§2, 2026-07-16), but `std.iter` has no lazy while-adapters (it has lazy
  `imap`/`ifilter`/`islice` but no while-form). Python `itertools.takewhile`/`dropwhile` are lazy;
  add the `i`-prefixed generators here in a later §3 wave (same pure-Chezzi generator shape as `imap`).

### 4. IO / files
- **Interactive CLI — SHIPPED** (see *Interactive CLI* below): `chezzi run` streams stdout, `io.flush()`
  and `io.input(prompt)` exist, and a prompt appears before its blocking read.
- **Buffered output — SHIPPED (R2).** `buffered(stdout())` (Go's `bufio.NewWriter` escape hatch) batches
  writes; the module-level `io.flush()` stays an honest no-op for the unbuffered stdout sink, and the
  *Writer*'s `flush()` is the real drain.
- **Writer / file handles — SHIPPED (R2).** `create`/`append` openers + a write-only `Writer`
  (`write`/`write_bytes`/`flush`/`close`) — append-to-an-open-file + streaming write now exist. Whole-file
  read stays (`std.io`: `read_file`/`read_bytes` ≤64 MB, `write_file`/`write_bytes` uncapped).
- **Reader / file handles — SHIPPED (R2b).** `open(path)` opener + a read-only `Reader`
  (`read_line`/`read_bytes`/`close`/`lines`) — line/chunk streaming of a large file (past the 64 MB
  whole-file cap) now exists, the read twin of R2's `Writer`. `lines() -> Iterator[str]` (a lazy
  generator over `read_line()`) is SHIPPED as a bodied Chezzi method on the native handle
  (`for ln in r.lines():`).
- **Read-all-stdin; char read — SHIPPED.** `io.read_all() -> str` drains all remaining stdin to EOF
  as one `str` (Python `sys.stdin.read()`; `""` at clean EOF; non-UTF-8 = fault, no stdin `read_bytes`
  hatch), and `io.read_char() -> Option[str]` reads one Unicode scalar as a 1-char `str` (`None` at
  clean EOF; partial/invalid UTF-8 = fault). Both are siblings of `read_line` — same shared stdin
  source, same task behavior (not offloaded; inherit the v1 pin-a-worker limit).
- **fs grab-bag — SHIPPED.** `fs.canonicalize(path) -> Result[str]` (resolves symlinks + `.`/`..`
  against the real filesystem — requires the path to EXIST, distinct from the lexical `path.normalize`),
  `fs.chmod(path, mode: int) -> Result[nil]` (unix permission bits, unix-only), and
  `fs.atomic_write(path, contents) -> Result[nil]` (same-dir temp + `rename`; observer-atomic +
  mode-preserving, not fsync-durable) all now exist.

### 5. Numbers / math
- **SHIPPED (Wave-3 Run E):** `gcd`, `lcm`, `sign`, `trunc`, `hypot`, `cbrt`, `factorial`, `comb`,
  `perm`, `parse_int_base(s, base)` (int-from-base, base 0 or 2..=36 w/ `0x`/`0o`/`0b` prefixes), plus
  `math.inf` / `math.nan` constants. Python `math` semantics; `factorial`/`comb`/`perm`/`parse_int_base`
  return `Result[int]` (clean `Err`, never a fault, on bad domain or i64 overflow). See `stdlib.md §std.math`.
- **`divmod` SHIPPED** — Python `(q, r)`. Landed NOT by expanding the native seam (`NativeRet` still has
  no `Tuple`) but as a **bodied Chezzi fn** in `std/math.chz` (`fn divmod(a, b) -> (int, int): return
  (a / b, a % b)`) — the first user of the hybrid native+Chezzi module form (bodyless `native fn`s and a
  bodied `fn` in one std file; see `syntax.md`). Uses Chezzi's own C-style `/` (truncating) and `%`
  (dividend's sign), so it is `(a / b, a % b)` — NOT Python's floor `divmod` (`divmod(-7,2)` is `(-3,-1)`
  here, `(-4,1)` in Python); a Python-floor variant would drift from Chezzi's own operators, a worse
  surprise than matching them.
- No **decimal / bigint**. `int` is a checked i64 (overflow FAULTS, never promotes), so a big-number or
  exact-money program simply cannot be written — there is no workaround. (Python: `int` is arbitrary
  precision + `decimal`; Go: `math/big`.) Rare in scripting; deferred, but it is a hard wall, not a
  slow path.

### 6. OS / system
- ~~os: `setenv`, `chdir`, `getpid`, `platform`, `hostname`, `environ()`, `home_dir`, `temp_dir`~~ —
  **SHIPPED** (2026-07-16): all eight in `std.os`. Queries (`getpid`/`platform`/`hostname`/`home_dir`/
  `temp_dir`/`environ`) are engine-agnostic; `setenv`/`chdir` mutate global state (see below). Still
  open: `os_name` alias for `platform` (trivial follow-up), Windows `USERPROFILE` fallback for
  `home_dir` (unix-focused today), metadata-reader.
- **No cleanup story at all** (three bullets that are really one): no temp-file/temp-dir creation, **no
  signal handling / `atexit` hook**, and `os.exit` does **not** run `defer`s. So a program that must
  clean up on Ctrl-C or on exit has no reliable path. (Python: `tempfile` + `atexit` + context managers;
  Go: `os.CreateTemp` + `defer` + `signal.Notify`.)
- ~~**No TTY detection**~~ — **SHIPPED** (2026-07-16): `io.isatty()` / `io.isatty_stdin()` /
  `io.isatty_stderr()` `-> bool` (via `std::io::IsTerminal` on stdout/stdin/stderr) let a CLI colorize
  only when not piped. Terminal size / echo-off (password prompts) remain a deliberate second step.
- **`os.env` and `process.cmd` disagree** (PARTIALLY RESOLVED 2026-07-16): `os.env`, `os.environ`, and
  `os.setenv` are now mutually consistent — all three read/write the SAME injected `HostConfig` env map,
  so a `setenv("X","V")` is observed by both `env("X")` and `environ()["X"]`. The map is **shared**
  (`Arc<Mutex<…>>`) across M:N workers, so a `setenv` from inside a task is visible to the parent +
  siblings — process-global, matching the serial engine and Python/Go (serial == M:N, no parity break);
  `environ` sorts by key so both engines emit identical output. What remains is the process-boundary
  axis: `process.cmd` shells out with the REAL inherited process env, so a `setenv` (HostConfig-only) is
  NOT seen by a child, and under a synthetic host config `os.env("X")` can differ from
  `process.cmd("echo $X")`. Bridging that would require writing the real process env at `setenv` (racy,
  edition-2024-unsafe `std::env::set_var`) — deliberately not done.
- fs: ~~recursive `walk`~~ — **`fs.walk(path) -> Result[List[str]]` SHIPPED** (deterministic per-dir
  sorted flat list, does NOT follow symlinked dirs; `native/fs.rs`). `remove_dir_all` (intentionally
  omitted today — see `stdlib.md §std.fs`). ~~metadata READ (mtime / permissions / size-struct)~~ —
  **`fs.stat(path) -> Result[FileInfo]` SHIPPED**: a native `FileInfo` struct (size/mtime/mode/is_dir/
  is_file/is_symlink), follows symlinks like `stat`/`os.stat`. `fs.chmod` still SETS permission bits
  (`fs.stat().mode` now READS them).

### 7. Crypto / encoding
- crypto: `sha256` / `sha256_bytes` / `sha1` / `sha1_bytes` / `sha512` / `sha512_bytes` / `md5`, plus
  `hmac_sha256(key, msg)` (RFC 2104, over the SHA-256 primitive). ~~secure-random-bytes / token~~ —
  **SHIPPED**: `secure_bytes(n) -> bytes` / `token_hex(n) -> str` (Python `secrets`), OS `getrandom`,
  **fail-closed** (recoverable fault, never weak fallback), 1 MiB cap; `token_urlsafe` (base64url)
  deferred. Missing: password hashing (bcrypt/argon2); `hmac_sha1`/`hmac_sha512` not shipped (add if a
  caller needs them — they want a block-size param + `&[u8]` adapters). All hand-rolled zero-dep today,
  so each is real work.
- encoding: no gzip / zlib (new dependency). ~~no CSV~~ — **CSV SHIPPED** as a NEW pure-Chezzi module
  `std.csv` (`parse(text) -> List[List[str]]` / `format(rows) -> str`, RFC 4180 quote state machine,
  round-trip proven; `std/csv.chz`, NOT `std.encoding` which is file-backed native). Deferred v1
  follow-ups: streaming/Reader, header→Map mapping, custom-delimiter/TSV `parse_sep`. Arbitrary-**bytes** base64 round-trip →
  **DONE (R1)** (`base64_encode_bytes`/`base64_decode_bytes`); hashing a *file* → **DONE (R1)**
  (`io.read_bytes` + `crypto.sha256_bytes`). Not added: hex / URL-safe bytes twins (~6 lines each, on demand).
- **URL parsing read-half — SHIPPED**: `query_decode(q) -> Map[str,str]` (dup key last-wins, `+`/`%20`
  → space, malformed escape kept raw) and `url_parse(u) -> Map[str,str]` (lexical
  scheme/host/port/path/query/fragment, components stay encoded, port a string) now round out
  `url_encode` / `url_decode` / `query_encode`. (Correction: the "Small, pure-Chezzi" label here was
  wrong — `std.encoding` is a FILE-BACKED NATIVE module; all members are bodyless `native fn` decls in
  `std/encoding.chz` implemented in `src/native/encoding.rs`. A pure-Chezzi fn there is dead code.)

### 8. Net
- TCP (`std.net`) + HTTP-client (`std.request`) only. No UDP, no HTTP **server**, no DNS-resolve
  exposed, no raw TLS socket (`request` does HTTPS internally via ureq). Also missing: unix-domain
  sockets, `shutdown()` half-close, socket options (`set_nodelay`, `SO_REUSEADDR`, keepalive),
  `Socket.peer_addr()`.
- **The HTTP-server blocker was not "no framework"** — you *can* hand-roll one on `listen`/`accept`/
  `read`/`write`. The blocker was that the socket seam was **`str`-only**, so a hand-rolled server could
  serve JSON and could not accept an image. **FIXED by R1** (`Socket.read_bytes`/`write_bytes`, 2026-07-14):
  binary sockets work byte-exactly. HTTP *fetch* of a binary body — a separate, `std.request`-side gap —
  is now **also DONE** via `request.get_bytes` (2026-07-15, byte-exact `into_reader().read_to_end`).
- **`std.net` now works from top-level `main` on the default engine (`W7-40`, 2026-08-10) — and
  `--serial` is still a deliberate divergence.** The rule is one predicate,
  `Vm::may_block_socket_in_place` (`src/vm/netio.rs:2044`): a would-block socket op **parks the fiber on
  the netpoller** when it is on an M:N worker; **blocks the calling thread in place** in exactly two
  contexts — the pre-existing in-callback demote (`mn.is_some() && native_reentry > 0`) and **top-level
  `main` on the default engine** (`parallel && eager_core.is_none() && is_counted_party()`); and returns
  the pre-existing `Err("<op> would block: std.net sockets require the --parallel engine")` everywhere
  else. Previously the *only* non-error path was the netpoller park, so all four of
  `read`/`read_bytes`/`write`/`accept` errored whenever `Vm::mn` was `None` — which is *not* "the parallel
  engine is off" (that is `Vm::parallel`) but "this Vm is not an M:N worker shell" — and the error fired
  on the DEFAULT engine, so the hello-world TCP server was unwritable. Go blocks `Accept()` on the main
  goroutine and now so does this.
- **`--serial` and eager `Executor` jobs keep the `Err`, on purpose, and it is measured.** Blocking in
  place needs a thread that is *not* also owed to the peer that would make the fd ready. `--serial` has
  exactly one thread, so blocking it starves the fiber that would connect — a permanent hang with **no
  escape**, since `--timeout` is rejected alongside `--serial` (`src/main.rs:660`). An eager `Executor`
  job runs on the bounded process-wide pool (`src/vm/pool.rs:77`, `vm::worker_count`, `src/vm/mod.rs:304`,
  which never grows), so at `CHEZZI_THREADS=1` a blocking job starves the sibling job that would unblock
  it. (**Re-derived 2026-08-18, UNCHANGED-BY-DESIGN**: `W8-8`'s two-runner floor lived in the nursery
  enlist/owner path in `src/vm/sched.rs`, while `vm::pool` is sized straight off `worker_count()` — so
  `CHEZZI_THREADS=1` gave the job pool exactly one thread before that fix and still does. Re-measured on
  the 1-wide binary: both ops return their `Err` in 0.006 s, rc=0, no hang.) Both were **measured as rc=124 hangs** on a first attempt that blocked everywhere; see `W7-40`.
  So `std.net` remains a **standing, deliberate serial≠M:N divergence** — the one this file has named
  since the 2026-07-14 audit — narrowed, not removed.
- `connect` is the exception, and since **`W7-59`** (2026-08-14) it is a *narrower* one, deliberately.
  It refuses **only** inside an eager `Executor` job; on `--serial` and on top-level `main` it blocks
  through the shared `demote_block_socket` loop and succeeds. Routing it through
  `Vm::may_block_socket_in_place` like the other four — the shape this file proposed while the gap was
  open — was measured **wrong**: on Linux a non-blocking `connect` reports `EINPROGRESS` even to a live
  loopback listener, so the wide gate breaks `--serial` `net.connect` outright. `accept`/`read` wait on
  a chezzi peer fiber that can only run on the thread they would block (self-starvation, `W7-40` R1);
  a `connect` handshake is finished by the **kernel** and starves no chezzi party, and both ancestors
  block it (CPython `socket.connect` 0.1 ms, Go `net.Dial` 314 µs). The old private
  `block_until_connected` sleep-spin — 10 s cap, no cancel, no `--timeout`, no replacement worker — is
  deleted; an executor job on a black-hole address with an outer `shutdown_now()` at 200 ms went
  **10 009 ms → 209 ms**.

### 9. Date/time — `parse_iso8601` LANDED; `strptime`/`from_string` remain
**`datetime.parse_iso8601(s: str) -> Result[DateTime]` shipped** (pure-Chezzi, the exact inverse of
`to_iso8601`): parses ISO-8601 / RFC-3339 — date-only, `'T'`/`' '` separator, optional `Z` or
`±HH:MM` offset (normalized to UTC), optional truncated `.fff` — with clean `Err` on malformed /
out-of-range fields. So a script **can** now turn a JSON / HTTP-header / CSV / log timestamp into a
`DateTime`. **Remaining follow-up:** a `strftime`-pattern formatter and a general
`strptime`/`from_string` (format-token vocabulary) — deferred (no token surface in v1, would balloon
scope). Known ceilings: sub-second precision dropped (`DateTime.second` is int), non-`Z` offsets
normalize to UTC rather than round-tripping. (Python: `fromisoformat` done, `strptime` pending; Go:
`time.Parse` layout pending.)

- ~~**No Go-like first-class `Duration` type.**~~ **SHIPPED** as pure-Chezzi `std.duration`
  (`std/duration.chz` + one `include_str!` in `std_embed.rs`; `Duration` is a plain user struct over an
  int of **milliseconds** — no native seam). Constructors `millis/seconds/minutes/hours(n)`, accessors
  `as_millis()/as_seconds()/as_minutes()/as_hours()`, arithmetic `add`/`sub`/`scale`, a Go
  `time.Duration.String()` formatter `to_string()` (`"1h30m0s"`, `"1.5s"`, `"250ms"`, `"0s"`, `"-1.5s"`)
  and its inverse `parse("1h30m")` (Go's looser forms + clean `Err` on malformed), plus `since(start:
  float) -> Duration` and `sleep(d)` convenience over native `std.time`. `sleep_ms`/`timer` stay int-ms
  (additive). Sub-ms ceiling documented (µs/ns → `Err`). Correctness = `parse`/`to_string` round-trip
  vectors in `examples/duration_test.chz` + `examples/duration.chz` golden (both engines). See
  `docs/reference/stdlib.md §5`.

### 10. Missing modules a real script reaches for
- ~~**`std.flag` — CLI arg parsing.**~~ **SHIPPED.** Pure-Chezzi `std/flag.chz`: a Go-`flag`-style
  `FlagSet` (`flag.new()` → `str_flag`/`bool_flag`/`int_flag` → `parse(args) -> Result[List[str]]` →
  `get_str`/`get_bool`/`get_int`/`positionals()`/`usage()`) over `os.args()`. `--name value` /
  `--name=value` / bool-presence / `--` terminator; unknown/missing/non-int → clean `Err`. See
  `docs/reference/stdlib.md §5 std.flag`.
- ~~**`std.log` — levels + timestamps + stderr default.**~~ **SHIPPED.** Pure-Chezzi `std/log.chz`:
  `log.new(min_level=INFO, to_stderr=true) -> Logger` with `debug/info/warn/error(msg)` gated by
  `set_level`, formatting `"LEVEL message"` (Go `slog` levels `DEBUG<INFO<WARN<ERROR`) to stderr by
  default. Timestamps are opt-in/injectable via `set_prefix` (the pure `format_line` core stays
  deterministic — no baked-in clock). See `docs/reference/stdlib.md § std.log`.
- **`std.db` (sqlite).** Absent. Reachable *in theory* via FFI to `libsqlite3` (the opaque `ptr` type
  names `sqlite3*` as its motivating case) but that is a research project, not a workaround. Blocks
  persistence-shaped scripts. **Large.**
- Config formats (TOML/YAML/INI): absent, JSON only. Low priority — JSON + env vars cover it. If ever:
  TOML, not YAML.
- ~~`bisect` / `binary_search` on a sorted `List` (sort/sort_by already exist). ~10 lines.~~
  **SHIPPED.** Pure-Chezzi `std/bisect.chz`: `bisect_left`/`bisect_right`/`bisect` (alias) +
  `insort_left`/`insort_right` over `List[T: Comparable]` (Python `bisect` semantics; left = before
  equals, right = after). No key-fn variant / no bare `insort` alias in v1 (YAGNI). See
  `docs/reference/stdlib.md § std.bisect`.
- ~~`functools.cache` / `memoize` — now *possible* (closures-as-data landed); ~15 lines.~~
  **SHIPPED.** Pure-Chezzi `std/memoize.chz`: `memoize1(f: fn(K) -> V) -> fn(K) -> V` caches per
  distinct arg in a captured `Map` (native ref type, so the cache persists across calls). Single-arg
  only — N-arg would key `Map[tuple, V]` but tuples aren't Hashable map keys yet. See
  `docs/reference/stdlib.md § std.memoize`.
- Runtime templating (`render(tpl, vars)`) — interpolation is compile-time only. Mostly obviated by
  format specs; the residual need is HTML generation, and **if an HTTP server ever ships, the lack of an
  auto-escaping template is an XSS hole**, not an ergonomics gap.

### 11. `std.process` cannot talk to a running child — *the ranked list had no `process` entry at all*
All three members (`cmd`/`run`/`run_args`) call `.output()`: spawn, wait, collect. There is **no
`Popen`/`exec.Cmd` equivalent**, so you cannot pipe stdin to a child, read its output incrementally
(progress from `ffmpeg`, a `tail -f`), set its env or cwd, get its pid, kill it, or run it in the
background. A child producing 4 GB of stdout is buffered entirely in RAM. `stdlib.md §std.process`
admits "Not yet: stdin piping, output streaming, per-process env/cwd overrides" — but that never made
it here. Compounded by the missing `os.setenv`/`os.chdir`: with neither, there is **no way at all** to
control a child's environment or working directory. Needs a `Child` handle (sibling of R2's `Writer`).

## Language features (category added 2026-07-14 — this file previously had none)

Verified against the parser/checker, not the docs. **Not gaps** (checked, and worth recording so nobody
"fixes" them): protocol **existentials give real dynamic dispatch** (trait objects work —
`examples/poly_method.chz`); `defer` is block-scoped and strictly more general than `with` for a
language with no destructors (`future.md §1` rejected `with` and is still right); generators/`yield`,
comprehensions, varargs, default args, keyword args, newtype, type aliases, static methods, enums with
methods — all shipped. The mutability model (aggregates share by reference like Python objects,
`Shared`/`Atomic`/`Channel` for cross-task shared mutation) is coherent. (`ref T`/`Ref[T]` were removed
2026-07-19 — see `future.md §12`.)

### L1. `Result` / `Option` have **ZERO methods** — **DEPRIORITIZED (2026-07-15): not imitating Rust's method surface**
`native enum Option[T]` / `Result[T, E]` (`std/prelude.chz`) declare no methods, and there is no
`Ty::Result`/`Ty::Option` arm in the method-call checker. So there is no `unwrap_or`, `unwrap_or_else`,
`is_ok`, `is_some`, `ok()`, `map`, `map_err`, `and_then`, `expect`. Verified: `Some(1).unwrap_or(0)` →
*"type Option[int] has no method 'unwrap_or'"*. Every `Result`/`Option` is handled with `match` or `?`.
**Small** if ever wanted (the `native enum … native fn` method-table machinery already exists — it is how
`List` works, ~8 native methods) — but **deprioritized**: `match`/`?` is the intended surface, and L3 (the
one thing L1 methods would have "unblocked") is itself won't-do, so there is no downstream forcing it.

### L2. No struct patterns in `match` — **struct match-patterns FIXED (2026-07-15); let/fn-param destructuring still deferred**
**Struct patterns in `match` now work**: `match p: Point(x, y):` binds the fields positionally, mirroring
enum-variant patterns (a struct has exactly ONE constructor, so a lone all-binding `Point(x, y)` arm is
irrefutable ⇒ exhaustive with no `_`). Nested (`Line(Point(x, y), _)`), generic (`Box(v)` on `Box[int]`
binds `v: int`), literal fields (`Point(0, y)` — refutable, needs a `_`/catch-all), and a whole-value
catch-all binding (`rest:`) all work. **Both a BARE `Point(x, y)`** (a local / `from`-imported struct)
**and a QUALIFIED `geo.Point(x, y)`** (the only spelling for a WHOLE-module-imported struct, since the bare
name isn't in scope — symmetric with qualified construction `geo.Point(3, 4)`) destructure. Arity mismatch,
a wrong constructor, an enum-name-collision qualifier (`E.Point`), a non-module qualifier, a 3-part path,
and a DUPLICATE constructor arm are all clean checker errors, never runtime panics. Reserved/native struct
handles (Socket/Ref/Match/…) are **not** destructurable (checker-gated to `StructOrigin::User`, so the
compiler never sees a struct pattern it can't lower). Example: `examples/match_struct.chz`. Landed as a
checker + pattern-compile + VM-lowering change reusing the enum-variant `Pattern::Variant` node (no new AST
node/opcode): `MatchKind::Struct` + `struct_fields_of` (checker/sig.rs), the Struct arms in
`bind_match_arm`/`bind_subpattern`/`check_exhaustive` + the shared `resolve_struct_ctor` (checker/pattern.rs),
and `struct_key_of_pattern` (bare + module-qualified) + the refined `EnsureEnum` guard + the `emit_pattern`
struct branch (compiler/mod.rs).
**Still deferred:** `let`-destructuring is tuple-only (`let Point(x, y) = p` — `StmtKind::Let` carries
`names: Vec<String>`, not a `Pattern`, so it needs a separate parser+AST+let-lowering seam, not this one);
no destructuring in fn params. (Python 3.10 class patterns; Rust/Go destructuring.)

### L3. Error handling: no conversion, no wrapping, no discrimination — **WON'T-DO (2026-07-15)**
**FIRST, the correction that scoped this down (2026-07-15):** a concrete error type WIDENS to the
built-in `Error` existential exactly like Go's `error` interface — a `struct`/`enum` with a
`message(self) -> str` **method** (declared *inside* the block, not a free `fn`) flows into an `Error`
param, into the `Result`-E position (`return Err(MyErr(..))` in a `-> int!` fn), and through `?`
(`inner()? ` where `inner` is `-> int!MyErr` inside a `-> int!` fn). Verified by `check` + `run` on both
engines. So the idiomatic `T!` (= `Result[T, Error]`) style already composes — `?` is NOT broadly broken.

Given that, the three "holes" are narrow and NOT worth building:
- **`?`-time conversion.** Only concrete-E1 → *different* concrete-E2 is closed (`T!IoErr` called from
  `T!DbErr`). Concrete → `Error` widens fine (above). Cross-concrete auto-conversion is rare and
  arguably SHOULD be explicit (that's a real decision, not boilerplate) — a Rust `From`-style machinery
  is exactly the imitation we don't want. **Won't-do.**
- **Wrapping / cause chain** (`source()`/`Unwrap()`). Nice-to-have, not blocking. **Defer.**
- **Downcast out of the `Error` existential** (`errors.As`). Needs **R4**, which is won't-do — avoid by
  keeping the error a typed enum and `match`ing it before laundering to `Error`. **Won't-do.**

### L4. ~~No `const`~~ — **`const` SHIPPED (2026-07-17)**; visibility still open
- ~~No `const`/`final` keyword.~~ **SHIPPED.** `const T` is an immutable *binding* modifier in the
  same type-slot as `ref` (`PI: const float = 3.14`) — the checker rejects any later reassignment
  (`=` + every compound). Immutable binding, NOT a compile-time constant (runtime RHS is fine, JS
  `const`/Java `final` semantics), and **shallow** (freezes the name; a `const` container's contents
  stay mutable). Locals + module globals only; rejected on params/`:=`/destructuring and `ref const`.
  Const-ness rides `ModuleSig.const_values` so a from-import/qualified rebind of a `const` global (or a
  native constant `math.pi`/`e`/`inf`/`nan`) reports it as const. Mirrors the `ref` sidecar end-to-end
  (`const_decls`); compile-time-only, zero VM/parity change. See `syntax.md §const T`,
  `examples/const_binding.chz`.
- **STILL OPEN — visibility.** No `pub`/private (every name in a module is importable). (Go:
  capitalization export; Python: convention + `__all__`.) Small-to-medium (resolver + `ModuleSig`
  filter). **Deferred**: it guards a boundary only **R3** (package manager) opens — with one author and
  no external importers, enforced privacy protects nothing yet. Do it when R3 lands. (`_`-prefix is the
  chosen spelling — Python-consistent, no new keyword, and std already uses it by convention.)
- Struct-**field** immutability (a `const` field) is a separate, unshipped axis (fields are all mutable).

### L4b. ~~`assert_native_protocol_shape_matches` does not guard `PathLike`~~ — **FIXED 2026-08-08**

The debug-only drift guard (`src/checker/setup.rs`) `debug_assert_eq!`s each reserved protocol's
`std/prelude.chz` decl against its `prebuilt_protocols` Rust seed. Its name list carried **20 of the
21** reserved protocols: `PathLike` was mirrored in `std/prelude.chz` but absent from the list, so it
was the one protocol shape that could drift between the `.chz` mirror and the Rust seed without any
test noticing. Pre-existing (it predated M23 — `PathLike` landed with W7-8 and was never added); M23
only made it visible, when the "All 20 of them are drift-guarded here" comment stopped matching.

**FIXED** — `"PathLike"` is in the list; all 21 are guarded. The harvest was the deliverable, not the
list edit: `PathLike`'s `std/prelude.chz` decl round-trips through `harvest_protocol_shape` and
**matched the seed unchanged** (zero type params, zero embeds, one `as_path(self) -> bytes`), so
neither side needed a correction. The guard was proven to actually EXECUTE for `PathLike` by drifting
the mirror's return type to `str` in a debug build — it panicked with `protocol 'PathLike' method
'as_path' sig drifted from prebuilt_protocols` — then reverting. (Debug build required: the body is
`cfg!(debug_assertions)`-gated and is a no-op in release.)

**FOLLOW-UP — the MECHANISM is gone, not just the instance (2026-08-08).** The list edit closed the
`PathLike` case and left intact the thing that produced it: the reserved set was written out **three**
independent times (`is_reserved_protocol`'s `matches!`, `prebuilt_protocols`'s keys, the guard's own
literal list) plus **two** stale hand-copied subsets in `checker/tests.rs` (16 and 17 of 21 names), and
nothing forced any of them to agree — a 22nd protocol re-opens the identical hole silently. Now there
is one declaration, `checker::RESERVED_PROTOCOLS`: `is_reserved_protocol` is a `.contains` over it (all
its callers are per-DECLARATION hoist/setup checks, so the linear scan over 21 entries is off any hot
path — it sits beside an identical `native::ffi::TYPE_NAMES.contains(..)`), the guard iterates it, and
both test lists are driven from it. The guard **additionally** `debug_assert_eq!`s the const's contents
against `prebuilt_protocols`'s key set — the half a const alone does not solve, catching *reserved but
not seeded* (a bound resolving to nothing) and *seeded but not reserved* (a builtin a user can shadow).
That set assertion passed unchanged on the first run, so no pre-existing mismatch was hiding behind the
three lists; widening the two test subsets to all 21 also passed, so the five names they had never
covered (`Any`, `Eq`, `Iterator`, `Contains`, `PathLike`) were already correctly rejected at their decl
sites. Proven to fire, in a debug build, in both directions: deleting `"Contains"` from the const and
adding an unseeded `"Bogus"` each panicked `RESERVED_PROTOCOLS and prebuilt_protocols disagree on the
reserved protocol set` with the two sorted sets diffed, then reverted. Adding a protocol is now a
two-place edit (the const + a seed) that fails loudly if you do only one.

### L5. Operator-protocol holes
The reserved set (`Add Sub Mul Div Mod Neg Arithmetic Comparable Stringable Hashable Index IndexSet
Slice Contains Iterator Iterable Convert Any Error`) covers arithmetic, ordering, indexing, slicing,
membership, iteration, hashing, display. **`Eq`** (`eq(self, other: Self) -> bool`) joined the set in
M23 as a reserved protocol + generic bound, **B2** (the permissive `==`) is FIXED, and `==`/`!=` now
dispatch to a user `eq` — gated on the hook's exact signature at the declaration, so a method that
merely shares the name (`Opt[T].eq(self, x: T)`) stays an ordinary method and `==` stays structural.
Container probes (`Map`/`Set` keys, `in`, `list.contains`/`index_of`/`dedup`/`unique`, set algebra,
and the recursive element/field/entry arms) route through the SAME dispatch — the hook lives in
`Vm::values_equal_guarded`, so every consumer inherits it, and containers keep CPython's
`x is y or x == y` identity short-circuit. The `hash`/`eq` contract stays the implementor's (an `eq`
coarser than its type's `hash` is structurally unreachable, exactly as in Rust/Python).
`Atomic.cas` is the one deliberate exception: it compares under the value lock, so it stays
structural — and a type with a user `eq` is already an illegal `Atomic[T]` payload.
**Residual (found writing the M23 docs) — FIXED 2026-08-08: a `newtype` may no longer declare `eq`, on
ANY underlying.** The W6-3d decl-site rule used to fire only for a *numeric* newtype, on the premise
that a non-numeric one "has no operator to disagree with" — true for `+`/`<`, false for `==`, which
exists for every newtype underlying. Measured on `5b699c9b`, both engines:
```chezzi
newtype Name = str:
    fn eq(self, o: Name) -> bool: return true
a := Name("a")
b := Name("b")
print(a == b, a.eq(b))     # false true   <- the two spellings disagree
```
`Name` never satisfied `Eq` (so a `[T: Eq]` bound already rejected it) and the answer was stable across
engines, so it was a wart, not a soundness hole — but it was the *milestone's own* defect one type-kind
over, so it ships fixed rather than filed. `eq` moved out of W6-3d's numeric-only list into an
unconditional per-newtype reject (`checker/setup.rs`, the newtype arm), with its own message naming
`==` (one diagnostic per declaration: a numeric newtype matches both premises and still reports once).
Task 3's struct/enum carve-out — a *generic* operand (`fn eq(self, x: T)`) marks an ordinary method and
leaves `==` structural — deliberately does **not** transfer: it exists to tell the hook apart from a
same-named ordinary method on a type whose `==` *does* dispatch, and a newtype's dispatches to no user
method at all, so both operand shapes are rejected. Ruling (b) — make `==` dispatch to it, as Rust's
`impl PartialEq` on a tuple struct and Python's `__eq__` on a wrapper do — is the same one W6-3d
implemented and rejected for `compare`: the numeric intrinsic grant is unconditional, so a
heterogeneous `List[Eq]` would take the user's equality for a same-newtype pair and the native one for
a newtype/underlying pair, giving non-transitive equality with no fault. Pinned by
`checker::tests::newtype_eq_method_rejected_at_decl` (numeric / non-numeric / both generic operand
shapes / the ordinary-method + `compare` boundary) and, for the behavior that remains, by
`tests/chz/spec/intrinsic_proto_methods_test.chz::newtype_equality_is_the_underlyings` on both engines.
Still missing: bitwise/shift protocols, and a call operator. Small
each. **`Contains`** (`x in my_struct` via `contains(self, item) -> bool`, Python's `__contains__`) —
**FIXED**: a user struct/enum with a `contains(self, item) -> bool` method makes `x in that_value`
dispatch to it, yielding `bool`; container `in` (list/set/map/str) is unchanged.

### L6. Smaller, confirmed
- Enums carry **no discriminant/value**, no variant iteration, no int conversion (Go's `iota`, Python's
  `Enum.value`). Small.
- No labeled `break`/`continue` (Go has them; Python doesn't). Small.
- No generator *expressions* (`(x for x in xs)`) — comprehensions are `[]`/`{}` only; `yield` covers it
  verbosely.
- No walrus in expression position (`if (n := f()) > 0`) — `:=` is a statement.
- No **struct embedding / extension methods**: methods may only be declared in the type's own body (no
  `impl` block), so you cannot add a method to a builtin or to another module's type, and "composition
  not inheritance" means hand-forwarding every delegated method. (Go's embedding is *the* composition
  mechanism.) Medium.
- Protocols have **no default method bodies** (a protocol method with a body is a parse error) → no
  mixins. Go's interfaces don't either; Python ABCs do. Small, if ever wanted.
- **Not a gap:** spread/unpack (`f(*args)`) was deliberately dropped in `spec.md` and varargs +
  `.concat`/`.merge` cover it.

### L7. Sendability-bounded protocol existentials — the sound way to admit `Channel[Error]` (✅ LANDED 2026-07-20)

**⚠️ SUPERSEDED by Task 2 (2026-07-21, backlog item 1 above).** The `Error`-only / "Rust `Send`, not
Go" framing below was **reversed**: all user protocol existentials are now sendable (Go `chan interface`
parity), `sendable_bounded` is deleted, and the genuine-non-sendable gate is the **runtime airlock**
(FFI/native handles), not a checker widening-site sweep. The "a struct satisfying `Error` yet holding a
non-`Error` protocol field launders past the gate" concern below is moot — that struct is genuinely
sendable now (a protocol field crosses by deep value copy); a field holding an FFI/generator handle is
caught at the runtime airlock. The historical Error-only record is kept below for provenance.

**⚠️ Airlock value-store gap — CLOSED 2026-07-23.** The "caught at the runtime airlock" claim above
held for the spawn-**arg**/capture/`submit`/worker-return paths (they pair `to_wire_at` with
`ensure_crossable`) but was **false** for the cross-heap **value-store** paths: `Channel.send`/
`try_send`/`wait:`-send-arm and every `Shared`/`RwShared`/`Atomic` construct/set/update/store/CAS
called bare `to_wire_at` with NO handle reject. An FFI/native/module handle sent over a channel or
stored in a `Shared` therefore crossed silently on `--serial` (and even executed) while M:N
reconstructed a garbage cross-heap `GcRef` — serial≠M:N + type confusion. Fixed by routing every
value-store site through a single `Vm::to_wire_crossable` helper (`= to_wire_at` then
`ensure_crossable`, `src/vm/sched.rs`), so both engines now reject identically and recoverably at the
send / store / construction site with the `a module handle cannot cross` message. Legit
`Channel`/`Shared`/`Executor`/socket handles map to shared-`Arc` wire arms (`has_handle()` == false)
and still cross unchanged (regressed by `positive_*` parity tests). **UPDATE 2026-07-23:** native
(`Obj::Native`) + FFI (`Obj::Cffi`) fn values were later moved OFF this reject — they are pure code and
now cross the airlock BY VALUE / shared `Arc` at every site (`WireValue::Native`/`Cffi`), so the sole
remaining reject is a genuine `Module` handle (see the session log below).

**✅ LANDED (branch `feat/l7-sendable-error`, commits `c1b4ab4` core gate · `997e642` direct-literal
guard · `2b29ed3` regression/residual tests · `ba2ea7c` recover diagnostic).** Surface shipped:
**`Error`-only, sendable-bounded by default** (not all protocols — that over-rejects in-task
non-sendable protocol values and diverges from Rust's opt-in `dyn Error + Send`; reference model is
Rust's `Send`, not Go's share-by-reference channels). `Channel[int!]` / `Channel[Error]` now
type-check and cross a task boundary on both engines; a non-sendable error witness is **rejected at
the widening site**, never laundered.

Design ("Option B", 5 edits, all `src/checker/`): `sendable_rec`'s `Ty::Protocol` arm returns
`self.sendable_bounded(p)` (`== "Error"`, the single surface knob); the three Error-**inference**
synthesis sites (`fill_ret` sig.rs, `default_expr_result_e` pattern.rs, `join_err_slot` sig.rs) default
to the `Error` existential **only if the concrete payload is sendable, else preserve the concrete
type** (so in-task use of a non-sendable error stays legal — the concrete survives to the boundary);
the explicit/direct-literal widening chokepoint (`assignable`'s `Protocol` arm) **rejects** a
non-sendable concrete when the target is sendable-bounded. Every value write-site routes through
`assignable`, so that one guard covers all explicit widenings including `?`-propagation.

Clarifications learned in implementation: (1) `Iterator[T]` is `Ty::Struct`, **structurally sendable**
— a live generator is handled by the runtime reach-gate, not the checker — so the *type-level*
non-sendable witness is a struct holding a **non-`Error` protocol / `Module` field**, not a generator
field (the old F2 framing below overstated the generator case). (2) A `recover:` block's error slot is
the (now sendable-bounded) `Error`, so `recover: f()?` requires `f`'s error to be sendable too; the
diagnostic distinguishes *doesn't-satisfy-Error* from *satisfies-but-non-sendable*.

**Deferred follow-ups (non-blocking):** (a) the direct-literal send rejection surfaces as a generic
type-mismatch (`expected Result[int], found Result[GErr]`), not the friendly "must be sendable" hint —
`assignable` returns `bool` with no reason channel; wire `sendable_error_hint` at the send call site
later. (b) `join_err_slot` is branch-order-sensitive for 3+ branches mixing sendable/non-sendable
`Error` payloads (over-rejection, not a soundness gap). (c) Full Option-B for `recover` (preserve the
concrete error in the recover *result* so in-task non-sendable recover stays legal) — deferred as rare
+ risky; the current construct-imposed `Error` slot rejecting non-sendable is consistent with explicit
annotations. (d) Per-use `+ send` bounding (Rust's `dyn Draw` vs `dyn Draw + Send`) if a second bounded
protocol ever appears.

*Original deferral note (historical — superseded by the landing above):*



**Motivation (F2, 2026-07-18 bug-hunt).** `Channel[int!]` / `Channel[Error]` are rejected today because a
protocol existential is non-sendable (`sendable_rec`, `src/checker/proto.rs`). A one-line whitelist of the
built-in `Error` existential was tried and **reverted as unsound**: the existential *erases field-level
sendability*, so a struct that satisfies `Error` yet holds a non-sendable field (a **live generator**,
a non-`Error` protocol / `Module` field) launders past the gate that the concrete `Channel[MyErr]` correctly rejects —
check-OK-then-run-fault (verified: `Err(GErr(gen()))` over `Channel[int!]` type-checked then faulted `a
generator cannot be sent across tasks`, both engines). The current **conservative rejection is correct**;
the workaround (a concrete sendable error type) already works: `Channel[int!str]`, `Channel[int!MyEnum]`,
`Channel[Result[int, MyErr]]` all type-check and send `Err(...)` across a spawn today, both engines. So this
is a **generalization, not a live gap** — deferred, not urgent.

**Why Rust, not Go, is the reference.** Go has NO static sendability check — you may send *anything* over a
channel (an `interface{}`, a pointer, a mutex), because Go channels **share by reference** (`chan *T` hands
both goroutines the same pointee) and defer safety to the race detector + discipline. Chezzi is the
opposite: the airlock **deep-copies** on send (tasks are isolated — value semantics, no shared mutable
memory except through `Shared`/`Channel` handles; this is why the B3 module-global mutation was *lost* on
M:N). That is **Rust's `Send`, not Go's channel model**. In Rust a bare `dyn Error` is not `Send`; you write
`Box<dyn Error + Send>` and the compiler then forces every concrete error stored into it to be `Send`. That
`+ Send` bound is exactly the design below.

**The feature: a sendability-bounded protocol existential.** Let a protocol (or a use-site) be marked
sendable-bounded, meaning the existential is itself sendable AND every value widened into it is required to
be sendable. Then `Channel[Error]` is sound: a `Ref`/generator-carrying witness is **rejected at the
widening**, not laundered — `sendable_rec` can safely return `true` for the bounded existential.

**The work (checker-side, well-scoped but real).** Chezzi protocols are **structural** (Go-style), so
widening to an existential is frequently *implicit* (a struct used where `Error` is expected — e.g. the
`Err(GErr(...))` argument in the F2 repro). Every implicit widening site becomes a sendability check point:
- add a sendable-bound marker to protocol types (a bounded existential `Ty`, or a per-use `+ send` flag);
- at every place a concrete type is coerced to a sendable-bounded existential (call args, `Err(...)`/`Ok(...)`
  payloads, struct fields, returns, channel `send`), require `sendable(concrete)` and error otherwise —
  mirroring the `+ Send` propagation;
- flip `sendable_rec`'s `Ty::Protocol` arm to `true` for a sendable-bounded existential (only);
- decide the surface: is `Error` sendable-bounded by default (simplest — errors are almost always plain
  data), or opt-in per protocol / per use? Default-bounded `Error` gives `Channel[int!]` for free but would
  reject a today-legal `Err(struct-with-Ref)` used purely in-task — measure that blast radius first.

**Risk / why POST-FREEZE.** This is checker surface with **real false-positive risk** (every widening site
must be found; a missed one is a soundness hole, an over-eager one rejects legal in-task code). Do NOT
attempt before the JIT freeze. The concrete-error-type workaround covers the practical need until then.
Related: [B2](#b2--between-disjoint-types-type-checks--fixed-m23) (another
typed-language tightening, landed), and the note that **dropping `ref`/`Ref` was the WRONG lever *for F2*** — it
would not close the generator-field laundering that F2 is about, so this milestone stands on its own.
(NOTE 2026-07-19: `ref`/`Ref`/`std.ref` were later removed *separately*, on minimalism/coherence
grounds — they only added scalar aliasing over Chezzi's Python object model — **not** as an F2/sendability
fix. That removal neither addresses nor blocks this L7 milestone; the two are orthogonal.)

## Tooling / ecosystem (category added 2026-07-14 — this file previously had none)

The CLI ships exactly 8 commands (`init run test check tokens ast docs help`). **R3 (no package
manager) is the headline and lives above** — it is the one gap that keeps the language author-only.

### T1. ~~Installing `chezzi` produces a binary that can't find its own stdlib~~ — **FIXED**
> **FIXED** (`fix(resolver): embed std/ so an installed chezzi finds its own stdlib`). `std/**/*.chz` is
> now `include_str!`'d into the binary (`src/resolver/std_embed.rs`, the same pattern the CLI already
> used for the `docs/*.md` topics), and *every* `std.*` source read — `Builder::visit` (incl. the
> always-linked `std.prelude`/`std.ref`) and `Builder::visit_native_file` (the file-backed natives
> `math`/`regex`/`io`/…) — routes through the new `resolver::std_source(dotted)`: **`$CHEZZI_STD` (dev
> override, exclusive) → the embedded stdlib.** The build-time `CARGO_MANIFEST_DIR/std` path is no longer
> in the *read* chain, so an installed `~/.cargo/bin/chezzi` keeps working with the checkout moved or
> deleted (verified E2E: `mv std std.bak`, then `chezzi run` + `chezzi run --serial` a program importing
> `std.math` / `std.regex` / `std.concurrency.collection` — byte-identical on both engines). A missing std
> module now says *"no such module in the stdlib"* instead of leaking the build machine's path. The
> hand-written table is rot-guarded by `embedded_std_table_matches_disk` (embedded key set **and**
> contents == the on-disk `std/` tree): **add a `std/foo.chz` and that test fails until you add its
> `include_str!` line.**
>
> Residual: a **pre-built** binary plus an edited `std/*.chz` is stale until rebuilt (`cargo run`/`cargo
> test` rebuild automatically via `include_str!`; the documented escape is `CHEZZI_STD=./std`).
>
> Residual 2 (**open**, found by the review panel, deliberately NOT fixed): `LoadedModule::is_std`'s
> ENTRY backstop still keys on `path_under_std_root` → `std_root()` → the build machine's
> `CARGO_MANIFEST_DIR/std`, which on an installed binary does not exist (`canonicalize` errs → `false`).
> So type-checking a stdlib file **as the entry** (`chezzi check ./std/concurrency/collection.chz` from
> an installed binary) loses stdlib auto-privilege and reports bogus "unknown type" errors on its bare
> reserved types (`RwShared`, `Map`). Before the embed this path failed loudly at `std.prelude` instead.
> Real, but the fix is re-keying `is_std` off the dotted path — a resolver change larger than the bug,
> with no plausible user (nobody entry-checks the stdlib from an installed binary). Revisit if one appears.

The original finding: `std_root()` = `$CHEZZI_STD` else **`env!("CARGO_MANIFEST_DIR")/std`**
(`src/resolver/mod.rs`), and the `std/*.chz` files were **not embedded** (only `docs/*.md` were
`include_str!`'d). So `cargo install --path .` yielded a `~/.cargo/bin/chezzi` that read its stdlib from
*the source checkout's build-time path*: move or delete the repo and every `import std.*` broke. The code
comment admitted it deferred "a real install story to M6, when `std/` actually ships content" — M6
shipped; the install story did not.

### T2. ~~`chezzi repl` is a stub that ERRORS — while `--help` advertises it~~ — **FIXED (de-advertised)**
> **FIXED** (`fix(cli): drop the repl stub — it never shipped`). The `repl` subcommand arm and its USAGE
> line are **deleted**: `chezzi repl` is now a plain *unknown command* (prints USAGE, exits 1), which is
> the honest behavior for a command that does not exist. `docs/reference/spec.md`'s M1 row no longer claims a REPL
> shipped, and the `CLAUDE.md` Commands block no longer lists it. **No REPL was built** — the idea lives
> in `docs/design/roadmap.md` (Tier 4, Ecosystem) as an explicitly-unbuilt item, which is its only correct home.

The original finding: `src/main.rs` printed *"'repl' is not implemented yet"* and exited 1, while `USAGE`
still listed `repl  Start an interactive REPL` — so for a language pitched as "Python-feel scripting" with
an ~11× faster cold start than CPython, the first thing a Python user types errored out. Building one
remains Medium: a naive v1 (accumulate lines, re-check + re-run the buffer, print the last expression) is
small, but the real work is incremental checker state, since the checker is whole-graph oriented.

### T3. No formatter
No `chezzi fmt`; no formatting provider in the LSP. (`src/fmtspec.rs` is the `{x:.2f}` mini-language —
easy to misread as a source formatter. It isn't one.) Convenience today with one author; **structural
the moment R3 lands and several people write code** — and a significant-whitespace language with no
formatter is especially exposed. Medium-large: needs a real AST→source printer with comment/blank-line
preservation (the AST doesn't carry comments today).

### T4. Test tooling is thin (but the base is honest)
`assert cond, msg`, `test fn`, `*_test.chz` discovery, `PASS/FAIL name (file:line:col)`, non-zero exit — a
real runner. **FAIL vs ERROR split SHIPPED (2026-07-24, §3b #1):** an `assert` failure buckets as FAIL,
any other runtime fault as ERROR (summary `P passed, F failed, E errored`). **`--max-heap=<bytes>`
memory cap SHIPPED (2026-07-24, §3b #1b):** the deterministic-in-VM runaway-allocation guard — a test
whose in-VM `Heap::live_bytes()` exceeds `N` is hard-aborted (bypassing `recover:`) and bucketed
`OVER-MEMORY` (counts as failure); `0`/omitted = OFF, so cap-off output + the dual-engine gate are
byte-identical (checks the same `lb` computed per `sweep()`, not OS RSS). The cap is **per-heap**: each
M:N worker owns its own heap (measures a task alone), so a real runaway trips on whichever worker heap
runs it. That is also the cap's real limitation, not an engine artifact — there is no cross-heap
aggregate, so a *concurrent* test whose allocations are *split* below `N` per-fiber but *sum* above it
across fibers may not trip (`docs/design/roadmap.md §3b`). v1 also trips only at a GC boundary + on `Obj`-count
growth — see §3b #1b. **`--timeout=<ms>`
wall-clock cap SHIPPED (2026-07-24, §3b #4):** the sibling of `--max-heap` — a test running longer than
`N` ms is hard-aborted (bypassing `recover:`) and bucketed `TIMED-OUT` (counts as failure); `0`/omitted
= OFF, so timeout-off output + the dual-engine gate are byte-identical. It rides the same `is_timed_out`
`RuntimeError` marker machinery, but the trip is observed at the **loop back-edge** (`jump_checked`) — the
hottest engine-independent checkpoint — so it catches BOTH the top-level test body (which runs outside the
fiber scheduler) and `spawn`ed-task loops. **Zero clock reads when off** (the `deadline: Option` guard
short-circuits before any `Instant::now()`; the read is throttled 1/1024 back-edges when on). A wall-clock
trip is non-deterministic, which is why the timeout-off output is what the byte-identity gate compares.
**v1 limit (watchdog follow-up):** a test blocked in a native call (blocking syscall, `Channel.recv` with
no traffic) or spinning in loop-free infinite recursion (hits the stack guard) is NOT caught — a true
watchdog thread is the next seam. **Selection + output ergonomics SHIPPED (2026-07-24, §3b #5/#6/#7):**
`-k`/`--filter <substr>` (run a subset by name; `(K filtered out)` in the summary; zero-match = clear
failure), `--fail-fast` (stop at first non-pass, deterministic order), `--show-output` (surface a
FAILING test's stdout byte-exactly, default discard; SHIPPED byte-exact 2026-08-16, matching
`chezzi run`/`go test` — see `W6-9r` item 4), `--errors=json` (machine output mirroring `check`/`run`:
`{tests:[{name,file,line?,status,message?,duration_ms}],totals}`, suppresses human lines — `message`
added 2026-08-18, the failure text on every non-`pass` verdict), `-q`/`-v` verbosity
(dots vs per-line vs per-line+timing), `--color=auto|always|never` (isatty-gated tag color), and per-
test/total timing (`-v`/json ONLY — never in default/quiet, so the byte-identity gate is untouched).
All opt-in; **default (no-flag) output is byte-identical to before**. Still missing:
fixtures/setup-teardown beyond suite hooks, coverage, benchmarks, `assert_eq` with a diff, parallel
execution across files — tracked as `docs/design/roadmap.md §3b` follow-ups (CLI ergonomics).

**KNOWN-LIMIT — assert inside an FFI callback buckets ERROR, not FAIL (found 2026-07-24, WON'T-FIX).**
The FAIL/ERROR split reads `RuntimeError.is_assert`, set true only by the `Op::Assert` arm. But when a
Chezzi closure fires as a *scalar-only C callback* (`invoke_callback`, `src/vm/mod.rs`), an inner
`assert false` is laundered through `HostError` — which carries only `message` — and re-raised
`is_assert:false` at the native-return boundary (`src/vm/call.rs:210/326`), so the runner tags it ERROR.
Deterministic (both engines agree — not a parity bug), and the test still FAILS with a non-zero exit; only
the bucket *label* is wrong, on an exotic path. The clean fix (thread `is_assert` through `HostError` +
its ~10 construction sites) grows a boundary type for one cosmetic label — poor trade, so documented not
fixed. Revisit only if FFI-callback tests become common.

### T5. No debugger, no profiler, no doc generator
- **Debugger:** nothing (no breakpoints, no DAP, no stepping). What exists is post-mortem: a fault
  trace. And there is no REPL either (T2 removed the false advertisement; **no REPL was ever built**),
  so the language has **no interactive introspection of any kind** — the debug loop is "add a `print`,
  re-run". An (unbuilt) REPL would buy most of this value far more cheaply than a DAP server; it is
  tracked as a Tier-4 idea in `docs/design/roadmap.md`, not as a shipped or in-progress feature.
- **Profiler:** nothing user-facing. Ironic for a project mid-perf-milestone: the VM is profiled with
  external Rust tooling, but a Chezzi *user* cannot find their own hot function. (Python: `cProfile`;
  Go: `pprof`, best in class.) A sampling counter keyed by function + a flat report is contained.
- **Doc generation:** `chezzi docs` prints *the language's own* embedded spec — it does **not** generate
  docs from a user's source. The raw material already exists (the lexer captures doc-comments; the LSP
  surfaces them on hover). Small-medium, and it's what makes third-party libraries browsable once R3
  lands (`go doc` / pkg.go.dev is a big part of why Go's ecosystem is navigable).

### T6. CI-friendliness — **not** a gap
`--errors=json` works for `check`, `run`, AND `test` (test-runner machine output SHIPPED 2026-07-24,
§3b #7 — per-test `{name,file,line?,status,message?,duration_ms}` + totals; `message` added
2026-08-18); exit codes are correct and
deliberate (type error → 1, fault → 1, `os.exit(n)` honored, stdout write failure → 1). No gap.

## Type-system / construction (adjacent, tracked in `docs/design/roadmap.md §15`)
- **Definable conversion constructors already exist** as named **static factory methods** (`fn
  Type.from_x(...) -> Type`, `Type.origin()`) — the Rust `T::from` / `T::new` idiom. No Python
  `__init__`-style overridable primary ctor is planned: `Type(...)` stays "set the fields, positionally"
  by design (`spec.md`: conversion is always visible).
- **`Convert[S]` protocol** (bound-only) is the principled generalization for generic-over-conversion
  (`[T: Convert[S]]`), and **generic construction over the bound LANDED in M24** (2026-08-10) —
  `fn make[T: Convert[int]](seed: T, n: int) -> T: return T.convert(n)` runs. It is one consumer of a
  general mechanism (witness passing: a hidden trailing parameter carrying the concrete type's runtime
  identity key), which serves `T.default()` on any user protocol with a static requirement.
  Value-position conversion (`c: Convert[int]` as an annotation) stays **rejected by design**, not
  deferred: a value cannot invoke a static ctor. Surface + the four remaining walls: `docs/reference/syntax.md
  §7a`. Its residuals **M24-1**, **M24-2**, **M24-3**, **M24-4** and **M24-5** are all fixed (table above).
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
