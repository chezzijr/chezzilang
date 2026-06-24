# Chezzi — Standard library & builtin reference

This is the complete reference for everything callable from Chezzi user code: global builtins,
methods on the built-in types, the runtime types, and the `std.*` modules. Language **syntax** lives
in [`syntax.md`](syntax.md); this file is the **library** surface.

Conventions used below:
- Signatures use Chezzi types: `int`, `float`, `bool`, `str`, `nil`, `list[T]`, `map[K, V]`,
  `set[T]`, `tuple` (`(A, B)`), `bytes`, `bytearray`, `Option[T]`, `Result[T]` / `Result[T, E]`,
  `fn(A) -> B` (function values).
- "*mutates*" means the call changes the receiver in place and returns `nil`; otherwise a method
  returns a fresh value and leaves the receiver untouched.
- `import std.X` then call as `X.func(...)`. Built-in (global) functions and type methods need no import.

---

## 1. Global builtins (no import)

| Function | Signature | Notes |
|----------|-----------|-------|
| `print` | `print(...args, sep=" ", end="\n") -> nil` | Write each argument (any type) to stdout. Variadic. The args are joined by `sep` (default `" "`) and `end` (default `"\n"`) is appended after — both `str` (the only builtin that takes named arguments). `print("a", end="")` emits `a` with no newline (incremental output); `print("a","b", sep="-", end="!")` emits `a-b!`. |
| `len` | `len(x) -> int` | Length of a `list`, `str`, `bytes`, or `bytearray`. (Types also have a `.len()` method.) |
| `range` | `range(end)` / `range(start, end)` / `range(start, end, step) -> list[int]` | End-exclusive list of ints. `step` is a non-zero int: positive counts up, negative counts down (e.g. `range(10, 0, -1)` → `10,9,…,1`). A wrong-direction step or `start == end` gives `[]`; `step == 0` is a recoverable fault. Capped at 10M elements. |
| `int` | `int(x) -> int` | Convert from `int`/`float`/`bool`/`str` (parses a string; truncates a float). Bad string raises (recoverable) — for `None`-on-failure use `s.to_int() -> int?`. |
| `float` | `float(x) -> float` | Convert from `float`/`int`/`str`. Bad string raises — for `None`-on-failure use `s.to_float() -> float?`. |
| `str` | `str(x) -> str` | Stringify an `int`/`float`/`bool` (and more — see the `Stringable` protocol in `syntax.md`). |
| `ord` | `ord(s) -> int` | Unicode codepoint of the first character of `s`. |
| `chr` | `chr(code) -> str` | One-character string for codepoint `code`. |
| `panic` | `panic(msg) -> never` | Raise a recoverable fault (caught by the nearest `recover:`, else aborts). Bottom-typed. |

### Container constructors

| Form | Result | Notes |
|------|--------|-------|
| `list[T]()` / `list(xs)` | `list[T]` | Empty list / convert an iterable to a list. List literal: `[a, b, c]`. |
| `map[K, V]()` / `{}` | `map[K, V]` | Empty map. Map literal: `{k: v, ...}`. |
| `set(...elems)` | `set[T]` | Set from elements. Empty set is `set()` (`{}` is the empty **map**). |
| `bytes(s)` | `bytes` | UTF-8 encode a `str` (same as `s.encode()`). Literal: `b"..."`. |
| `bytearray()` | `bytearray` | Empty growable byte buffer. |

---

## 2. Methods on built-in types

### `str`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | Character (codepoint) count. |
| `upper` / `lower` | `() -> str` | Case-mapped copy. |
| `trim` | `() -> str` | Strip leading/trailing whitespace. |
| `split` | `(sep: str) -> list[str]` | Split on `sep`. |
| `chars` | `() -> list[str]` | One-character strings. |
| `starts_with` | `(prefix: str) -> bool` | |
| `ends_with` | `(suffix: str) -> bool` | Empty suffix is always true. |
| `contains` | `(sub: str) -> bool` | Substring test. |
| `join` | `(xs: list[str]) -> str` | Join `xs` with the receiver as the separator. |
| `replace` | `(old: str, new: str) -> str` | Replace every non-overlapping `old`; empty `old` → unchanged. |
| `repeat` | `(n: int) -> str` | `n <= 0` → `""`. Raises a recoverable `string repeat capacity overflow` fault if `n * len` would exceed allocatable capacity. |
| `reverse` | `() -> str` | Reversed copy (by codepoint). |
| `pad_left` | `(width: int, fill: str) -> str` | Left-pad to `width` codepoints; never shrinks. |
| `index_of` | `(sub: str) -> int` | First **codepoint** index, `-1` if absent, `0` for empty `sub`. |
| `count` | `(sub: str) -> int` | Non-overlapping occurrences; empty `sub` → `0`. |
| `strip` | `() -> str` | Trim alias (strip leading/trailing whitespace). |
| `strip_prefix` | `(p: str) -> str` | Remove `p` from the front if present, else unchanged. |
| `strip_suffix` | `(p: str) -> str` | Remove `p` from the end if present, else unchanged. |
| `split_lines` | `() -> list[str]` | Split on `"\n"`. |
| `to_int` | `() -> int?` | Safe parse (trims first): `Some(n)` or `None` on bad input. |
| `to_float` | `() -> float?` | Safe parse (trims first): `Some(f)` or `None` on bad input. |
| `encode` | `() -> bytes` | UTF-8 encode. |
| `message` | `() -> str` | Returns self — lets a bare `str` satisfy the `Error` protocol. |

The `ends_with`/`replace`/`repeat`/`reverse`/`pad_left`/`index_of`/`count`/`strip_prefix`/`strip_suffix`/`split_lines`
methods are receiver-method aliases of the identically-named `std.str` free fns — `s.replace(a, b)` and
`text.replace(s, a, b)` (after `import std.str as text`) are byte-identical for valid inputs; the free fns
keep working. (One safety divergence: `s.repeat(n)` raises a recoverable capacity-overflow fault for a
huge `n` rather than allocating until it aborts.)

### `list[T]`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | |
| `push` | `(x: T) -> nil` | *mutates* — append. |
| `pop` | `() -> Option[T]` | *mutates* — remove & return last (`None` if empty). |
| `reverse` | `() -> nil` | *mutates* — reverse in place. |
| `contains` | `(x: T) -> bool` | |
| `index_of` | `(x: T) -> int` | First index, or `-1`. |
| `concat` | `(other: list[T]) -> list[T]` | Returns a **new** list. Operator form: `a + b`. |
| `extend` | `(other: list[T]) -> nil` | *mutates* — append all of `other`. |
| `sum` | `() -> T` | Numeric lists only (`int`→`int`). |
| `sort` | `() -> nil` | *mutates* — ascending. Orderable elements (`int`/`float`/`str`) or `Comparable` structs. |
| `sort_by` | `(cmp: fn(T, T) -> int) -> nil` | *mutates* — custom comparator (`<0`, `0`, `>0`). |
| `sort_by_key` | `(key: fn(T) -> K) -> nil` | *mutates* — sort by a derived orderable/`Comparable` key. |
| `map` | `(f: fn(T) -> U) -> list[U]` | Returns a new list. |
| `filter` | `(pred: fn(T) -> bool) -> list[T]` | Returns a new list. |
| `fold` | `(init: U, f: fn(U, T) -> U) -> U` | Left fold. |

`map`/`filter`/`fold` iterate over a **snapshot** of the receiver's elements taken at call time: a
callback that mutates the receiver (e.g. `xs.pop()`/`xs.push(..)`) does not change the iteration
sequence (and never faults). Same as comprehensions and Python `map`/`filter`.

### `map[K, V]`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | |
| `has` | `(key: K) -> bool` | |
| `get` | `(key: K) -> Option[V]` | |
| `keys` | `() -> list[K]` | Insertion order. |
| `values` | `() -> list[V]` | Insertion order. |
| `remove` | `(key: K) -> Option[V]` | *mutates* — returns the removed value, or `None`. |
| `merge` | `(other: map[K, V]) -> map[K, V]` | Returns a **new** map (`other` wins on key clash). |
| `update` | `(other: map[K, V]) -> nil` | *mutates* — merge `other` into self. |

Index a map with `m[k]` (read/write); iterate with `for k, v in m:`.

### `set[T]`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | |
| `has` | `(x: T) -> bool` | |
| `add` | `(x: T) -> nil` | *mutates* — idempotent insert. |
| `remove` | `(x: T) -> bool` | *mutates* — returns whether it was present. |
| `union` / `intersection` / `difference` | `(other: set[T]) -> set[T]` | Return a **new** set. Operator forms: `a \| b` / `a & b` / `a - b`. |

> **Set operators.** `\| & - ^` on two `set[T]` are union / intersection / difference /
> symmetric-difference, identical to the methods above (`^` has no method form). Lists support `+`
> (concat) and `*` (repeat); see [`syntax.md` §4](syntax.md).

### `bytes` (immutable) and `bytearray` (mutable)
| Type | Method | Signature | Notes |
|------|--------|-----------|-------|
| both | `decode` | `() -> str` | UTF-8 decode (recoverable fault on invalid UTF-8). |
| `bytearray` | `len` | `() -> int` | |
| `bytearray` | `push` | `(byte: int) -> nil` | *mutates* — append a byte (0–255). |
| `bytearray` | `pop` | `() -> Option[int]` | *mutates* — remove & return last byte. |

Index either with `b[i]` (byte as `int`); `bytearray` also supports `b[i] = byte`.

---

## 3. Runtime types (concurrency & iteration)

These types come from the language/runtime; see [`concurrency.md`](concurrency.md) for the full model.

### `Channel[T]` — FIFO mailbox
`send(x: T) -> nil` · `try_send(x: T) -> bool` · `recv() -> T` · `try_recv() -> Option[T]` ·
`close() -> nil` · `trip() -> nil` (permanent level-trigger latch) · `len() -> int`.
Iterate received values with `for v in ch:` (ends when closed and drained).

### `Shared[T]` — cross-task shared cell
`get() -> T` · `set(x: T) -> nil` · `update(f: fn(T) -> T) -> nil`.

### `Atomic[T]` — cross-task atomic (numeric `T` for add/sub)
`load() -> T` · `store(x: T) -> nil` · `exchange(x: T) -> T` · `cas(expected: T, new: T) -> bool` ·
`add(x: T) -> T` · `sub(x: T) -> T` (return the **new** value).

### `Executor` — task pool
`submit(task: fn() -> _) -> nil` (detached) · `shutdown() -> nil` (drain) · `shutdown_now() -> nil` (abandon pending).

### `Socket` / `Listener` — from `std.net` (see §4)
- `Socket`: `read(n: int, timeout_ms?: int) -> Result[str]` · `write(s: str, timeout_ms?: int) -> Result[int]` · `close() -> nil`.
- `Listener`: `accept(timeout_ms?: int) -> Result[Socket]` · `addr() -> Result[str]` · `close() -> nil`.

### Iterator cursors & generators
A `.iter()` cursor and a generator value both expose `next() -> Option[T]` and `iter() -> Iterator[T]`
(idempotent — an iterator is its own iterable). See the `Iterator`/`Iterable` protocols and `yield`
in `syntax.md`.

---

## 4. Native modules

Each is `import std.<name>` then `name.func(...)`. Implemented in Rust (`src/native/*.rs`).

### `std.math`
Functions: `abs`, `floor`, `ceil`, `round`, `pow(base, exp)`, `sqrt`, `sin`, `cos`, `tan`,
`asin`, `acos`, `atan`, `atan2(y, x)`, `exp`, `ln`, `log2`, `log10`, `log(value, base)`.
`abs` is numeric-polymorphic (`int`→`int`, `float`→`float`); the rest take/return `float`. A `float`
parameter accepts an `int` argument via one-way `int`→`float` widening — `sqrt(16)` / `floor(2)` are
the same as `sqrt(16.0)` / `floor(2.0)` (the int is converted to a real `f64`; see `syntax.md §3`).
Constants: `math.pi`, `math.e`.

### `std.io`
| Function | Signature | Notes |
|----------|-----------|-------|
| `print` | `(s: str) -> nil` | stdout + newline. |
| `eprint` | `(s: str) -> nil` | stderr + newline. |
| `read_line` | `() -> Option[str]` | Blocking stdin line, newline stripped (`None` at EOF). |
| `read_file` | `(path: str) -> Result[str]` | Whole file as text (≤ 64 MB). |
| `write_file` | `(path: str, contents: str) -> Result[nil]` | Write / overwrite. |

### `std.os`
| Function | Signature | Notes |
|----------|-----------|-------|
| `args` | `() -> list[str]` | Program args (the positionals after the script path). |
| `env` | `(key: str) -> Option[str]` | Environment variable. |
| `getcwd` | `() -> Result[str]` | |
| `exit` | `(code: int) -> never` | Hard, uncatchable halt (status clamped `0..=255`), unwinding past any `recover:`. **Does NOT run `defer`s.** |

### `std.fs`
**Queries:** `list_dir(path) -> Result[list[str]]` (sorted names) · `exists(path) -> bool` ·
`is_file(path) -> bool` · `is_dir(path) -> bool` · `size(path) -> Result[int]` ·
`glob(pattern) -> Result[list[str]]` (`*`/`?` in the final path component).

**Mutations** (all `Result[nil]` — a permission-denied / missing-parent failure is a catchable `Err`,
never a panic):
`mkdir(path) -> Result[nil]` — create a directory **recursively** (like `mkdir -p`: missing parents
are created, an existing dir is a no-op/idempotent) ·
`remove_file(path) -> Result[nil]` — delete a file (`Err` if missing or a directory) ·
`remove_dir(path) -> Result[nil]` — delete an **empty** directory; **non-recursive** (`Err` on a
non-empty dir — there is intentionally no silent `rm -rf`) ·
`rename(from, to) -> Result[nil]` — move/rename a path ·
`copy(from, to) -> Result[nil]` — copy a file's contents (file-only; the byte count is dropped) ·
`append(path, contents) -> Result[nil]` — append a string to a file, creating it if absent and
**never truncating** (complements `std.io.write_file`, which overwrites).

**Limit (v1):** recursive directory removal (`remove_dir_all` / `rm -rf`) is intentionally **not**
provided — `remove_dir` is empty-only to avoid an accidental recursive wipe. Walk + remove in Chezzi
if you need it.

### `std.time`
`now() -> int` (Unix epoch seconds, UTC) · `monotonic() -> float` (seconds, immune to clock changes) ·
`sleep_ms(ms: int) -> nil` · `format(epoch: int) -> str` (`"YYYY-MM-DD HH:MM:SS"`, UTC).

### `std.process`
`cmd(line: str) -> Result[str]` — run `sh -c <line>`, capture stdout; `Err(stderr)` on non-zero exit
(on failure stdout is discarded — use `run` for the full result).
`run(line: str) -> Result[ProcResult]` — run `sh -c <line>` and return the **structured** result:
`struct ProcResult { stdout: str, stderr: str, code: int }`. A non-zero exit is a normal
`Ok(ProcResult)` with `code != 0` (both streams kept); **only a spawn failure** (no such program,
permission denied) is `Err`. A signal-killed process has no exit code and reports `code = -1`.
`run_args(prog: str, args: list[str]) -> Result[ProcResult]` — run `prog` directly with `args` as the
argv vector, **NO shell** — so metacharacters in `args` (`$(...)`, `;`, `&&`, …) are passed literally
and are **injection-safe**. Same `Ok`/`Err` contract as `run`. Prefer `run_args` over `run`/`cmd` when
any argument comes from untrusted input.
All three are blocking subprocess I/O (offloaded under the OS-thread engine). `ProcResult` is a
reserved (program-global) struct name.
**Security:** `cmd`/`run` hand `line` to the shell — never interpolate untrusted input (shell-injection
risk); use `run_args` instead.
**Not yet:** stdin piping, output streaming, per-process env/cwd overrides.

### `std.rand`
Pseudo-random scalars (SplitMix64 PRNG). `seed(n: int) -> nil` (reseed deterministically) ·
`float() -> float` (uniform in `[0, 1)`) · `int(lo: int, hi: int) -> int` (uniform in the half-open
`[lo, hi)`; **faults** `rand.int(lo, hi): hi must be > lo` if `hi <= lo`) · `bool() -> bool`.
The stream auto-seeds from OS entropy on first use; call `seed(n)` to make it reproducible. Draws are
inline CPU (not I/O). Generic collection helpers (`shuffle`/`choice`/`sample`) live in `std.iter` —
the native seam carries only scalars, so it cannot return a generic `list[T]`.
**Limit (not a bug):** the PRNG state is a single process-global, so under `--parallel` *concurrent*
draws from multiple tasks interleave nondeterministically (engines may diverge). *Sequential* draws
are deterministic and byte-identical across all engines once seeded — draw in one task, or guard with a
`Shared`/lock, when you need reproducibility under concurrency.

### `std.regex`
Returns use `struct Match { text: str, start: int, end: int, groups: list[str] }` (byte offsets;
`groups` are capture groups 1..n; a non-participating optional group is `""`).
`is_match(pattern, subject) -> Result[bool]` · `find(pattern, subject) -> Result[Option[Match]]` ·
`find_all(pattern, subject) -> Result[list[Match]]` · `replace_all(pattern, subject, repl) -> Result[str]` ·
`split(pattern, subject) -> Result[list[str]]`. A bad pattern is `Err`. Patterns are ordinary
strings, so a literal backslash is doubled: `"\\d+"`, `"\\."`.

### `std.request`
Returns use `struct Response { status: int, body: str, headers: map[str, str] }` (header names
lowercased). A ≥400 status is **not** an error — the code rides in `Response.status`; only
transport/DNS/TLS failures become `Err`. Blocking (offloaded under the OS-thread engine).
`Match`, `Response`, and `ProcResult` are reserved (program-global) struct names.
`get(url) -> Result[Response]` · `post(url, body) -> Result[Response]` ·
`put(url, body)` · `patch(url, body)` · `delete(url)` · `head(url)` ·
`request(method, url, body, headers: map[str, str]) -> Result[Response]` (method in UPPERCASE).

### `std.net`
Non-blocking TCP (scheduler-aware). `connect(addr: "host:port") -> Socket` ·
`listen(addr: "host:port") -> Listener`. Socket/Listener methods are in §3. See `concurrency.md`.

### `std.ffi`
C-ABI vocabulary for `extern "lib":` blocks (see the FFI section of `syntax.md`).
`null() -> ptr` · `is_null(p: ptr) -> bool`. Also re-exports the fixed-width integer marshalling type
names: `int8`, `int16`, `int32`, `int64`, `uint8`, `uint16`, `uint32`, `uint64`.
(Sync scalar **callbacks** need no `std.ffi` surface — a callback extern param is just a function-typed
param spelled `fn(scalars) -> scalar`; see the FFI section of `syntax.md`.)

**Memory deref (`load_*` / `store_*`).** Read/write the C-owned memory *behind* an opaque `ptr` — for
struct fields, return buffers, event payloads, and C output-params. Each has a base form (byte offset
`0`) and an `_at(p, off)` form that takes a **byte** offset (the `_at` store puts the offset *before*
the value). Loads:

- `load_int(p)` / `load_int_at(p, off) -> int` — C `long`.
- `load_int8`/`load_int16`/`load_int32`/`load_int64` (+ `_at`) `-> int` — sign-extended to `int`.
- `load_uint8`/`load_uint16`/`load_uint32`/`load_uint64` (+ `_at`) `-> int` — zero-extended (a
  `uint64` with the top bit set wraps negative — the documented v1 limit).
- `load_float(p)` / `_at -> float` (C `double`); `load_float32(p)` / `_at -> float` (C `float`, widened).
- `load_bool(p)` / `_at -> bool` (1 byte, nonzero = true).
- `load_ptr(p)` / `_at -> ptr` — deref a `void**` (the result may itself be NULL).
- `load_str(p)` / `_at -> str` — copy a NUL-terminated C string (the buffer is **not** freed; the
  precondition is a well-formed, NUL-terminated string — there is no max-length cap).

Stores mirror every width except `str` (a `store_str` is deferred — an unbounded write into a caller
buffer is a footgun). Each returns `nil`:
`store_int`/`store_int8`..`store_int64`/`store_uint8`..`store_uint64`/`store_float`/`store_float32`/
`store_bool`/`store_ptr` — base form `(p, v)`, `_at` form `(p, off, v)`. Stores write at the value's
**natural C width** (`store_int8` writes one byte only, leaving adjacent bytes untouched).

> **Unsafe surface.** `load_*`/`store_*` read/write *arbitrary* memory through a C-sourced address —
> like Python `ctypes` (`POINTER(c_int)` + `a[0]`), a bad pointer **segfaults**. Chezzi's one
> mitigation ctypes lacks: a `ptr` is **opaque** and cannot be forged from an `int` (only an `extern`
> return, a callback arg, or `ffi.null()` yields one — provenance is C-sourced). The only cheaply
> checkable guard is the **NULL** base pointer: a `load_*`/`store_*` on address `0` returns a
> *recoverable* error (`ffi.<fn>: null pointer`), it does **not** segfault. A dangling, misaligned, or
> out-of-bounds *non-null* pointer is undefined behavior (not detectable — documented limit). These
> builtins are **unix-only** (a non-unix build registers the names but every call errors).

**C-buffer alloc layer (`alloc` / `alloc_zeroed` / `free`).** Allocate raw C-laid-out memory to hand
to a C array/buffer API (`qsort`, `bsearch`, `fread`-into-buffer, …). Fill and read it with the
`store_*`/`load_*` builtins above. Backed by the **libc allocator** (`malloc`/`calloc`/`free`), so a
buffer can be handed to a C fn that itself reallocs/frees it.

- `alloc(nbytes) -> ptr` — `malloc(nbytes)`; the bytes are **garbage** (uninitialized).
- `alloc_zeroed(nbytes) -> ptr` — `calloc`-style; the bytes are **zeroed**.
- `free(p)` — release a buffer; returns `nil`. `free(ffi.null())` is a safe **no-op**.

> **Manual free.** A `ptr` is **never auto-freed** (the same rule as every other `ptr`). The idiom is
> `p := ffi.alloc(n)` then `defer ffi.free(p)`. **Forgetting to free is a leak.** A `nbytes < 0` is a
> recoverable error (`ffi.alloc: negative size`); `malloc`/`calloc` returning NULL for `nbytes > 0` is
> a recoverable `ffi.alloc: out of memory` (not a crash). `nbytes == 0` passes through to `malloc(0)`
> (impl-defined: may be NULL or a unique ptr). **Double-free, use-after-free, or `store_*`/`load_*`
> beyond the allocation are undefined behavior** — the same inherently-unsafe contract as ctypes; there
> is no bounds or lifetime tracking. Unix-only (a non-unix build registers the names but every call
> errors).

Sort a Chezzi list with libc `qsort` (the full composition — alloc + `store_*` + a callback comparator
+ `load_*`):

```chezzi
import std.ffi

extern "libc.so.6":
    fn qsort(base: ptr, n: int, size: int, cmp: fn(ptr, ptr) -> int)

fn cmp(a: ptr, b: ptr) -> int:           # qsort hands two const void* (each an int64 slot)
    x := ffi.load_int64(a)
    y := ffi.load_int64(b)
    if x < y: return -1
    if x > y: return 1
    return 0

data := [5, 2, 9, 1, 7]
buf := ffi.alloc(len(data) * 8)          # one int64 slot per element
defer ffi.free(buf)                      # manual free — never auto-freed
for i in range(len(data)):
    ffi.store_int64_at(buf, i * 8, data[i])
qsort(buf, len(data), 8, cmp)            # sorts in place, calling back into `cmp`
for i in range(len(data)):
    print(ffi.load_int64_at(buf, i * 8)) # 1 2 5 7 9
```

### `std.encoding`
Reversible text codecs. Every function takes a `str` and operates on its **UTF-8 bytes** (like
`bytes(s)` / `s.encode()`); encoders return `str` (infallible), decoders return `Result[str]`
(malformed input — or decoded bytes that aren't valid UTF-8 — is a recoverable `Err`, never a panic).
*All members are pure CPU str transforms (no I/O); they run inline on every engine.*
- base64 (RFC 4648): `base64_encode(s) -> str` / `base64_decode(s) -> Result[str]` (std `+/` alphabet,
  `=` padding) · `base64_encode_url(s) -> str` / `base64_decode_url(s) -> Result[str]` (URL-safe `-_`
  alphabet). The std decoder rejects `-_`; the URL decoder rejects `+/`.
- hex: `hex_encode(s) -> str` (lowercase) · `hex_decode(s) -> Result[str]` (rejects odd length /
  non-hex digits).
- URL percent-encoding (RFC 3986 **component** form): `url_encode(s) -> str` keeps the unreserved set
  `A-Za-z0-9-._~` literal and `%XX`-escapes everything else (uppercase hex) · `url_decode(s) ->
  Result[str]` reverses it. **Strict 3986** — `+` is *not* treated as a space (that's
  `application/x-www-form-urlencoded`, a different scheme).

**Seam limit (deferred, not a bug):** the native FFI seam carries only `str` (no raw-bytes arg/return),
so base64/hex `decode` UTF-8-validate their output and surface non-UTF-8 results as `Err`. Round-tripping
**arbitrary binary** (e.g. an image) back to raw bytes through this surface is therefore not possible
yet — it needs a bytes-arg/bytes-return seam expansion (a separate, larger change). Text round-trips
fully.

### `std.crypto`
Hand-rolled digests (zero dependencies). Each hashes the str's UTF-8 bytes and returns the
lowercase-hex digest as a `str` (always valid UTF-8 → infallible, no `Result`).
`sha256(s) -> str` (FIPS 180-4) · `md5(s) -> str` (RFC 1321).
**Security:** MD5 is **cryptographically broken** — use it only for checksums / legacy interop, never
for passwords, signatures, or integrity against an adversary. *Pure CPU (no I/O); inline on every engine.*

### `std.uuid`
RFC 4122 version-4 (random) UUIDs. `v4() -> str` returns a fresh random UUID as the canonical 36-char
`8-4-4-4-12` lowercase-hex string (version nibble `4`, variant in `8/9/a/b`). `uuid_seed(n: int) -> nil`
reseeds the generator deterministically (for reproducible/golden runs). The generator has its **own**
process-global stream (separate from `std.rand`, auto-seeded from OS entropy), so a `v4()` draw never
perturbs a program's `rand` sequence. *Pure CPU draws (no I/O); inline on every engine.*
**Limit (not a bug, same as `std.rand`):** the stream is a single process-global, so under `--parallel`
*concurrent* `v4()` draws interleave nondeterministically; an EXACT seeded value is reproducible only for
*sequential* draws.

---

## 5. Pure-Chezzi modules

Written in Chezzi (`std/*.chz`); same `import std.<name>` surface.

### `std.str` — string helpers
`is_empty(s)` · `repeat(s, n)` · `reverse(s)` · `pad_left(s, width, fill)` · `split_lines(s)` ·
`ends_with(s, suffix)` · `index_of(s, sub) -> int` (or `-1`) · `count(s, sub) -> int` ·
`replace(s, old, new)` · `strip_prefix(s, p)` · `strip_suffix(s, p)`.

All of these except `is_empty` are also available as receiver methods on `str` (no import needed):
`s.ends_with(x)` ≡ `text.ends_with(s, x)`. See the `str` method table in §2.

### `std.path` — unix path-STRING manipulation
Pure string ops on **unix `/` paths** — **NO filesystem I/O** (that is `std.fs`). Separator policy:
`/` only; there is no Windows `\` handling. Edge-case semantics follow Python `os.path` (basename/
dirname/split/splitext) and Go `path.Clean` (`normalize`). `import std.path` (or `as p`).

| fn | signature | semantics |
| --- | --- | --- |
| `is_abs` | `(p) -> bool` | `p` starts with `/`. `""` → `false`. |
| `is_rel` | `(p) -> bool` | `not is_abs(p)`. |
| `basename` | `(p) -> str` | Final component (after the last `/`), on the **raw** string. A trailing slash yields `""`: `basename("a/b/")` → `""`, `basename("a/b")` → `"b"`, `basename("/")` → `""`, `basename("")` → `""`, `basename("a")` → `"a"`. |
| `dirname` | `(p) -> str` | Everything before the final component; the head's trailing slash is stripped **unless** the head is all slashes. `dirname("a/b")` → `"a"`, `dirname("a/b/")` → `"a/b"`, `dirname("/a")` → `"/"`, `dirname("a")` → `""`, `dirname("/")` → `"/"`, `dirname("")` → `""`. |
| `split` | `(p) -> (str, str)` | `(dirname(p), basename(p))` as a 2-tuple, so `d, b := path.split(p)`. `split("a/b/")` → `("a/b", "")`. |
| `ext` | `(p) -> str` | Final extension of the basename, **including the leading dot**. A leading-dot-only hidden file has **no** ext, and only the basename is inspected: `ext("a/b.tar.gz")` → `".gz"`, `ext("a.txt")` → `".txt"`, `ext("README")` → `""`, `ext(".bashrc")` → `""`, `ext("a.")` → `"."`, `ext("dir.d/file")` → `""`. |
| `stem` | `(p) -> str` | `basename` with its `ext` removed: `stem("a/b.tar.gz")` → `"b.tar"`, `stem(".bashrc")` → `".bashrc"`, `stem("a.txt")` → `"a"`. |
| `with_ext` | `(p, e) -> str` | Replace the final ext with `e`; `e` is normalized to exactly one leading dot when non-empty (`"md"` ≡ `".md"`), `""` strips it: `with_ext("a/b.txt", ".md")` → `"a/b.md"`, `with_ext("a/b", ".md")` → `"a/b.md"`, `with_ext("a/b.txt", "")` → `"a/b"`. |
| `normalize` | `(p) -> str` | Go `path.Clean` lexical clean (no filesystem): collapse `//`, drop `.`, resolve `..` against the preceding real element. `""` → `"."`; leading `..` is **preserved** on a relative path but a `..` past root on an **absolute** path is dropped. `normalize("/")` → `"/"`, `normalize("//")` → `"/"`, `normalize("..")` → `".."`, `normalize("a/b/../c")` → `"a/c"`, `normalize("a/./b")` → `"a/b"`, `normalize("a/b/")` → `"a/b"`, `normalize("./a")` → `"a"`, `normalize("/..")` → `"/"`, `normalize("/a/../../b")` → `"/b"`, `normalize("a/../../b")` → `"../b"`. |
| `join` | `(parts: list[str]) -> str` | **Go `path.Join` style** (NOT Python's absolute-resets-earlier behavior): drop empty parts, join with `/`, then `normalize`. All-empty → `""`: `join(["a","b","c"])` → `"a/b/c"`, `join(["a/","b"])` → `"a/b"`, `join(["","b"])` → `"b"`, `join([])` → `""`, `join(["a","","c"])` → `"a/c"`, `join(["/a","b"])` → `"/a/b"`. |

### `std.cmp` — ordering generics (`Comparable`)
`max[T: Comparable](a, b) -> T` · `min[T: Comparable](a, b) -> T` ·
`clamp[T: Comparable](x, lo, hi) -> T`.

### `std.iter` — list/iterator helpers
`enumerate(xs) -> list[(int, T)]` · `zip(xs, ys) -> list[(A, B)]` · `map(xs, f)` · `filter(xs, pred)` ·
`fold(xs, init, f)` · `reduce(xs, f) -> T` (non-empty) · `take(xs, n)` · `drop(xs, n)` ·
`any(xs, pred) -> bool` · `all(xs, pred) -> bool` · `find(xs, pred) -> Option[T]` ·
`flatten(xss) -> list[T]`.
Random helpers (call `std.rand`; seed via `rand.seed(n)` for reproducibility — these are pure-Chezzi
because the native seam can't return a generic `list[T]`):
`shuffle(xs) -> list[T]` (new randomly-permuted list, Fisher–Yates, non-mutating) ·
`choice(xs) -> Option[T]` (`None` on empty) ·
`sample(xs, k) -> list[T]` (`k` elements without replacement; `k` clamped to `[0, len]`).

### `std.json` — JSON
```chezzi
enum Json:
    Null
    Bool(bool)
    Num(float)
    Str(str)
    Arr(list[Json])
    Obj(map[str, Json])
```
`parse(s) -> Result[Json]` · `stringify(j) -> str` · `is_null(j) -> bool` ·
`as_bool(j) -> Option[bool]` · `as_float(j) -> Option[float]` · `as_int(j) -> Option[int]` ·
`as_str(j) -> Option[str]` · `as_object(j) -> Option[map[str, Json]]` · `as_array(j) -> Option[list[Json]]` ·
`get(j, key) -> Option[Json]` · `at(j, i) -> Option[Json]` · `len(j) -> int`.

For a known shape, `decode[T](s) -> Result[T]` (a generic builtin) deserializes straight into a
struct / `map[str, V]` / `list[T]` / scalar: `Option` fields accept null-or-absent, extra keys are
ignored, and recursive/generic struct targets are rejected (use the `Json` enum for those).

A JSON *literal in Chezzi source* clashes with string interpolation, so use a raw string
(`r"""{"k": 1}"""`, verbatim — preferred) or double the braces (`"{{ }}"`); a bare `{…}` in a normal
string is interpolation.

### `std.ref` — mutable box
```chezzi
struct Ref[T]:
    value: T
```
Construct `Ref(v)`; methods `get() -> T` · `set(v: T) -> nil` · `update(f: fn(T) -> T) -> nil`.

### `std.cancel` — cooperative cancellation & timeouts
`struct Token` with methods `cancelled() -> bool` · `reason() -> str?` · `cancel() -> nil` ·
`done() -> Channel[bool]` (use in `wait:`) · `deadline_at() -> float` · `derive() -> Token` (linked child).
Constructors: `manual() -> Token` · `timeout(ms: int) -> Token` · `derive(parent: Token) -> Token`.
See `concurrency.md` for the cancellation model.

---

> Where this lives: native modules are Rust under `src/native/*.rs`; the pure-Chezzi modules are real
> `.chz` files under `std/`. Built-in type methods and global builtins are dispatched by the checker
> (`src/checker/mod.rs`) and both engines.
