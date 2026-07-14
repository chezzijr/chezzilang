# Chezzi — gap backlog

Catch-all backlog of missing / shallow surface. **Not a commitment** — draw from it when a feature
earns its own milestone. Categories: **bugs** (fix, don't backlog), **root causes** (one change that
unblocks many gaps), **language**, **stdlib**, **IO/runtime**, **tooling/ecosystem**, **deps**.

**Audit history:** first stdlib pass 2026-07-07. **Full four-axis audit 2026-07-14** (IO/runtime,
stdlib breadth, language features, tooling) — that pass found one live data-corruption **bug**, three
cross-cutting **root causes** that were each recorded as unrelated footnotes, and a whole missing
**tooling** category. It also found the file's own #1 entry ("number format-specs") had been **shipped
and never de-staled**. Re-audit periodically: a gap backlog nobody re-reads rots into a to-do list for
work already done.

## Bugs found by the 2026-07-14 audit — FIX, do not backlog

### B1. `Socket.read` silently CORRUPTS data (`from_utf8_lossy`) — P0 — **FIXED (2026-07-14, R1)**
`src/vm/netio.rs:315` and `:360` did `String::from_utf8_lossy(&buf)`, and `std/net.chz` types the
method `read(self, n: int, ...) -> Result[str]`. So the socket seam **had to** lossily decode. Two
failures, both silent — no `Err`, no fault, just wrong data:
1. **Any binary payload** (TLS, an image, protobuf, a gzip body) becomes U+FFFD replacement chars.
2. **Even pure UTF-8 text** is mangled when a multibyte codepoint straddles a `read(n)` chunk boundary
   — i.e. the ordinary "read in a loop" idiom. VERIFIED end-to-end (`--parallel`, localhost TCP,
   sending `"héllo"`, reading 1 byte at a time):
   ```
   expected   : héllo
   reassembled: h��llo      # equal? false
   ```
This is the same family as the false-EOF and the swallowed exit status: **the runtime lies to the
program.** It is worse than those, because it corrupts *data* rather than control flow, and `std.net`
is documented as working.

**MITIGATION LANDED (2026-07-14).** Both lossy sites now route through one guard,
`Vm::decode_carry` (`src/vm/netio.rs`), and `from_utf8_lossy` is gone from the socket path.
The two failure modes are now separated, exactly as `Utf8Error` separates them:
- **Split codepoint (`error_len() == None`) — case 2 is FIXED, not merely reported.** The incomplete
  ≤3-byte tail is retained on the `SocketCore` and prepended to the next read, so a byte-at-a-time read
  of valid text reassembles **byte-exactly**. Contract: `n` bounds the NEW bytes off the fd, so a
  `read(n)` may return up to `n + 3` bytes; a read whose chunk holds no complete codepoint re-reads
  (never `Ok("")` — that is the EOF sentinel), so it may block past its first fd read. `timeout_ms` bounds
  the WHOLE call (the deadline is latched on the fiber — `Vm::poll_deadline` — so re-parking to finish a
  codepoint does not re-arm the budget — on the in-callback demote path too), and the carry survives a
  timeout `Err`. Blocking for the rest of a character is the Go `bufio.Reader.ReadRune` / Python
  text-mode-socket contract. A poll-once `read(n, 0)` that took a partial codepoint says so —
  `Err("incomplete utf-8: …")`, not the `Err("timeout")` that means *nothing arrived*. `read(0)` is a
  no-op `Ok("")` (it never touches the fd, so it can neither spin nor fake an EOF) but still reports a
  closed socket, and the fd read + carry update are ONE critical section (carry lock outer), so two tasks
  sharing a socket decode in wire order.
- **Genuinely invalid bytes (`error_len() == Some(_)`) — i.e. a BINARY payload — case 1 is REPORTED, not
  supported:** `Err("invalid utf-8 on the socket: std.net read is str-only — binary payloads need
  Socket.read_bytes …")`. The error is **non-destructive and sticky**: the valid text that arrived before
  the bad byte is delivered first, the undecodable bytes stay carried on the socket, and every later read
  re-errs identically — so a caller that logs the `Err` and keeps reading (what a `Result` invites) cannot
  silently shred the stream. It must `close()`. (Swallowing the chunk would just be silent data loss
  wearing an `Err`.) An incomplete codepoint left when the peer closes is likewise
  `Err("invalid utf-8 at eof: …")`, never a silent drop.

**FIXED (2026-07-14) — R1 landed the honest fix.** `Socket.read_bytes(n[, timeout_ms]) -> Result[bytes]`
and `Socket.write_bytes(b[, timeout_ms]) -> Result[int]` (`src/vm/netio.rs`, declared in `std/net.chz`):
they never decode, so **binary sockets work byte-exactly**. `read_bytes(n)` returns AT MOST `n` bytes
(the natural byte contract — the str `read`'s `n` bounds only the NEW fd bytes, hence its `n + 3`), `Ok(b"")`
is the EOF sentinel, and it **drains the carry first** — so the undecodable bytes the str `read`'s sticky
`Err` refused to deliver are recovered here instead of forcing a `close()`. The str `read` keeps its
documented decode contract, unchanged (`read_bytes` is purely additive).
**What remains is not a defect:** the caller must pick the right method — a `str` seam cannot hand back
bytes that are not UTF-8, and it now says so and points at `read_bytes`.

### B2. `==` between disjoint types type-checks (a proposed tightening, not a clear bug)
`1 == "a"` compiles and evaluates to `false` (`src/checker/pattern.rs`, the `Eq | NotEq` arm returns
`Ty::Bool` without checking operand compatibility). Note the tension before "fixing" it: this is
**exactly Python's runtime behavior** (`1 == "a"` → `False`), so by the no-drift rule it is not a
divergence. But Chezzi is **statically typed**, and a comparison between provably disjoint types is
always a bug in user code — which is why mypy ships `--strict-equality` to reject it and Go/Rust make
it a compile error. Recommendation: reject at check time (a typed language should), and say so in the
docs as a deliberate, explained divergence from Python's runtime.

## Root causes — one change each, many gaps unblocked

These are the entries that were previously scattered as unrelated one-liners. Ranked by how much they
unblock.

### R1. The native seam cannot carry `bytes` — **DONE (2026-07-14)**
`bytes`/`bytearray` existed in the language, but `NativeRet` had no `Bytes` variant and `Host` no
`arg_bytes` (`src/native/mod.rs`), so **no native fn could accept or return them**. Landed as a seam
expansion (no new type, no heap obj, no GC/airlock work — they already shipped below the seam):
`NativeRet::Bytes` (lowered by `Vm::lower_native` to the immutable `Obj::Bytes`), a defaulted-to-error
`Host::arg_bytes` (on `VmHost`: `bytes`-only — a `bytearray` is not assignable to a `bytes` sink
(7b29552), so a built-up buffer is passed as `bytes(ba)`, the explicit copy CPython also makes), and
`NativeArg::Bytes` + `OffloadHost::arg_bytes` so a *blocking* bytes native still offloads to the dirty
pool instead of pinning a core worker (D5). `value_to_native_ret` gets no bytes arm on purpose (it fills
C's return register; a callback return is checker-restricted to C scalars).
Consumers wired, and the gaps that were filed separately as if each were its own feature:
- binary file read/write → **DONE**: `io.read_bytes(path) -> Result[bytes]` / `io.write_bytes(path, b) ->
  Result[nil]` (`read_file` decodes UTF-8, so it hard-failed on any binary file — it now errs with
  `use io.read_bytes for binary files`). Same 64 MB read cap; `write_bytes` uncapped, like `write_file`.
- arbitrary-bytes base64 round-trip → **DONE**: `encoding.base64_encode_bytes` / `base64_decode_bytes`.
  gzip/zlib → **still open** (a new dependency, not a seam gap).
- binary sockets → **DONE**: `Socket.read_bytes` / `write_bytes` — this is the fix for **B1** (above).
  A hand-rolled HTTP server can now accept an image.
- `sha256` of a file / hashing binary data → **DONE**: `crypto.sha256_bytes(b)` over `io.read_bytes(p)`.
- `std.request` cannot fetch a non-text body (`src/native/request.rs:62` → `into_string()`), i.e.
  **"download a file" is still impossible** → **still open, and it never was an R1 gap**: it needs
  reader plumbing + a `Response.body` type change inside `std.request`. Its own (small) task.

### R2. There is no `Writer` / file-handle type — so no buffering, no streaming
Files are read/written **whole** — `std.io`'s `read_file`/`read_bytes` cap the read at 64 MB
(`MAX_READ_FILE_BYTES`, `src/native/io.rs`; the write side is uncapped). (`std.fs` has **no file-content
API at all** — only metadata + mutations, `std/fs.chz`.) There is no `io.Writer`-equivalent anywhere.
Binary whole-file write landed with R1 (`io.write_bytes`); what R2 owes is *handles*. Consequences:
- **You cannot opt into buffered output.** Every `print` is one `Msg::Write` → one `write_all` +
  `flush` (`src/vm/stream.rs`), i.e. **one syscall per print, with no way out**. `io.flush()` is a
  genuine no-op (`Host::flush_stdout` is a defaulted no-op, `src/native/mod.rs`).
- **Go gets away with unbuffered `os.Stdout` precisely because `bufio.NewWriter` exists**, and every Go
  programmer reaches for it in a hot loop. Python block-buffers automatically when piped. **Chezzi took
  Go's default without Go's escape hatch** — the worst corner of that design space. (Measured: 200k
  lines piped = 0.108s unbuffered vs 0.068s for CPython's buffered; the writes are already off the VM
  thread, so the gap is smaller than it looks, but the *capability* is missing.)
- No line-streaming of a large file; no writing to anything that isn't stdout/stderr/whole-file.
**The principled home:** a `Writer` native handle (the `Socket` userdata is the template) from
`fs.open(path, mode)` / `io.stdout()`, with `write`/`write_bytes`/`flush`/`close`, plus a
`buffered(w, size)` wrapper. Buffering then becomes *a value you hold*, not a global mode — and
`io.flush()` keeps its honest no-op meaning for the process's (unbuffered) stdout. Lands together with
R1 and with file handles: **one milestone, not three.**
**Known ceiling (mapped in-tree):** the stream queue is **unbounded** (`src/vm/stream.rs:26-27`, a
`ponytail:` comment naming the same upgrade path) — a program printing faster than a stalled consumer
drains grows memory without limit. Deliberate (never pin a core worker), but it is a real ceiling;
bounded `sync_channel` is the upgrade.

### R3. No package manager — **the wall that keeps Chezzi author-only**
`Manifest` is `{name, version, entrypoint}` (`src/manifest.rs`) and the parser **silently ignores**
unknown sections, so a `[dependencies]` block does nothing. The resolver knows exactly two roots — the
project root and `std_root()` (`src/resolver/mod.rs`) — so **a third-party Chezzi library cannot be
imported at all**, except by copying its `.chz` files into your tree. No registry, no lockfile, no
versions, no vendoring.
Everything else in this file is a bad afternoon for a user. This one is a closed door: **nobody can use
anyone else's code, and nobody can use yours.**
`docs/ffi-and-packaging.md §6.1` calls the pure-Chezzi source registry "cheap, do first" — and it is
(a third resolver search path + a fetch cache + a lockfile; **no** ABI/NaN-boxing/`repr(C)` work, which
is only needed for *native* packages). It has never been scheduled. That mis-sequencing — the cheap 90%
stalled behind a native-ABI narrative it does not depend on — is the most consequential finding of the
audit.

### R4. No runtime type tags → no `cast[T]`, no `errors.As`
`Any` (an empty protocol) lets values *in* and nothing *out*; there is no `type()`, no `isinstance`, no
downcast. Protocol **existentials do** give real dynamic dispatch (`examples/poly_method.chz`), so the
sharp edge is narrower than `future.md §14` implies — it is mostly **error discrimination** (see L3)
and dynamic data-walking. **Size: large** (needs runtime type tags on heap objects); design already
correct in `future.md §14`.

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

### 1. String formatting
- ~~Number format-spec in interpolation~~ — **SHIPPED** (`src/fmtspec.rs`, `Op::ToStrFmt`,
  `docs/syntax.md §10`): the full Python mini-language, `{x:.2f}` / fill / align / width / `d f x X b o
  e %`. This entry sat here as "the single biggest ergonomic gap" long after it landed — the audit's
  cautionary tale. It also largely **obsoletes** the next bullet (`"{s:^10}"` is `center`).
- `str.pad_right` / `center` / `ljust` / `rjust` / `zfill` — now only *method spellings* of what format
  specs already do. Downgraded: alias sugar, not a gap.
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

### 4. IO / files — *see **R2** (no `Writer` type) for the root cause*
- **Interactive CLI — SHIPPED** (see *Interactive CLI* below): `chezzi run` streams stdout, `io.flush()`
  and `io.input(prompt)` exist, and a prompt appears before its blocking read.
- **No way to BUFFER output** (R2). Unbuffered always, one syscall per `print`, `io.flush()` inert.
- Files are **whole-file only** (`std.io`: `read_file`/`read_bytes` ≤64 MB, `write_file`/`write_bytes`
  uncapped) — binary IO landed with R1; what is still missing is **file handles + line-streaming** (R2).
- Read-all-stdin; char read.
- fs: no `canonicalize`/`realpath` (`path.normalize` is purely lexical — no symlink resolution), no
  `chmod`/executable bit, no atomic write (write-temp + rename).

### 5. Numbers / math
- `divmod`, `gcd`, `lcm`, `sign`, `trunc`, `hypot`, `cbrt`. `math.inf` / `math.nan` constants (only
  `pi`/`e` today). Int-from-base (parse a hex/bin string). `factorial` / `comb` / `perm`.
- No **decimal / bigint**. `int` is a checked i64 (overflow FAULTS, never promotes), so a big-number or
  exact-money program simply cannot be written — there is no workaround. (Python: `int` is arbitrary
  precision + `decimal`; Go: `math/big`.) Rare in scripting; deferred, but it is a hard wall, not a
  slow path.

### 6. OS / system
- os: `setenv`, `chdir`, `getpid`, `platform` / `os_name`, `hostname`, `environ()` (all vars),
  `home_dir` / `temp_dir`.
- **No cleanup story at all** (three bullets that are really one): no temp-file/temp-dir creation, **no
  signal handling / `atexit` hook**, and `os.exit` does **not** run `defer`s. So a program that must
  clean up on Ctrl-C or on exit has no reliable path. (Python: `tempfile` + `atexit` + context managers;
  Go: `os.CreateTemp` + `defer` + `signal.Notify`.)
- **No TTY detection** — `isatty` does not exist (zero hits in `src/`). So a CLI must either always
  colorize (garbage when piped) or never. `io.isatty() -> bool` is **one fn** (`std::io::IsTerminal`) —
  the cheapest real win in this file. Terminal size / echo-off (password prompts) are a second step.
- **`os.env` and `process.cmd` disagree**: `os.env` reads the injected `HostConfig`, while `process.cmd`
  shells out with the real inherited process env — so under a synthetic host config `os.env("X")` is
  `None` while `process.cmd("echo $X")` prints the real value. Nobody has written this down.
- fs: recursive `walk`, `remove_dir_all` (intentionally omitted today — see `stdlib.md §std.fs`),
  metadata (mtime / permissions / size-struct).

### 7. Crypto / encoding
- crypto: `sha256` / `sha256_bytes` (R1: hash binary data / a file) / `md5`. Missing `sha1` / `sha512`,
  **`hmac`**, secure-random-bytes / token,
  password hashing (bcrypt/argon2). All hand-rolled zero-dep today, so each is real work.
- encoding: no gzip / zlib (new dependency), no CSV. Arbitrary-**bytes** base64 round-trip →
  **DONE (R1)** (`base64_encode_bytes`/`base64_decode_bytes`); hashing a *file* → **DONE (R1)**
  (`io.read_bytes` + `crypto.sha256_bytes`). Not added: hex / URL-safe bytes twins (~6 lines each, on demand).
- **URL parsing is the missing half of an already-built module**: `url_encode` / `url_decode` /
  `query_encode` exist — but **no `query_decode`** (query string → `Map[str,str]`) and no `url_parse`
  (scheme/host/port/path/query). You can build a query string and not read one back. Blocks anything
  webhook- or server-shaped. Small, pure-Chezzi.

### 8. Net — *and `std.net` is `--parallel`-only, which is a standing serial≠M:N divergence*
- TCP (`std.net`) + HTTP-client (`std.request`) only. No UDP, no HTTP **server**, no DNS-resolve
  exposed, no raw TLS socket (`request` does HTTPS internally via ureq). Also missing: unix-domain
  sockets, `shutdown()` half-close, socket options (`set_nodelay`, `SO_REUSEADDR`, keepalive),
  `Socket.peer_addr()`.
- **The HTTP-server blocker was not "no framework"** — you *can* hand-roll one on `listen`/`accept`/
  `read`/`write`. The blocker was that the socket seam was **`str`-only**, so a hand-rolled server could
  serve JSON and could not accept an image. **FIXED by R1** (`Socket.read_bytes`/`write_bytes`, 2026-07-14):
  binary sockets work byte-exactly. Missing HTTP *fetch* of a binary body is a separate, `std.request`-side
  gap (`into_string()`), not a socket one.
- **`std.net` requires the M:N engine**: off it, a would-block op returns `Err("read would block:
  std.net sockets require the --parallel engine")` (`src/vm/netio.rs`). So the same TCP program behaves
  differently on `--serial` vs the default engine. This is an *accepted design fallback*, not a bug —
  but it must be written down, because §"Audited residuals" previously claimed the task-stdin bug was
  "the only known serial≠M:N divergence", and that was **wrong as written**.

### 9. Date/time — `datetime` is **write-only** (no parsers)
`std/datetime.chz` has 21 functions and **every one is a formatter or epoch-math helper**: `from_epoch`,
`to_iso8601`, `to_date_string`… There is **no `parse_iso8601` / `strptime` / `from_string`**, and
`std.time` is `now`/`monotonic`/`sleep_ms`/`format` — also format-only. So a script **cannot turn a
timestamp from JSON, an HTTP header, a CSV or a log line into a `DateTime`**. The inverse is fully
built. (Python: `fromisoformat`/`strptime`; Go: `time.Parse`.) **Small** — `days_from_civil` already
exists; this is only the string→ints half. Blocks a very common script shape.

### 10. Missing modules a real script reaches for
- **`std.flag` — CLI arg parsing.** `os.args() -> List[str]` and nothing else. Every tool hand-rolls a
  `for` over argv (Chezzi's own `src/main.rs` does exactly that). Go-style `flag.str/bool/int` +
  positionals is ~120 lines of pure Chezzi, no native seam. (Python: `argparse`; Go: `flag`.)
- **`std.log` — levels + timestamps + stderr default.** Does not exist; every script reimplements it.
  ~80 lines pure Chezzi. (Python: `logging`; Go: `log`/`slog`.)
- **`std.db` (sqlite).** Absent. Reachable *in theory* via FFI to `libsqlite3` (the opaque `ptr` type
  names `sqlite3*` as its motivating case) but that is a research project, not a workaround. Blocks
  persistence-shaped scripts. **Large.**
- Config formats (TOML/YAML/INI): absent, JSON only. Low priority — JSON + env vars cover it. If ever:
  TOML, not YAML.
- `bisect` / `binary_search` on a sorted `List` (sort/sort_by already exist). ~10 lines.
- `functools.cache` / `memoize` — now *possible* (closures-as-data landed); ~15 lines.
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
methods — all shipped. The mutability model (`ref T`, by-reference capture, `Shared`/`Atomic`/`Channel`)
is coherent.

### L1. `Result` / `Option` have **ZERO methods** — highest hit-rate gap in the language
`native enum Option[T]` / `Result[T, E]` (`std/prelude.chz`) declare no methods, and there is no
`Ty::Result`/`Ty::Option` arm in the method-call checker. So there is no `unwrap_or`, `unwrap_or_else`,
`is_ok`, `is_some`, `ok()`, `map`, `map_err`, `and_then`, `expect`. Verified: `Some(1).unwrap_or(0)` →
*"type Option[int] has no method 'unwrap_or'"*. Every `Result` must be handled with `match` or `?`.
**Small**: the `native enum … native fn` method-table machinery already exists (it is how `List` works).
~8 native methods. This also unblocks half of L3.

### L2. No struct patterns in `match`, no struct destructuring
`match p: Point(x, y):` → *"variant pattern 'Point' cannot match a value of type Point"*. Patterns are
variant / tuple / literal / range / binding only; let-destructuring is tuple-only; no destructuring in
fn params. **Enums destructure and structs don't — the asymmetry is arbitrary.** (Python 3.10 class
patterns; Rust/Go destructuring.) Medium, and cheap at the VM (struct fields are already a positional
`Vec`).

### L3. Error handling: no conversion, no wrapping, no discrimination
Three holes, one seam — together these **block a class of program** (any library composing errors from
two sources; today the choice is one god-enum or stringly-typed messages):
- **No `?`-time conversion.** `?` requires the inner `E` be assignable to the enclosing `E`, so a
  `T!IoErr` fn called from a `T!DbErr` fn is a hard error — and with no `map_err` (L1) you hand-write a
  `match` at every call site. (Rust: `From`-based auto-conversion; Go: `fmt.Errorf("%w")`.)
- **No wrapping / cause chain.** The `Error` protocol is one method, `message() -> str`. No `source()` /
  `Unwrap()`.
- **No downcast out of the `Error` existential.** Once laundered into `Error`, only `message()` is
  callable. There is no `errors.As` / `except DbError` equivalent — that needs **R4**.

### L4. No `const`, no visibility
No `const`/`final` keyword (every module global and struct field is mutable); no `pub`/private (every
name in a module is importable). (Go: `const` + capitalization export; Python: convention + `__all__`.)
`const` is small (a checker flag on the binding); visibility is small-to-medium (resolver + `ModuleSig`
filter). Matters much more once **R3** lands and people publish libraries with an intended API surface.

### L5. Operator-protocol holes
The reserved set (`Add Sub Mul Div Mod Neg Arithmetic Comparable Stringable Hashable Index IndexSet
Slice Iterator Iterable Convert Any Error`) covers arithmetic, ordering, indexing, slicing, iteration,
hashing, display. Missing: **`Eq`** (`==`/`!=` cannot be overloaded — and see **B2**, the checker is
*permissive* about them), **`Contains`** (`x in my_struct` is a hard error — Python's `__contains__`),
bitwise/shift protocols, and a call operator. Small each.

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
> the honest behavior for a command that does not exist. `docs/spec.md`'s M1 row no longer claims a REPL
> shipped, and the `CLAUDE.md` Commands block no longer lists it. **No REPL was built** — the idea lives
> in `docs/future.md` (Tier 4, Ecosystem) as an explicitly-unbuilt item, which is its only correct home.

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
`assert cond, msg`, `test fn`, `*_test.chz` discovery, `PASS/FAIL name (file:line)`, non-zero exit — a
real runner. Missing: **test filtering** (`chezzi test` rejects *every* flag, so on a big suite it's all
or nothing — a ~20-line change and the best ratio in this file), fixtures/setup-teardown, coverage,
benchmarks, `assert_eq` with a diff, parallel execution, machine-readable output (`go test -json`).

### T5. No debugger, no profiler, no doc generator
- **Debugger:** nothing (no breakpoints, no DAP, no stepping). What exists is post-mortem: a fault
  trace. And there is no REPL either (T2 removed the false advertisement; **no REPL was ever built**),
  so the language has **no interactive introspection of any kind** — the debug loop is "add a `print`,
  re-run". An (unbuilt) REPL would buy most of this value far more cheaply than a DAP server; it is
  tracked as a Tier-4 idea in `docs/future.md`, not as a shipped or in-progress feature.
- **Profiler:** nothing user-facing. Ironic for a project mid-perf-milestone: the VM is profiled with
  external Rust tooling, but a Chezzi *user* cannot find their own hot function. (Python: `cProfile`;
  Go: `pprof`, best in class.) A sampling counter keyed by function + a flat report is contained.
- **Doc generation:** `chezzi docs` prints *the language's own* embedded spec — it does **not** generate
  docs from a user's source. The raw material already exists (the lexer captures doc-comments; the LSP
  surfaces them on hover). Small-medium, and it's what makes third-party libraries browsable once R3
  lands (`go doc` / pkg.go.dev is a big part of why Go's ecosystem is navigable).

### T6. CI-friendliness — **not** a gap
`--errors=json` works for `check` and `run`; exit codes are correct and deliberate (type error → 1,
fault → 1, `os.exit(n)` honored, stdout write failure → 1). Missing only machine-readable *test* output.

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

## Audited residuals — the Tier-0 post-merge gate (2026-07-14)

Found by the post-merge adversarial panel on the B1 merge. **Not** caused by it; none are blockers;
each is recorded rather than silently carried.

### N1. A last `print` into a just-closed pipe exits **0 or 1 nondeterministically** — a real bug
`stream_halt` (`src/vm/exec.rs`) is consulted **after** `emit_out` queues the line, and the EPIPE is
discovered asynchronously on the writer thread (`src/vm/stream.rs`). So for the *same* run, a program
whose final `print` lands in a pipe the reader just closed exits **0** (the VM's `Acquire` load wins →
bytes silently dropped) or **1** (the writer's EPIPE lands first → `stdout closed (broken pipe)` fault).
A ~nanosecond race decides which. **Python raises `BrokenPipeError` deterministically at write/flush.**
This is the `runtime lies to the program` family again — and it is what made `tests/interactive.rs` flake
(~1-in-N loaded; 5/60 pinned to one core). The TEST bug is fixed (`read_bytes_timeout` was itself
manufacturing the broken pipe by dropping `ChildStdout` early, then asserting `success()` — it now drains
to EOF). **The product race is NOT fixed** and nothing pins it. Fix = check `stream_halt` (or make the
write synchronous enough to observe EPIPE) *before* declaring the print done. Small; own task.

### N2. `Socket.write`/`accept` still restart their timeout budget on every park
`src/vm/netio.rs` `write` (~:658) and `accept` (~:755) still pass `timeout.map(|t| t.deadline)` — a
deadline **recomputed on every `ip`-rewind re-execution**. That is exactly the budget-restart bug
`Vm::poll_deadline` was added to kill for `read` (a park rewinds `ip` and re-runs the whole op, so a
re-parking op re-arms `now + timeout_ms` forever). Pre-existing, and `read` parks far more often now that
a split codepoint forces a re-park — but `write`/`accept` are the same class and should latch the same
way. Mechanical; own task.

### N3. Two cosmetic B1 leftovers
- The in-callback demote path (`src/vm/sched.rs`) and the netpoller-park path (`src/vm/netio.rs`) return
  `Err("timeout")` even when that call already took a **partial codepoint** off the wire — while the
  poll-once path was deliberately changed to say `Err("incomplete utf-8: …")` for exactly that case, and
  `docs/stdlib.md` now states `Err("timeout")` means *nothing arrived*. Non-destructive (the carry is
  kept), but it contradicts the doc the same change just wrote. No test.
- `read(0)` on a socket whose carry holds sticky **invalid** bytes still returns `Ok("")` (only a *closed*
  socket errs), so a `read(want - have)` loop that computes `0` cannot observe the sticky `Err`. Matches
  the documented contract, harmless, but it is the one hole in "every later read re-errs identically".

### N4. A cancelled task's `defer` **silently did not run** on M:N — **FOUND + FIXED (2026-07-14)**
**Pre-existing** (not caused by the B1/bytes-seam merge, which touched zero lines of `src/vm/sched.rs`).
Every cancel trip and its `cancel_drain` sat **two separate core-lock acquisitions apart** — in
`mn_worker_loop` (`sched.rs`) a faulting fiber is settled by `finish(...)` and only *then* by
`cancel_drain(scope_id)`, which is what requeues the scope's **parked** siblings so they can observe the
cancel and unwind. In that window another worker's `take_runnable` evaluated `is_deadlocked`, which had
**no cancel exemption**: it saw `running == 0 && runnable == 0 && inflight == 0 && parked_n > 0 &&
done < total`, declared **DEADLOCK**, and `flag_deadlock` wrote the still-parked sibling's slot as
`Deadlocked` and **dropped the fiber without ever calling `unwind_deferred`** — so its `defer`s never ran.
A file left unclosed, a lock left held, silently.

**Why it was invisible:** `reduce_task_slots` ranks `Exit > Fault > Deadlocked`, so the *real* sibling
fault is what got reported and the spurious deadlock was completely hidden. The skipped `defer` was the
**only** symptom — no program could detect it. Same "the runtime lies to the program" family as the false
EOF (§0) and N1.

**Fix:** a cancel-teardown **veto** in `MnSched::is_deadlocked` (`src/vm/mod.rs`) —
`SchedCore::any_incomplete_scope_cancelled()`: a scope with `cancel` set and `done < total` is
*mid-teardown*, not deadlocked. Placed at the predicate itself rather than reordering the one seam that
reported it, because **four** seams trip a scope cancel and then `cancel_drain` in a later lock
acquisition — `mn_worker_loop`'s `finish`→`cancel_drain`, `abort_enlisted_scope` (which first clears the
`awaiting_builder` veto), `abort_eager_nursery` (which first clears the `any_body_open` veto), and the two
demote self-detect loops. Patching only the first would have left the others broken: **an invariant
enforced at one seam is not enforced** (the wave-5 lesson, §0). Liveness holds because `park`/`park_wait`/
the netpoller's `register` all refuse to park a cancelled-scope fiber and every trip is followed by a
`cancel_drain` that requeues + notifies, so a cancelled scope always drains to `done == total` and the veto
is transient by construction. Genuine deadlock detection (nothing cancelled) is untouched.
Uses the **per-scope** `JoinScope::cancel`, never the legacy global one — an inner fault must not veto an
outer sibling.

Repro was a **race**: `parallel_defer_runs_on_cancelled_sibling` printed `0` instead of `42` in
**35/200** runs under CPU contention before the fix, **0/200** after (and `--threads=1`/`2` always passed —
no idle worker, so the window could not open). Pinned by the invariant test
`mnsched_cancelled_scope_with_parked_fibers_is_not_deadlock`, which asserts the predicate directly rather
than the scenario. `reduce_task_slots`'s ranking is **not** touched: it is correct — the spurious
`Deadlocked` simply must never be produced.

### N5. A **genuine** deadlock tears tasks down without running their `defer`s — open
Found while fixing N4, and **independent** of it. `flag_deadlock` (`src/vm/mod.rs`) drops each parked
`Fiber` **without** `unwind_deferred`, so on a real deadlock (every fiber parked, nothing cancelled, no
send possible) the tasks' `defer`s are skipped. Arguably the same silent-lie class as N4 — Go still runs
deferred fns on a panic. Deliberately **not** folded into the N4 fix, for two reasons:
1. The **serial** oracle does the same (it faults from the parent nursery join and never resumes the
   parked children), so the two engines currently **agree**. Fixing M:N alone would *break* serial == M:N
   parity — this is an engine-consistent **known limit**, not a divergence.
2. `flag_deadlock` runs inside `SchedCore` under the core lock with no `Vm` shell, so it cannot execute
   bytecode there. A real fix means requeueing the parked fibers with a deadlock sentinel (plus a matching
   serial change), which moves deadlock-path stdout ordering — a behavior change, so its own task.

Documented as the one exception to the "cancellation always runs `defer`" guarantee in
`docs/concurrency.md`.

### N6. `--serial` does **not** run a PARKED task's `defer` on a sibling fault — a real serial ≠ M:N divergence, open
Found while verifying the N4 fix end-to-end on the CLI (it is **not** caused by it — reproduced on the
unfixed `main` binary, `0b23703`). The N4 repro program (`spawn consumer` blocks on `ch.recv()` after
registering `defer cleanup(s)`; `spawn boom` faults; the fault is `recover:`ed and the sentinel printed):

| engine | 2026-07-14 unfixed `main` | after the N4 fix |
|---|---|---|
| M:N (default) | `42` (but `0` in **35/200** runs under load — that was N4) | `42`, **0/200** failures |
| `--serial` | **`0`** (10/10) | **`0`** (unchanged — N4 never touched serial) |

`0` means **the cancelled consumer's `defer` never ran**. Cause: serial's `run_scheduler`
(`src/vm/sched.rs`) drives children with `run_child(i)?` — the `?` propagates the faulting child's error
**straight out of the scheduler loop**, so the still-parked children are abandoned where they sit: never
resumed, never cancelled, never unwound. It is the same shape as N5 (a parked fiber torn down without
`unwind_deferred`), but on the *cancel* path rather than the *deadlock* path.

**M:N is the correct one here** (Go runs a cancelled goroutine's deferred fns); serial is the engine that
is wrong, which is uncomfortable given serial is the parity oracle — the oracle is not automatically
right, and this is the second time it has been the one bending the language (cf. §0's stdin false-EOF and
the stdout task-order buffering). The existing parity suite does **not** cover this scenario, which is why
it was green throughout.

**Not fixed here** (the N4 task is the M:N scheduler race, and this is a serial-engine change of its own):
the fix is for `run_scheduler` to trip the scope cancel and re-drive the parked children to completion
*before* propagating the fault, which necessarily moves serial's fault-path stdout ordering. Own task, and
it needs a parity test for exactly this shape.

## Audited residuals — pre-JIT hunt wave 5 (2026-07-13)

Everything below was **found, reproduced on both engines, and deliberately NOT fixed** in the wave-5
sweep (13 bugs fixed, main `0741a0b`). Each is either an accepted design consequence, a
documented-but-unusable surface, or a safe over-rejection. Recorded so they are decisions, not
surprises — **re-read this before the JIT freeze**, since a JIT bakes in whatever is true at freeze time.

### 0. Task stdin: serial-vs-M:N divergence + the false EOF — **BOTH FIXED (2026-07-14); stdin is now SHARED**
Two bugs, one seam. Stdin was **entry-task-owned**: every other task was handed `Stdin::Empty`, so
`read_line`/`input` inside a task returned `None` — a **false EOF**, while the entry task still had
unread lines queued. And that rule was enforced at exactly ONE task-entry seam (`swap_ctx` — the
`spawn:`/nursery fiber path), while the cooperative `Executor` drain runs a submitted closure **inline
on the entry Vm** (`src/vm/netio.rs`, no `swap_ctx`) — so on serial the task read *and consumed* the
entry's stdin while M:N's workers reported EOF: an **accidental serial≠M:N divergence**, the invariant
the whole parity oracle rests on.

> **Correction (2026-07-14 audit):** this entry used to call it "the only known serial≠M:N divergence".
> That was wrong. `std.net` is a **standing, deliberate** one — a socket op on the serial engine returns
> `Err("… requires the --parallel engine")`, so the same TCP program behaves differently on the two
> engines (see §Net). An accepted design fallback, but a divergence, and the map must say so.

The semantics is now **shared stdin** (Go's `os.Stdin` / Python's `sys.stdin`): ONE source, any task may
read it, a line goes to **exactly one** task (never duplicated, never dropped), WHICH task gets it is
**nondeterministic** on both engines, and `None` means genuinely exhausted. The `Empty`-for-tasks rule
was fake determinism protecting the oracle at the user's expense — the same mistake the interactive-CLI
milestone removed from stdout. The oracle bends; the language does not. `Stdin::Empty` survives only as
a legitimate host config (an embedder with no stdin). Killed at every task-entry seam — `swap_ctx`
(field deleted), `spawn_worker` (shares the handle), the netio inline drain (park reverted) — and pinned
by `parity_{spawned,executor}_tasks_share_stdin_exactly_once` (line-multiset, not exact stdout: the
assignment is nondeterministic by design) + the real-binary `task_reads_piped_stdin_{mn,serial}`.
**Lesson for the remaining hunt: an invariant enforced at one seam is not enforced — enumerate every
task-entry path.**

**New v1 limit it introduces:** `read_line`/`input` are deliberately outside `is_blocking` (the off-heap
`OffloadHost::read_line` is `unreachable!`), so a task blocked in a read now **pins an M:N core worker** —
K blocked readers occupy K workers until stdin produces lines. Previously impossible (tasks got instant
EOF). Accepted; offloading stdin reads is its own milestone.

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
