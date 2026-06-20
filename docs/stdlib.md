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
| `print` | `print(...args) -> nil` | Write each argument (any type) to stdout, then a newline. Variadic. |
| `len` | `len(x) -> int` | Length of a `list`, `str`, `bytes`, or `bytearray`. (Types also have a `.len()` method.) |
| `range` | `range(end) -> list[int]` / `range(start, end) -> list[int]` | End-exclusive list of ints. |
| `int` | `int(x) -> int` | Convert from `int`/`float`/`bool`/`str` (parses a string; truncates a float). |
| `float` | `float(x) -> float` | Convert from `float`/`int`/`str`. |
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
| `contains` | `(sub: str) -> bool` | Substring test. |
| `join` | `(xs: list[str]) -> str` | Join `xs` with the receiver as the separator. |
| `encode` | `() -> bytes` | UTF-8 encode. |
| `message` | `() -> str` | Returns self — lets a bare `str` satisfy the `Error` protocol. |

(See `std.str` below for more string helpers: `repeat`, `reverse`, `pad_left`, `replace`, `index_of`, etc.)

### `list[T]`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | |
| `push` | `(x: T) -> nil` | *mutates* — append. |
| `pop` | `() -> Option[T]` | *mutates* — remove & return last (`None` if empty). |
| `reverse` | `() -> nil` | *mutates* — reverse in place. |
| `contains` | `(x: T) -> bool` | |
| `index_of` | `(x: T) -> int` | First index, or `-1`. |
| `concat` | `(other: list[T]) -> list[T]` | Returns a **new** list. |
| `extend` | `(other: list[T]) -> nil` | *mutates* — append all of `other`. |
| `sum` | `() -> T` | Numeric lists only (`int`→`int`). |
| `sort` | `() -> nil` | *mutates* — ascending. Orderable elements (`int`/`float`/`str`) or `Comparable` structs. |
| `sort_by` | `(cmp: fn(T, T) -> int) -> nil` | *mutates* — custom comparator (`<0`, `0`, `>0`). |
| `sort_by_key` | `(key: fn(T) -> K) -> nil` | *mutates* — sort by a derived orderable/`Comparable` key. |
| `map` | `(f: fn(T) -> U) -> list[U]` | Returns a new list. |
| `filter` | `(pred: fn(T) -> bool) -> list[T]` | Returns a new list. |
| `fold` | `(init: U, f: fn(U, T) -> U) -> U` | Left fold. |

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
| `union` / `intersection` / `difference` | `(other: set[T]) -> set[T]` | Return a **new** set. |

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
`abs` is numeric-polymorphic (`int`→`int`, `float`→`float`); the rest take/return `float`.
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
`list_dir(path) -> Result[list[str]]` (sorted names) · `exists(path) -> bool` ·
`is_file(path) -> bool` · `is_dir(path) -> bool` · `size(path) -> Result[int]` ·
`glob(pattern) -> Result[list[str]]` (`*`/`?` in the final path component).

### `std.time`
`now() -> int` (Unix epoch seconds, UTC) · `monotonic() -> float` (seconds, immune to clock changes) ·
`sleep_ms(ms: int) -> nil` · `format(epoch: int) -> str` (`"YYYY-MM-DD HH:MM:SS"`, UTC).

### `std.process`
`cmd(line: str) -> Result[str]` — run `sh -c <line>`, capture stdout; `Err(stderr)` on non-zero exit.
**Security:** `line` is a shell string — never interpolate untrusted input (shell-injection risk).

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
`Match` and `Response` are reserved (program-global) struct names.
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

---

## 5. Pure-Chezzi modules

Written in Chezzi (`std/*.chz`); same `import std.<name>` surface.

### `std.str` — string helpers
`is_empty(s)` · `repeat(s, n)` · `reverse(s)` · `pad_left(s, width, fill)` · `split_lines(s)` ·
`ends_with(s, suffix)` · `index_of(s, sub) -> int` (or `-1`) · `count(s, sub) -> int` ·
`replace(s, old, new)` · `strip_prefix(s, p)` · `strip_suffix(s, p)`.

### `std.cmp` — ordering generics (`Comparable`)
`max[T: Comparable](a, b) -> T` · `min[T: Comparable](a, b) -> T` ·
`clamp[T: Comparable](x, lo, hi) -> T`.

### `std.iter` — list/iterator helpers
`enumerate(xs) -> list[(int, T)]` · `zip(xs, ys) -> list[(A, B)]` · `map(xs, f)` · `filter(xs, pred)` ·
`fold(xs, init, f)` · `reduce(xs, f) -> T` (non-empty) · `take(xs, n)` · `drop(xs, n)` ·
`any(xs, pred) -> bool` · `all(xs, pred) -> bool` · `find(xs, pred) -> Option[T]` ·
`flatten(xss) -> list[T]`.

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
