# Chezzi — Standard library & builtin reference

This is the complete reference for everything callable from Chezzi user code: global builtins,
methods on the built-in types, the runtime types, and the `std.*` modules. Language **syntax** lives
in [`syntax.md`](syntax.md); this file is the **library** surface.

Conventions used below:
- Signatures use Chezzi types: `int`, `float`, `bool`, `str`, `nil`, `List[T]`, `Map[K, V]`,
  `Set[T]`, `tuple` (`(A, B)`), `bytes`, `bytearray`, `Option[T]`, `Result[T]` / `Result[T, E]`,
  `fn(A) -> B` (function values).
- "*mutates*" means the call changes the receiver in place and returns `nil`; otherwise a method
  returns a fresh value and leaves the receiver untouched.
- `import std.X` then call as `X.func(...)`. Built-in (global) functions and type methods need no import.

---

## 1. Global builtins (no import)

| Function | Signature | Notes |
|----------|-----------|-------|
| `print` | `print(...args: Any, sep: str = " ", end: str = "\n") -> nil` | Write each argument (any type) to stdout. Variadic — declared as `native fn print(...args: Any, sep, end)` in `std/prelude.chz`; `Any` is the top type so every value is accepted. The args are joined by `sep` (default `" "`) and `end` (default `"\n"`) is appended after — both `str` keyword-only (the only builtin that takes named arguments). `print("a", end="")` emits `a` with no newline (incremental output); `print("a","b", sep="-", end="!")` emits `a-b!`. The **value form** (`p := print`) is a fixed 1-arg call (see `syntax.md`). |
| `range` | `range(end)` / `range(start, end)` / `range(start, end, step) -> List[int]` | End-exclusive list of ints. `step` is a non-zero int: positive counts up, negative counts down (e.g. `range(10, 0, -1)` → `10,9,…,1`). A wrong-direction step or `start == end` gives `[]`; `step == 0` is a recoverable fault. Capped at 10M elements. |
| `int` | `int(x) -> int` | Convert from `int`/`float`/`bool`/`str` (parses a string; truncates a float). Bad string raises (recoverable) — for `None`-on-failure use `s.to_int() -> int?`. |
| `float` | `float(x) -> float` | Convert from `float`/`int`/`str`. Bad string raises — for `None`-on-failure use `s.to_float() -> float?`. |
| `bool` | `bool(x) -> bool` | Truthiness cast (never faults on a scalar). `int`: `0` → `false`, else `true`. `float`: `0.0`/`-0.0` → `false`, `NaN` → `true` (Python parity), else `true`. `bool`: identity. `str`: `""` → `false`, else `true` (non-empty is truthy — **not** a parse, so `bool(" ")` is `true`). |
| `str` | `str(x) -> str` | Stringify an `int`/`float`/`bool` (and more — see the `Stringable` protocol in `syntax.md`). Scalars (`int`/`float`/`bool`/`str`) also intrinsically satisfy the `Stringable` protocol, so `[T: Stringable]` generics accept them. A string NESTED inside a container / struct field / enum payload renders as its Python `repr` — quoted and escaped (`str(["a", "b"])` is `['a', 'b']`) — while a bare string stays its own characters (`str("a")` is `a`). See `syntax.md` §"A nested `str` is quoted". |
| `ord` | `ord(s) -> int` | Unicode codepoint of `s`, which must be exactly **one character** (`ord("é")` → 233; `ord("ab")` faults, like Python). |
| `chr` | `chr(code) -> str` | One-character string for codepoint `code`. |
| `panic` | `panic(msg) -> never` | Raise a recoverable fault (caught by the nearest `recover:`, else aborts). Bottom-typed. |

### Container constructors

| Form | Result | Notes |
|------|--------|-------|
| `List[T]()` / `List()` / `List(xs)` | `List[T]` | Empty list (`List[T]()` pins the element type; bare `List()` is refined from the expected type / first use, like `Set()` — but a *never*-pinned empty is a static error: annotate it) / convert an iterable to a list. List literal: `[a, b, c]`. `List[T](xs)` checks `xs`'s elements against `T`. |
| `Map[K, V]()` / `Map()` / `{}` | `Map[K, V]` | Empty map (`Map[K, V]()` pins the key/value types; bare `Map()` is refined from the expected type / first use — a *never*-pinned empty errors, annotate it). Map literal: `{k: v, ...}`. |
| `Set[T]()` / `Set()` / `Set(xs)` | `Set[T]` | Empty set (`Set[T]()` pins the element type; `{}` is the empty **map**, not a set; a *never*-pinned bare `Set()` errors, annotate it) / set from an iterable. `Set[T](xs)` checks elements against `T`. |
| `bytes(x)` | `bytes` | Convert a `bytes` / `bytearray` / `List[int]` to `bytes`. To UTF-8 encode a `str`, use `s.encode()` (Python's `bytes(str)` also errors without an encoding). Literal: `b"..."`. |
| `bytearray()` | `bytearray` | Empty growable byte buffer. |

### Which failures fault, and why they are not `Result`s

Chezzi uses `Option`/`Result` "so the caller knows it could fail", so it is fair to ask why the calls
below **fault** instead of returning a carrier. They were all audited on 2026-08-17 against the
ancestor that owns them, and each one **matches** it — the reference language raises there too, so a
carrier would be the divergence, not the fix. A second audit round on 2026-08-18 covered the
remaining ~15 functions the first round hadn't reached (range/math/datetime/crypto/concurrency/json/
flag/io/path/ffi) plus two functions that go the *other* direction — they **swallow** an error the
owning ancestor reports — recorded in the short list right after the table. **This table is the
recorded negative result of BOTH rounds: do not re-run either audit, and do not convert an entry to
`Result` without new ancestor evidence.** Every fault below is recoverable (`r := recover: <expr>`)
**except one, called out in its own row**: `ffi.load_*`/`ffi.free` on a bad pointer is a raw C
dereference (SIGSEGV), not a Chezzi fault — nothing for `recover:` to catch. The ones with a carrier
sibling name it. Unless a row names a different ancestor, "measured" means **CPython 3.14** (3.14.7);
the concurrency rows are measured against **Go 1.26** (go1.26.6) instead — Chezzi's concurrency
primitives are Go's, not Python's.

| Chezzi | Fault message | Ancestor (measured) | Carrier alternative |
|--------|---------------|----------------------|---------------------|
| `xs[i]`, `s[i]` out of range | `index 5 out of bounds (len 3)` | `IndexError: list index out of range` / `string index out of range` | — (`first`/`last`/`get`) |
| `m[k]`, key absent | `key not found: 'zz'` | `KeyError: 'zz'` | **`m.get(k) -> Option[V]`** (Python `dict.get` → `None`) |
| `xs.remove_at(i)` out of range | `index 9 out of bounds (len 3)` | `IndexError: pop index out of range` | — |
| `xs.chunk(n)` / `xs.windows(n)`, `n <= 0` | `chunk/window size must be positive, got 0` | `ValueError: n must be at least one` (`itertools.batched`) | — |
| `int(s)` / `float(s)` on a bad string | `int(): cannot parse 'abc' as an integer` | `ValueError: invalid literal for int() with base 10: 'abc'` | **`s.parse_int()`/`parse_float() -> Result`**, `s.to_int()/to_float() -> Option` |
| `chr(code)` out of range | `chr(): -1 is not a valid Unicode codepoint` | `ValueError: chr() arg not in range(0x110000)` | — |
| `ord(s)` on `""` | `ord() of an empty string` | `TypeError: ord() expected a character, but string of length 0 found` | — |
| `ord(s)` on a multi-**character** string | `ord() expects a 1-character str, got 2 characters` | `TypeError: ord() expected a character, but string of length 2 found` | — |
| `s.split("")` | `split: sep must not be empty` | `ValueError: empty separator` | — |
| `b.decode()` on invalid UTF-8 | `invalid UTF-8 in decode()` | `UnicodeDecodeError: 'utf-8' codec can't decode byte 0xff…` | **`b.decode_lossy()`** (Python `decode(errors="replace")`) |
| `rand.int(lo, hi)`, `hi <= lo` | `rand.int(lo, hi): hi must be > lo` | `ValueError: empty range in randint(5, 1)` | — |
| `a / b`, `a % b` with `b == 0` | `division by zero` / `modulo by zero` | `ZeroDivisionError: division by zero` | — |
| `a << n` with `n` out of range | `shift amount -1 out of range (0..64)` | `ValueError: negative shift count` | — |
| integer overflow | `integer overflow in Add` | *(upward divergence — Python ints are arbitrary-precision; Chezzi's i64 faults recoverably rather than wrapping)* | — |
| `crypto.secure_bytes(n)`, `n` out of range | `secure_bytes: n must be >= 0, got -1` | *(deliberate fail-closed — a crypto primitive must not silently produce fewer bytes)* | — |
| `range(a, b, 0)`, step is zero | `range() step cannot be zero` | `ValueError: range() arg 3 must not be zero` | — |
| `range(a, b)`, materialized length > 10,000,000 | `range() length 20000000 exceeds the maximum of 10000000` | *(no ancestor — CPython's `range` is lazy, `range(0, 20_000_000)` is legal; this is a Chezzi resource limit, not a domain error)* | — |
| `math.lcm`/`math.gcd`/`math.abs(int)`, i64 overflow | `integer overflow in lcm` (also `in gcd` / `in abs`) | *(upward divergence — `math.gcd`/`math.lcm`/`abs` are arbitrary-precision in Python; same family as the `integer overflow` row above)* | — |
| `math.trunc(x)`, non-finite or out of i64 range | `trunc(): NaN is out of integer range` / `trunc(): inf is out of integer range` / `trunc(): 9223372036854776000 is out of integer range` | `ValueError: cannot convert float NaN to integer` / `OverflowError: cannot convert float infinity to integer` | — |
| `math.divmod(a, 0)` | `division by zero` | `ZeroDivisionError: division by zero` | — |
| `[...].sum()`, i64 overflow across the fold | `integer overflow in Add` | *(upward divergence — Python's `sum()` is arbitrary-precision; same family as the `integer overflow` row above)* | — |
| `s.repeat(n)` / `std.string.repeat(s, n)`, capacity overflow | `string repeat capacity overflow` | `OverflowError: repeated string is too long` (`"x" * n`) | — |
| `s.pad_left(w, fill)`, empty `fill` | `pad_left: fill must not be empty` | `TypeError: The fill character must be exactly one character long` (`str.ljust`) | — |
| `s.pad_left(w, fill)` / `string.pad_left(s, w, fill)`, huge `w` (capacity overflow) | `string pad capacity overflow` | `MemoryError` (`str.ljust` with a huge width actually tries to allocate) | — |
| `std.string.split(s, "")` / `std.string.rsplit(s, "")` | `split: sep must not be empty` / `rsplit: sep must not be empty` | `ValueError: empty separator` | — (free-fn siblings of the `s.split("")` row above) |
| `crypto.token_hex(n)`, `n` out of range | `token_hex: n must be >= 0, got -1` / `token_hex: n exceeds the 1048576-byte cap, got …` | `ValueError: negative argument not allowed` (`secrets.token_hex`; no upper cap in Python) | — |
| `uuid.v4()`, OS entropy unavailable | *(not measured — no way to force `getrandom`/`/dev/urandom` to fail in this environment)* | `OSError` from `os.urandom` (same entropy source `crypto.secure_bytes`/`token_hex` share) | — |
| `io.read_all()`, non-UTF-8 stdin | `stdin: stream is not valid UTF-8` | `UnicodeDecodeError` (`sys.stdin.read()`) | — (a mid-read I/O error is a separate, rarer fault on the same call). Its siblings `read_line`/`read_char`/`input` return `Option[str]` **and can still fault** — see `Reader` (R2b) below |
| `path.Path.decode()`, invalid UTF-8 | `invalid UTF-8 in decode()` (inherited verbatim from `bytes.decode()` above) | `UnicodeDecodeError` | **`p.str()`** (lossy, U+FFFD-substituted, never faults — Python has no direct equivalent since `os.fsdecode` is also lossy but different substitution) |
| `datetime.days_in_month(y, m)`, `m` outside 1..12 | `days_in_month: month out of range: 13` | `calendar.IllegalMonthError: bad month number 13; must be 1-12` | — |
| `datetime.weekday_name(wd)`, `wd` outside 0..6 | `weekday_name: wd must be 0..6 (0=Sunday), got 7` | `IndexError: list index out of range` (`calendar.day_name[7]`) | — |
| `datetime.fdiv(a, 0)` / `datetime.fmod(a, 0)` | `division by zero` | `ZeroDivisionError: division by zero` | — (thin wrappers over `/`/`%`, same fault as the row above) |
| `datetime.to_uint(s)`, overflow on a long digit string | `integer overflow in Mul` | *(upward divergence — Python's `int()` is arbitrary-precision; same family as the `integer overflow` row above)* | — |
| `Atomic[T].add`/`sub`, `AtomicInt.add`/`sub`, i64 overflow | `integer overflow in Add` / `integer overflow in Sub` | Go 1.26: **wraps**, never panics (`atomic.Int64.Add(MAX, 1)` → `MIN`, measured) — a **stated divergence** for concurrency specifically, justified by Chezzi's language-wide recoverable-overflow rule | — |
| `Channel[T].send(v)`, closed channel | `send on a closed channel` | Go 1.26: `panic: send on closed channel` — identical | `try_send(v) -> bool` |
| `Channel[T].recv()`, closed and drained | `receive on a closed channel` | Go 1.26: `v, ok := <-ch` returns the zero value + `false`, **no panic** — a divergence | **`try_recv() -> Option[T]`** |
| `json.stringify(j)`, depth ceiling | `json.stringify: exceeded max depth` | CPython's `json.dumps` raises `RecursionError` on a tree built past its recursion limit | — |
| `json.stringify(j)`, `Json.Num` holding NaN/±inf | `cannot serialize non-finite float to JSON` | CPython's `json.dumps` emits a bare `NaN`/`Infinity` (invalid JSON, no error); Go's `json.Marshal` errors `json: unsupported value: NaN` (measured) — Chezzi refuses, matching Go | — |
| `fs.get_str`/`get_bool`/`get_int(name)` on an unregistered name | `flag: unregistered str flag --nope` (also `bool`/`int`) | CPython: `AttributeError: 'Namespace' object has no attribute 'nope'` (`argparse`); Go's `flag.Lookup` returns `nil`, no panic, and has no name-keyed typed getter at all — the Go divergence is already noted at `std/flag.chz:19-22` | — |
| `ffi.load_*`/`ffi.load_*_at`/`ffi.free`, a dangling/misaligned/OOB (non-null) `ptr` | **SIGSEGV — not a recoverable Chezzi fault** | `ctypes`/`cgo` are identically unsafe | — (there is no carrier for undefined behavior) |

The last row above is the **one entry in this whole table that is not recoverable**: every other fault
here is catchable with `r := recover: <expr>`, but a bad `ptr` dereference in `std.ffi` is a raw C
memory access — it segfaults the process (or silently corrupts memory) before any Chezzi fault
machinery runs. A reader who has internalised "every fault here is recoverable" from the paragraph
above this table must not carry that belief into FFI.

Two entries go the **opposite** direction — they **swallow** an error the owning ancestor reports,
rather than fault:

* **`net.Socket.close()` / `net.Listener.close()`** return `nil` unconditionally, silently dropping any
  flush error. CPython's `socket.close()` also returns `None` (measured); **Go's `Close() error`
  reports it** (measured, Go 1.26) — a divergence. Contrast `io.Writer.close(self) -> Result[nil]`,
  which gets this right.
* **`os.setenv(key, value)`** returns `nil` unconditionally for any `key`/`value`, including one
  Python/Go would reject (e.g. a key containing `=`) — Chezzi writes an in-process `Map`, not the real
  OS environment, so there is nothing to validate against. Go's `os.Setenv` returns `error` there;
  CPython raises `ValueError: illegal environment variable name` (measured) — both a divergence.

One entry people expect to find here is **not** a fault at all: `",".join(xs)` on a non-`str` element
is a **static type error** in Chezzi (`expected List[str], found List[int]`) where Python raises
`TypeError` at runtime.

The `ord(s)` multi-character row above was a **silent wrong answer** until it was fixed: `ord("ab")`
returned `97` (the first codepoint) with rc=0, where CPython 3.14 raises `TypeError`. The count is
**characters, not bytes** — `ord("é")` is `233` (one char, two bytes) and `ord("éa")` faults.

`chr(code)` is measured aligned on everything except **lone surrogates**: `chr(0xD800)` faults in
Chezzi where CPython returns `'\ud800'`. That is a consequence of Chezzi `str` being UTF-8 (which
cannot represent a lone surrogate), and it is a loud recoverable fault, not a wrong value — the
opposite failure mode from the `ord` defect. Negative and `> 0x10FFFF` both fault in both languages.

`Reader.read_line()`'s non-UTF-8 fault is a fourth kind of case, documented in full in the `std.io`
section below ("`Reader` (R2b)"): there Chezzi is deliberately **louder** than Python, which raises
once and then silently reports EOF over data that is still in the file.

`encoding.query_decode(q)` / `encoding.url_parse(u)` on a malformed percent-escape (e.g. `%FF`, which
is not valid UTF-8) is **not a fault and not a bug**, so it does not belong in the table above:
measured, `query_decode("a=%FF")` keeps the raw `"%FF"` text in the value rather than substituting
anything. CPython's `urllib.parse.parse_qs("a=%FF")` substitutes U+FFFD (`{'a': ['�']}`,
measured) — losing which byte was actually there. Chezzi's `url_parse` also keeps percent-escapes
un-decoded at all (matching CPython's `urlsplit`, which does the same). Chezzi loses less information
than the Python analogue; there is nothing to fix.

---

## 2. Methods on built-in types

### `str`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | Character (codepoint) count. |
| `upper` / `lower` | `() -> str` | Case-mapped copy. |
| `trim` | `() -> str` | Strip leading/trailing whitespace. |
| `split` | `(sep: str) -> List[str]` | Split on `sep`. Yields `separators + 1` pieces, so the empty string splits to a one-element list holding `""` (`"".split(",")` → `[""]`, length 1), matching Python/Go/Rust/JS. An empty `sep` raises a recoverable `split: sep must not be empty` fault (Python `ValueError`; matches `std.string.split`). |
| `chars` | `() -> List[str]` | One-character strings. |
| `starts_with` | `(prefix: str) -> bool` | |
| `ends_with` | `(suffix: str) -> bool` | Empty suffix is always true. |
| `contains` | `(sub: str) -> bool` | Substring test. |
| `join` | `(xs: List[str]) -> str` | Join `xs` with the receiver as the separator. |
| `replace` | `(old: str, new: str) -> str` | Replace every non-overlapping `old`; empty `old` → unchanged. |
| `repeat` | `(n: int) -> str` | `n <= 0` → `""`. Raises a recoverable `string repeat capacity overflow` fault if `n * len` would exceed allocatable capacity. |
| `reverse` | `() -> str` | Reversed copy (by codepoint). |
| `pad_left` | `(width: int, fill: str) -> str` | Left-pad to `width` codepoints; never shrinks (`width` ≤ len → unchanged). A multi-char `fill` is a repeating cycle truncated to fit, so the result is **exactly** `width` codepoints (`"a".pad_left(4, "xy")` → `"xyxa"`). An empty `fill` raises a recoverable `pad_left: fill must not be empty` fault. Raises a recoverable `string pad capacity overflow` fault if the pad would exceed allocatable capacity. |
| `index_of` | `(sub: str) -> int` | First **codepoint** index, `-1` if absent, `0` for empty `sub`. A **sentinel, not a carrier** — see the `-1` hazard under `List.index_of`. |
| `count` | `(sub: str) -> int` | Non-overlapping occurrences; empty `sub` → codepoint length + 1 (`"abc".count("")` → `4`), matching Python/Go/`std.string.count`. |
| `strip` | `() -> str` | Trim alias (strip leading/trailing whitespace). |
| `strip_prefix` | `(p: str) -> str` | Remove `p` from the front if present, else unchanged. |
| `strip_suffix` | `(p: str) -> str` | Remove `p` from the end if present, else unchanged. |
| `split_lines` | `() -> List[str]` | Split on `"\n"`. |
| `to_int` | `() -> int?` | Safe parse (trims first): `Some(n)` or `None` on bad input. |
| `to_float` | `() -> float?` | Safe parse (trims first): `Some(f)` or `None` on bad input. |
| `parse_int` | `() -> Result[int, str]` | Result-returning parse (trims first): `Ok(n)` or `Err(msg)` carrying a human-readable parse-error message. The error-message sibling of `to_int`. |
| `parse_float` | `() -> Result[float, str]` | Result-returning parse (trims first): `Ok(f)` or `Err(msg)`. The error-message sibling of `to_float`. |
| `encode` | `() -> bytes` | UTF-8 encode. |
| `message` | `() -> str` | Returns self — lets a bare `str` satisfy the `Error` protocol. |

The `ends_with`/`replace`/`repeat`/`reverse`/`pad_left`/`index_of`/`count`/`strip_prefix`/`strip_suffix`/`split_lines`
methods are receiver-method aliases of the identically-named `std.string` free fns — `s.replace(a, b)` and
`text.replace(s, a, b)` (after `import std.string as text`) are byte-identical for valid inputs; the free fns
keep working. **There is no longer any safety divergence between the two spellings** — as of 2026-08-18
`std.string.repeat` and `std.string.pad_left` *delegate* to their native receiver methods rather than
re-implementing the loop, so a huge `n`/`width` raises the same recoverable `string repeat capacity
overflow` / `string pad capacity overflow` fault either way. Before that, only the native method could
probe the allocator: the free fns grew a string in a `while` loop until the process died (`repeat`) or
hung indefinitely (`pad_left`, measured still running at 15s). `tests/chz/stdlib/fault_contracts_test.chz`
pins both spellings against each other so they cannot drift apart again.

### `List[T]`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | |
| `push` | `(x: T) -> nil` | *mutates* — append. |
| `pop` | `() -> Option[T]` | *mutates* — remove & return last (`None` if empty). |
| `reverse` | `() -> nil` | *mutates* — reverse in place. |
| `contains` | `(x: T) -> bool` | |
| `index_of` | `(x: T) -> int` | First index, or `-1`. A **sentinel, not a carrier** — see the `-1` hazard below. |
| `concat` | `(other: List[T]) -> List[T]` | Returns a **new** list. Operator form: `a + b`. |
| `extend` | `(other: List[T]) -> nil` | *mutates* — append all of `other`. |
| `sum` | `() -> T` | Numeric lists (`int`→`int`), or a list of a **scalar numeric `newtype`** — `List[Cents]` (`newtype Cents = int`) sums to `Cents`, and an **empty** one to `Cents(0)`, matching Go's `type Cents int`. Integer sums use checked add — overflow raises a recoverable `integer overflow in Add`, never wraps (the newtype path uses the underlying's same checked op); any-float lists accumulate to `float` (may reach `inf`). A newtype OF a newtype, a generic newtype and a non-numeric one (`newtype Name = str`) are rejected, exactly as their `+` is. |
| `sort` | `() -> nil` | *mutates* — ascending. Orderable elements (`int`/`float`/`str`) or `Comparable` structs. Float `NaN` is handled by a total order (`NaN` sorts to one end), deterministic — never faults. |
| `sort_by` | `(cmp: fn(T, T) -> int) -> nil` | *mutates* — custom comparator (`<0`, `0`, `>0`). |
| `sort_by_key` | `(key: fn(T) -> K) -> nil` | *mutates* — sort by a derived orderable/`Comparable` key. A `NaN` float key sorts deterministically (total order, `NaN` to one end), consistent with `sort()` — never faults. |
| `map` | `(f: fn(T) -> U) -> List[U]` | Returns a new list. |
| `filter` | `(pred: fn(T) -> bool) -> List[T]` | Returns a new list. |
| `fold` | `(init: U, f: fn(U, T) -> U) -> U` | Left fold. |
| `min` / `max` | `() -> Option[T]` | Smallest / largest element by natural order (`int`/`float`/`str` or a `Comparable` struct), wrapped in `Some`. Ties resolve to the first-seen element. Empty list is `None` — never faults (same shape as `first`/`last`/`pop`; unwrap with `??` or `match`). Float `NaN` uses the same total order as `sort()`. |
| `min_by` / `max_by` | `(key: fn(T) -> K) -> Option[T]` | The **element** whose derived key `K` (orderable/`Comparable`) is smallest / largest, wrapped in `Some`; first-seen ties. Empty list is `None`, never a fault. |
| `first` / `last` | `() -> Option[T]` | The first / last element, `None` if empty. Non-mutating. |
| `reversed` | `() -> List[T]` | Returns a **new** reversed list — the receiver is untouched (contrast in-place `reverse`). |
| `insert` | `(i: int, x: T) -> nil` | *mutates* — insert `x` before index `i`. Python-clamped: `i > len` appends, negatives are length-relative and clamp to `0`; never faults. |
| `remove_at` | `(i: int) -> T` | *mutates* — remove & return the element at index `i` (Python-relative negatives). A true out-of-range index **faults** (`index {i} out of bounds (len {n})`). |
| `unique` | `() -> List[T]` | Returns a **new** list with all duplicates removed, first-occurrence order preserved (Python `dict.fromkeys`). Structural equality; never mutates the receiver. |
| `dedup` | `() -> List[T]` | Returns a **new** list collapsing only **consecutive** duplicate runs (Rust `Vec::dedup`) — non-adjacent duplicates survive. |
| `chunk` | `(n: int) -> List[List[T]]` | Consecutive fixed-size chunks (final chunk short if `len` not divisible). `n <= 0` **faults** (`chunk size must be positive, got {n}`). |
| `windows` | `(n: int) -> List[List[T]]` | Sliding windows of size `n` (Rust `slice::windows`). `n > len` yields an **empty** list; `n <= 0` **faults** (`window size must be positive, got {n}`). |
| `take_while` | `(pred: fn(T) -> bool) -> List[T]` | Returns a **new** list of the leading prefix while `pred` holds (stops at the first false). |
| `drop_while` | `(pred: fn(T) -> bool) -> List[T]` | Returns a **new** list of the suffix after the leading prefix where `pred` holds. |
| `count` | `(pred: fn(T) -> bool) -> int` | Number of elements satisfying `pred`. |
| `position` | `(pred: fn(T) -> bool) -> Option[int]` | Index of the **first** element satisfying `pred` (`None` if none). The **carrier** twin of `index_of` — a miss is `None`, not a usable `-1`. |

> **The `-1` miss hazard (`List.index_of`, `str.index_of`).** These two return a **sentinel**, not a
> carrier, and indexing is Python-negative — so `xs[xs.index_of(v)]` on a miss silently yields the
> **last** element instead of faulting. Measured Chezzi: `[1,2,3][[1,2,3].index_of(9)]` → `3`;
> `"hello"["hello".index_of("zz")]` → `'o'`. Python has the identical trap:
> `'hello'['hello'.find('zz')]` → `'o'`. The sentinel therefore **stays** — but reach for the carrier
> when a miss must not be silently usable: `List.position(pred) -> Option[int]`, `Map.get(k) ->
> Option[V]`, or a plain `contains` guard. (Note Python's *list* twin, `list.index(x)`, raises
> `ValueError` instead — the `-1` here is Chezzi applying one uniform shape to `List` and `str`, not
> `List` inheriting Python's.)

The predicate/callback methods — `map`/`filter`/`fold`/`take_while`/`drop_while`/`count`/`position`,
**and `sort`/`sort_by`/`sort_by_key`** — iterate over a **snapshot** of the receiver's elements taken at
call time: a callback that mutates the receiver (e.g. `xs.pop()`/`xs.push(..)`) does not change the
iteration sequence (and never faults). Same as comprehensions and Python `map`/`filter`, **and a
deliberate divergence from Python for the three `sort` variants**, which detect the mutation and raise
`ValueError: list modified during sort`. In Chezzi the pushes/pops a comparator makes are **discarded
when the sort writes the reordered snapshot back**, silently and with `rc=0`:

```chezzi
w := [3, 1, 2]
fn bump(x: int) -> int:
    w.push(100)         # three pushes — all lost
    return x
w.sort_by_key(bump)
print(w)                # [1, 2, 3]   (CPython: ValueError)
```

Keep callbacks pure; if you need both, sort a copy and merge after. (`docs/gaps.md` **W8-4** — the
divergence is filed, not settled: "fault like Python" is the likelier resolution.)

### `Map[K, V]`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | |
| `has` | `(key: K) -> bool` | |
| `get` | `(key: K) -> Option[V]` | |
| `keys` | `() -> List[K]` | Insertion order. |
| `values` | `() -> List[V]` | Insertion order. |
| `remove` | `(key: K) -> Option[V]` | *mutates* — returns the removed value, or `None`. |
| `merge` | `(other: Map[K, V]) -> Map[K, V]` | Returns a **new** map (`other` wins on key clash). |
| `update` | `(other: Map[K, V]) -> nil` | *mutates* — merge `other` into self. |

Index a map with `m[k]` (read/write); iterate with `for k, v in m:`.

### `Set[T]`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | |
| `has` | `(x: T) -> bool` | |
| `add` | `(x: T) -> nil` | *mutates* — idempotent insert. |
| `remove` | `(x: T) -> bool` | *mutates* — returns whether it was present. |
| `union` / `intersection` / `difference` | `(other: Set[T]) -> Set[T]` | Return a **new** set. Operator forms: `a \| b` / `a & b` / `a - b`. |

> **Set operators.** `\| & - ^` on two `Set[T]` are union / intersection / difference /
> symmetric-difference, identical to the methods above (`^` has no method form). Lists support `+`
> (concat) and `*` (repeat); see [`syntax.md` §4](syntax.md).

> **Cyclic keys fault (recoverably).** Map/Set membership and key-equality (`has`, `get`, `remove`,
> `add`, `m[k]`, `in`, set algebra, and `List.contains`/`index_of`/`unique`/`dedup`) are *defined by*
> `==`, so a key holding a genuine reference cycle raises the same recoverable *"maximum structural
> depth (10000) exceeded"* fault that `a == b` does — never a silent wrong `false`. This matches
> Python's `RecursionError` (both `a == b` and `a in s` raise). Catch it with `recover:`.
> The 10 000 is **one budget shared across nested re-entries**, not a fresh allowance per level: a
> user `eq`/`str` that compares or stringifies from inside its own body continues on the depth the
> enclosing walk already consumed, exactly as CPython's single recursion budget does. Without that,
> hook-nesting depth × per-hook walk depth is unbounded and overflows the host stack uncatchably
> (`docs/gaps.md` **W8-43**).

### `bytes` (immutable) and `bytearray` (mutable)
| Type | Method | Signature | Notes |
|------|--------|-----------|-------|
| both | `decode` | `() -> str` | UTF-8 decode (recoverable fault on invalid UTF-8). |
| `bytes` | `decode_lossy` | `() -> str` | UTF-8 decode with each **maximal invalid subsequence** replaced by `U+FFFD` — Python's `b.decode(errors="replace")`, Rust's `String::from_utf8_lossy`. **Never faults**, so it is the DISPLAY twin of `decode` (it is what `path.Path.str()` is built on). Not injective: use `decode` when the exact bytes matter. |
| both | `len` | `() -> int` | Byte count. |
| `bytearray` | `push` | `(byte: int) -> nil` | *mutates* — append a byte (0–255). |
| `bytearray` | `pop` | `() -> Option[int]` | *mutates* — remove & return last byte. |
| `bytearray` | `extend` | `(other: bytes \| bytearray) -> nil` | *mutates* — append all of `other`. |

Index either with `b[i]` (byte as `int`); `bytearray` also supports `b[i] = byte`. **Both types slice**
(`b[1:3]`), and a slice keeps the receiver's type — `bytes[1:3]` is `bytes`, `bytearray[1:3]` is a fresh
`bytearray`. (`syntax.md` §2's type table lists the same surface; the two used to disagree.)

A `bytearray` is **not** assignable to a `bytes` slot (it is mutable — an alias under an immutable
`bytes` type would change under you). Convert with **`bytes(ba)`** (an explicit copy, exactly like
CPython's `bytes(ba)`) — that is how a built-up buffer reaches the binary APIs (`io.write_bytes`,
`crypto.sha256_bytes`, `encoding.base64_encode_bytes`, `Socket.write_bytes`).

---

## 3. Runtime types (concurrency & iteration)

These types come from the language/runtime; see [`concurrency.md`](concurrency.md) for the full model.

> **`import std.concurrency` required for `Shared` / `RwShared` / `Atomic` / `AtomicInt` / `Executor`.** These
> are NOT global builtins — a module must `import std.concurrency` (whole-module licenses all of them) or
> `import Shared from std.concurrency` (per-name) before it can use them; bare use otherwise is an
> `unknown type 'Shared' (import it from std.concurrency: \`import std.concurrency\`)` error. They also
> stay **reserved names** (a user `struct Shared`/`struct Executor` is rejected). `Channel` stays global
> (no import needed). `timer(ms)` now requires **`import std.time`** (whole-module, or `import timer from
> std.time`) — bare use otherwise is an `unknown function 'timer' (import it from std.time: \`import
> std.time\`)` error. `timer` stays a reserved name too (no user `struct timer`/`fn timer`).
>
> **Qualified / aliased path (additive).** Every import-gated native type above is **also** reachable
> by the two-level module-member path, exactly like a `.chz` module type or `regex.Match`: after
> `import std.concurrency`, `concurrency.Shared[int]` / `concurrency.Shared(0)` resolve and construct;
> `import std.concurrency as c` gives `c.Shared(0)`. It works in every position — annotation, ctor call,
> `type S = concurrency.Shared[int]`, `newtype MyS[T] = concurrency.Shared[T]`, method call — and lowers
> to the same value as the bare name. Likewise `net.Socket` / `net.Listener` (`import std.net`) and the
> FFI widths / `ptr` (`import std.ffi`, e.g. `ffi.int32`, valid inside an `extern` signature), except
> those are **type-only**: `net.Socket(...)` is rejected (no from-nothing ctor). `time.timer(ms)` is a
> qualified call; `time.timer` in **type** position is rejected (it is a function). Paths are two-level
> (`concurrency.Shared`, not `std.concurrency.Shared`). The qualified form still requires the `import`
> (qualified access to a non-imported module is an `unknown module` error), so the import gate is
> unchanged; the bare-after-import spelling stays fully supported.

### `Channel[T]` — FIFO mailbox
`Channel[T]()` is an **unbounded** FIFO (`send` never blocks); `Channel[T](cap)` (`cap > 0`) is a
**bounded** FIFO whose `send` **blocks/parks** while `cap` messages are queued and resumes once a `recv`
frees a slot (Go's buffered channel; a full `send` with no possible consumer is a deadlock fault, not an
over-fill). Methods: `send(x: T) -> nil` · `try_send(x: T) -> bool` (`false` = closed **or** full — never
blocks) · `recv() -> T` · `try_recv() -> Option[T]` · `close() -> nil` ·
`trip() -> nil` (permanent level-trigger latch — **`Channel[bool]` only**, gated by `where T: bool`, since
it always delivers `true`; the primitive behind `std.cancel`'s `done()`) · `len() -> int` · `cap() -> int`
(the bound, or `0` for unbounded). Iterate received values with `for v in ch:` (ends when closed and drained). Backpressure only
changes *which* task runs *when*, never the value sequence a consumer sees — bounded channels are
byte-identical across runs.

### `Shared[T]` — cross-task shared cell
`get() -> T` · `set(x: T) -> nil` · `update(f: fn(T) -> T) -> nil`. `get` is a **snapshot copy out**
(the value lives off the GC heap so it can cross threads): mutating it — `s.get().push(x)` — changes a
throwaway, not the box, and is silently lost. Mutate via `update` (or `set` a whole new value). Same for
`RwShared`/`Atomic`; *unlike* a plain in-task `struct` field, whose reads alias the live value but can't cross a spawn.
`update(f)` runs `f` **under the box's exclusive write lock** (read-modify-write is atomic against other
tasks — this is why it exists over a `get`-then-`set`, which races). **Reentrancy limit:** `f` must not
touch the **same** box. Mutate a *different* box, or restructure so the nested step runs after `update`
returns. What actually happens if you do is **three different things, and only one of them tells you** —
measured at the default worker count and again at `CHEZZI_THREADS=2`, identical both times (`docs/gaps.md` **W8-3**):

| from inside `s.update(f)` / `rw.write(f)` | measured |
|---|---|
| `s.get()` (or `rw.read(g)`) | **succeeds, returning the PRE-guard value** — a stale read, no signal |
| `s.set(x)` | **the write is SILENTLY LOST, `rc=0`** — the guard's own return value overwrites it (`Shared(10)` + `set(99)` nested in an `update(+1)` ends at **11**) |
| `s.update(g)` / `rw.write(g)` — the same guard nested | **hangs forever**, and the deadlock detector does **not** report it (it catches its other three shapes in ms) |

Only the third is the documented self-deadlock. The first two are why this is a limit you must respect
rather than one the runtime enforces for you. (`rw.write` nested inside `rw.read` is the one legal
crossing — it takes the write lock and persists.)

### `RwShared[T]` — cross-task read-write cell (many readers OR one writer)
`get() -> T` · `set(x: T) -> nil` · `read(f: fn(T) -> R) -> R` (shared read guard; returns `f`'s
result, no write-back) · `write(f: fn(T) -> T) -> nil` (exclusive write guard; `Shared.update` under
the write lock). Reach for it over `Shared` when reads dominate. Same reentrancy limit as
`Shared.update`, with the same three measured outcomes (see the table above): a nested `write` inside
`write` hangs, a nested `read`/`get` reads the pre-guard value, a nested `set` is silently lost.
Constructed
value-first: `RwShared(v)`; an optional turbofish pins (and is checked against) the element type —
`RwShared[T](v)` (a mismatch like `RwShared[str](0)` is a type error).

**Zero-copy read-view (container element).** Gated by a constructor-kind `where T: List/Map/Set` bound
to the element's HEAD constructor (Tuple **excluded** — heterogeneous):
- `RwShared[List[E]]`: `len() -> int` · `at(i: int) -> Option[E]` (out of range is `None`, never a
  fault — same as `get_key` below and `std.json.at`; negative index normalizes like `xs[i]`. `RwShared`
  has no `[]` of its own, so this is its only read accessor) · `slice(lo: int, hi: int) -> List[E]` ·
  `for_each(f: fn(E) -> _) -> nil` · `fold(init: R, f: fn(R, E) -> R) -> R`.
- `RwShared[Map[K,V]]`: `len() -> int` · `get_key(k: K) -> Option[V]` · `has(k: K) -> bool` ·
  `for_each_entry(f: fn(K, V) -> _) -> nil` · `fold_entries(init: R, f: fn(R, K, V) -> R) -> R`.
- `RwShared[Set[E]]`: `len() -> int` · `contains(e: E) -> bool` · `for_each(f: fn(E) -> _) -> nil` ·
  `fold(init: R, f: fn(R, E) -> R) -> R`.

(`fold*`'s R is inferred from `init`.) These walk the stored value **entry-at-a-time** and materialize
ONE entry at a time, so a worker can scan/reduce a shared large container in **O(1) memory** — instead of
`get`/`read`, which `from_wire`-copy the WHOLE inner into the caller's heap on every access. Reach for
`fold*`/`for_each*` to **reduce in place** when fanning a big shared container out to many workers. Every
walk RE-ACQUIRES the shared read guard **per entry** and drops it before running the callback (and before
any `has`/`get_key`/`contains` hash+eq probe — never held across user code), so a nested read OR write of
the same box — and a GC pass inside the callback — are deadlock-free. Trade-off: the walk is **not one
atomic snapshot** (a concurrent/in-callback `set`/`write` to the same box may be seen mid-walk; use
`read`/`get` for a stable snapshot). Reduce into a **different** box (an `AtomicInt`/local) — the real use
case. On a non-container element (or a Tuple) these methods cleanly report "no method" (checker-gated).
Second trade-off: each piece is copied out **independently**, so two sibling closures over one captured
local do NOT share their binding when pulled out one at a time (two `at()` calls are two crossings); a
whole-container `get()`/`read()`, and `slice` (one call returning a container), are one crossing and do
share — see [`concurrency.md`](concurrency.md) §airlock.

### `Atomic[T]` — cross-task atomic (numeric `T` for add/sub)
`load() -> T` · `store(x: T) -> nil` · `exchange(x: T) -> T` · `cas(expected: T, new: T) -> bool` ·
`add(x: T) -> T` · `sub(x: T) -> T` (return the **new** value).
`cas` compares **structurally**, never through a user `eq` — so a payload type that *reaches* a user
`eq` (its own, or one on any element/entry/field/payload the structural compare recurses into) is a
check-time error (use `Shared[T]`, which has no `cas`); the VM also switches the `eq` hook off for the
compare, so the guarantee holds even where the checker cannot see the witness (a protocol existential
payload). See [`concurrency.md`](concurrency.md).

### `AtomicInt` — monomorphic **lock-free** int atomic
`load() -> int` · `store(x: int) -> nil` · `exchange(x: int) -> int` · `cas(expected: int, new: int) -> bool` ·
`add(x: int) -> int` · `sub(x: int) -> int` (return the **new** value; overflow **faults**, like `+`/`-`).
The monomorphic-int sibling of `Atomic[T]` — no `[T]`, so it is backed by a genuine lock-free
`std::sync::atomic::AtomicI64` (Rust `AtomicI64` / Java `AtomicInteger` / Go `atomic.Int64` style) instead
of a `Mutex`. Reach for it over `Atomic(0)` for a hot int counter/flag under contention (measured ~2.7×
faster than the Mutex-backed `Atomic` on an 8-way counter; see [`benchmarks.md`](benchmarks.md)). Same
import gate + reserved name as `Atomic`. Constructed `AtomicInt(v)` (one int arg; `AtomicInt(3.5)` is a
type error).

### `Executor` — task pool
`submit(task: fn() -> _) -> nil` — **starts the job immediately** on the shared pool (detached,
fire-and-forget), like Python's `ThreadPoolExecutor.submit` ·
`shutdown() -> nil` (**wait** for the submitted work — every job runs; raises the lowest-index fault,
see `concurrency.md` §8) ·
`shutdown_now() -> nil` (drop work that has not started, ask running jobs to stop **cooperatively**,
then wait — Java `shutdownNow`; a job with no cancellation point still finishes, but one **sleeping,
waiting a timer, or parked in a nested `Executor` join is ended** — see `concurrency.md`
§cancellation points) ·
`submit_result[T](f: fn() -> T) -> Channel[T]` — submit `f` and get back a cap-1 `Channel[T]` carrying
its result (`.recv()` it **after** `shutdown()`). This is the result-returning primitive
`std.concurrency.task.submit_task` / `Task[T]` wraps.

**Jobs do not print in submission order, and `shutdown()` does not withhold their output.** Under
`chezzi run` a job's `print` reaches stdout the moment it runs, so concurrent jobs interleave in
completion order — same as `spawn`, same as `ThreadPoolExecutor` (measured: three CPython jobs each
doing real work held submission order in 0 of 30 runs; three jobs that only `print` held it 30/30,
because they are too short to overlap). What submission order *does* fix is which job's fault wins
(`shutdown()` raises the lowest-index one) and the byte order of the **buffered** sink embedders get.
For ordered output, collect results with `submit_result` and print them after the join.
Full statement: `concurrency.md` §"Output ordering".

A job may `shutdown()`/`shutdown_now()` the executor it is itself running under: that join waits for
the executor's *other* jobs and not for itself, and the enclosing join — or, if there is none, the
program-exit join — reduces the slots, so no sibling's output or fault is lost.

An `Executor` is **detached**: it outlives the scope that made it, and the program waits for its
outstanding work at exit. Detached is about **lifetime**, not cancellation — an executor created inside
a *running job* of another executor is still joined at exit, but it inherits that job's cancel, so an
outer `shutdown_now()` stops the nested executor's jobs at their checkpoints too (`concurrency.md`
§cancellation points). That inheritance is **sticky** (a cancelled parent stays cancelled, as in Go),
so a later `submit` to such an executor **faults** — `submit on an Executor whose creating job was
cancelled` — rather than accepting work it would immediately cancel. **Read results after
`shutdown()`, never between it and the `submit`** — in that window a job is already running (eager
`submit`, like CPython's `ThreadPoolExecutor`), so what you observe is a race with it, not a defined
state. The same holds for a `spawn`ed task, which also starts at the `spawn` (Go's `go f()`) rather
than at the nursery's join (`concurrency.md` §4, `future.md` §2c1).

A blocking `recv`/`send`/`wait:` inside a job, or in `main` while jobs are running, **blocks and waits**
— it does not assume "no scheduler means nobody can send". A `deadlock` fault is raised only once the
whole run is stuck: every party blocked, none of their waits satisfiable. That is Go's rule
(`all goroutines are asleep`), so `ex.submit(fn(): ch.recv())` with nobody to send faults in
milliseconds instead of hanging, while `ex.submit(fn(): ch.send(42))` then `ch.recv()` in `main` simply
works. Details and the residual cases in `docs/gaps.md` (`W7-12r / W7-15`) and `future.md` §2d.

### `Socket` / `Listener` — from `std.net` (see §4)
- `Socket`: `read(n: int, timeout_ms?: int) -> Result[str]` · `write(s: str, timeout_ms?: int) -> Result[int]` ·
  `read_bytes(n: int, timeout_ms?: int) -> Result[bytes]` · `write_bytes(b: bytes, timeout_ms?: int) -> Result[int]` ·
  `close() -> nil`.
  `read` is a **`str`-only seam** — it decodes, and it never decodes lossily (no U+FFFD, ever):
  - `n` bounds the NEW bytes taken off the socket. If the previous read ended mid-codepoint, its ≤3-byte
    tail is carried on the socket and prepended here — so `read(n)` can return **up to `n + 3` bytes**,
    and reading valid text in a loop (even `read(1)`) reassembles it **byte-exactly**.
  - **A read blocks until it has at least one whole character.** A chunk that ends mid-codepoint cannot
    be handed to a `str` seam, so the read waits for the rest of it — the same contract as Go's
    `bufio.Reader.ReadRune` and Python's text-mode socket file. The peer owes those 1–3 bytes; the two
    escapes are `timeout_ms` and the peer closing (which errors — see below — rather than dropping the
    tail). `timeout_ms` bounds the **whole call** on every path (a plain read, one that parks, and one
    reached inside a callback like `list.map`): the deadline is fixed when the call starts, so finishing
    a split codepoint never re-arms it. A timed-out read keeps the carried tail for the next read — no
    bytes are lost. **Timeout vs. incomplete-utf-8:** on **every** timeout path (poll-once, the netpoller
    park, and the in-callback demote loop), `Err("timeout")` means *nothing arrived*, but if the call DID
    take 1–3 bytes off the wire that did not complete a character you get `Err("incomplete utf-8: …")`
    instead — a distinct error, because those bytes are retained on the socket; read again to finish the
    character. (`read_bytes`/`write`/`accept` never decode, so their timeouts are always `"timeout"`.)
  - **`read(n, 0)` polls once.** Same classification as above: `Err("timeout")` if nothing arrived,
    `Err("incomplete utf-8: …")` if the poll took a partial character. Both are benign "not ready yet"
    signals for a poll loop.
  - `read(0)` (or a negative / caller-computed-to-zero `n`) is a **no-op** `Ok("")`: it never touches the
    socket, never reports EOF, and leaves any carried tail for the next read. It *does* still report a
    closed socket (`Err("read on a closed socket")`).
  - Two tasks may share one `Socket` (it crosses the airlock as a shared handle): each `read` takes its
    bytes off the socket and decodes them as ONE atomic step, so concurrent readers see wire order (they
    still must not both *block* on it — a second parked op on a shared socket is a fault, unchanged).
  - Bytes that are genuinely not UTF-8 (a **binary payload**) → `Err("invalid utf-8 on the socket: …")`.
    **Nothing is discarded, and the error is sticky:** any valid text that arrived *before* the bad byte
    is delivered first (a normal `Ok`), and the undecodable bytes stay on the socket — so every later
    `read` returns the same `Err` rather than silently eating the stream. A `str` seam can never hand
    those bytes back — switch to `read_bytes` (below), which hands them over byte-exactly.
  - An incomplete codepoint left when the peer closes → `Err("invalid utf-8 at eof: …")`.
  - `close()` returns `nil` (no error channel): a still-carried tail at `close` is dropped silently — the
    EOF error surfaces on the `read` that sees the close, not on `close`.
  - **Binary payloads: use `read_bytes` / `write_bytes`.** They never decode, so any payload survives
    byte-exactly. Contract differences from the `str` `read`: `read_bytes(n)` returns **at most `n`**
    bytes (`read(n)`'s `n` bounds only the NEW fd bytes, so it can return up to `n + 3`); `Ok(b"")` is
    the EOF sentinel; `read_bytes(0)` is a no-op `Ok(b"")` that still errs on a closed socket; and it
    **drains any carried tail first** — including the undecodable bytes a str `read`'s sticky
    `Err("invalid utf-8 …")` refused to deliver, so mixing the two on one socket is lossless.
    `write_bytes` takes a `bytes` (convert a `bytearray` with `bytes(ba)`). `timeout_ms` behaves exactly
    as for `read`/`write`.
- `Listener`: `accept(timeout_ms?: int) -> Result[Socket]` · `addr() -> Result[str]` · `close() -> nil`.
- `Socket`/`Listener` are **reserved type names** (no user `struct Socket`) and a bare annotation
  requires `import std.net` (whole-module, or `import Socket from std.net`) — they are NOT global
  builtins, matching the `Shared`/`Executor` (std.concurrency) and `ptr` (std.ffi) gates.

### Iterator cursors & generators
A `.iter()` cursor and a generator value both expose `next() -> Option[T]` and `iter() -> Iterator[T]`
(idempotent — an iterator is its own iterable). See the `Iterator`/`Iterable` protocols and `yield`
in `syntax.md`.

**Re-entrancy.** A generator cannot be resumed while it is already running: a `.next()` (or a `for`)
on the generator that is *currently executing*, reached from inside its own body, is a recoverable
`generator already running` fault — catchable by `recover:`, never a panic, identical
(Python raises `ValueError: generator already executing`). It is a fault rather than an answer because
a live, non-exhausted generator must never report itself EXHAUSTED (`None`). A generator whose body
**faulted** is *closed*, like Python's: a later `.next()` answers `None`.

---

## 4. Native modules

Each is `import std.<name>` then `name.func(...)`. Implemented in Rust (`src/native/*.rs`).

### Blocking calls cannot be interrupted

The `std.io` file seams, every `std.fs` syscall, every `std.process` runner (`cmd`/`run`/`run_args`/
`run_bytes`/`run_args_bytes`) and every `std.request` call are **offloaded** to a dirty-pool thread so
they never pin an OS-thread worker. The price: a thread sitting inside one has **no cancellation
checkpoint until the call returns**. A sibling's fault (scope cancel), a `cancel.Token`, `os.exit` and
`chezzi test --timeout` therefore do **not** abort a call already in flight — the halt takes effect
when the call returns, not before. Size the operation itself (a subprocess `timeout 5 …`, a request
timeout) if you need a bound.

**Planned:** `std.process` calls will become killable — a cancelled task will signal the child's
process group instead of waiting it out (`docs/concurrency.md` §6h, milestone M25). `std.fs`/`std.io`
will stay uninterruptible, matching Go, which does not cancel file I/O either.

`chezzi test --timeout` consequently reports such a test `TIMED-OUT` **after** it finishes rather than
cutting it short; its message says `… could not be aborted (a blocking … call has no checkpoint until
it returns)`, which is how it reads apart from a real abort.

### `std.math`
Functions: `abs`, `floor`, `ceil`, `round`, `pow(base, exp)`, `sqrt`, `sin`, `cos`, `tan`,
`asin`, `acos`, `atan`, `atan2(y, x)`, `exp`, `ln`, `log2`, `log10`, `log(value, base)`.
`abs` is numeric-polymorphic (`int`→`int`, `float`→`float`); the rest take/return `float`. A `float`
parameter accepts an untyped int CONSTANT via one-way `int`→`float` widening — `sqrt(16)` / `floor(2)`
are the same as `sqrt(16.0)` / `floor(2.0)` (the int is converted to a real `f64`). A **typed** int
value does not adapt: `i := 16; sqrt(i)` is a type error — write `sqrt(float(i))` (see `syntax.md §3`).
`math.round` rounds **half away from zero** (`round(2.5)` → `3`, `round(-2.5)` → `-3`), which differs
from the `:.0f` string-format spec's **banker's rounding** (`"{2.5:.0f}"` → `2`, matching Python) — the
two rounding conventions coexist by design; pick `math.round` for arithmetic, the format spec for display.
Math is **total IEEE-754**: out-of-domain inputs return `NaN`/`inf` instead of faulting —
`sqrt(-1.0)`, `ln(-1.0)`, `asin(2.0)` are all `NaN`; `ln(0.0)` is `-inf`. (`abs` on `int` `MIN`
still overflow-faults — that's integer.)
Predicates (`float -> bool`, IEEE-754 classification): `is_nan(x)`, `is_inf(x)` (±infinity),
`is_finite(x)` (neither `NaN` nor infinite).

Number / integer functions (Python `math` semantics):
- `gcd(a, b) -> int`, `lcm(a, b) -> int` — greatest common divisor / least common multiple.
  `gcd(0, 0)` is `0`; negatives use absolute value (`gcd(-12, 8)` → `4`). `lcm` involving `0` is `0`.
  `lcm` is computed as `|a|/gcd * |b|`; a result that overflows i64 faults (integer, like `abs`).
- `divmod(a, b) -> (int, int)` — the pair `(a / b, a % b)` using Chezzi's own C-style `/` and `%`:
  int `/` **truncates toward zero** and `%` carries the **dividend's** sign — NOT Python's floor
  `divmod`. `divmod(17, 5)` → `(3, 2)`; `divmod(-7, 2)` → `(-3, -1)` (Python gives `(-4, 1)`). `b == 0`
  faults like `a / b`. (A bodied Chezzi fn living alongside the native decls — the hybrid module form.)
- `sign(x)` — numeric-polymorphic like `abs` (`int`→`int`, `float`→`float`); returns `-1`/`0`/`1`
  (numpy/Go convention). `sign(0.0)` is `0.0`, `sign(NaN)` is `NaN`.
- `trunc(x: float) -> int` — truncate toward zero. Equivalent to the `int(x)` builtin; faults on a
  non-finite or out-of-i64-range input (same as `int()`).
- `hypot(x, y) -> float` — `sqrt(x*x + y*y)`. `cbrt(x) -> float` — real cube root (total; `cbrt(-8.0)` → `-2.0`).
- `factorial(n) -> Result[int]`, `comb(n, k) -> Result[int]`, `perm(n, k) -> Result[int]` — return a
  clean `Err` (never a fault) on a bad domain (negative `n`/`k`) or i64 overflow. `factorial` tops out
  at `20!` (`21!` exceeds i64, so it Errs — the ceiling is the i64 limit, not a design choice). `comb`/`perm`
  yield `0` when `k > n` (Python), compute in i128 internally, and Err only when the true result exceeds i64.
- `parse_int_base(s: str, base: int) -> Result[int]` — parse `s` in `base` (`0` or `2..=36`); malformed
  input Errs (never faults). `base 0` auto-detects a `0x`/`0o`/`0b` prefix (else decimal); bases `2`/`8`/`16`
  also accept the matching prefix. A leading `+`/`-` sign is allowed (`parse_int_base("-2a", 16)` → `-42`).

Constants (all `const` — reassigning `math.pi`, or `import pi from std.math; pi = x`, is a type
error naming them const): `math.pi`, `math.e`, `math.inf` (positive infinity), `math.nan` (NaN;
`math.nan != math.nan`).

### `std.io`
The file seams (`read_file`/`write_file`/`read_bytes`/`write_bytes`) are
[uninterruptible while in flight](#blocking-calls-cannot-be-interrupted).

| Function | Signature | Notes |
|----------|-----------|-------|
| `print` | `(s: str) -> nil` | stdout + newline. |
| `eprint` | `(s: str) -> nil` | stderr + newline. |
| `read_line` | `() -> Option[str]` | Blocking stdin line, newline stripped (`None` at EOF). |
| `read_all` | `() -> str` | Drain **all** remaining stdin to EOF as one `str` (Python `sys.stdin.read()`); `""` at a clean EOF. Shares the one stdin source with `read_line` (a later read then sees EOF). Non-UTF-8 stdin is a **fault** — there is no stdin `read_bytes` hatch. |
| `read_char` | `() -> Option[str]` | Read one Unicode scalar as a 1-char `str` (Chezzi has no `char` scalar); `None` at a clean EOF, a **fault** on a partial/invalid UTF-8 sequence. |
| `flush` | `() -> nil` | Flush this process's stdout. Effectively a **no-op**: the CLI's stdout is unbuffered (every write, partial line included, is flushed as it is produced) and captured output has nothing to flush. Kept because it is the portable idiom — and it never waits on stdout's consumer, so it cannot stall a task. (For real buffering, wrap `stdout()` in `buffered(...)` and call the *Writer*'s `flush()`.) |
| `input` | `(prompt: str) -> Option[str]` | Print the prompt (no newline), flush, read one line. Exactly `print(prompt, end="") + flush + read_line` (`None` at EOF). |
| `isatty` | `() -> bool` | `true` when **stdout** is a real terminal, `false` when piped/redirected (via `std::io::IsTerminal`). Python `sys.stdout.isatty()` / Go `isatty`. Lets a CLI colorize only when not piped. |
| `isatty_stdin` | `() -> bool` | Same, over **stdin**. |
| `isatty_stderr` | `() -> bool` | Same, over **stderr**. |
| `read_file` | `(p: PathLike) -> Result[str]` | Whole file as text (≤ 64 MB — larger files: stream with `open(...)` → `Reader`). **Decodes UTF-8** — a binary file is an `Err` pointing at `read_bytes`. |
| `write_file` | `(p: PathLike, contents: str) -> Result[nil]` | Write / overwrite. |
| `read_bytes` | `(p: PathLike) -> Result[bytes]` | Whole file as raw bytes (≤ 64 MB) — binary files. |
| `write_bytes` | `(p: PathLike, data: bytes) -> Result[nil]` | Write / overwrite raw bytes; no size cap, like `write_file`. |
| `create` | `(p: PathLike) -> Result[Writer]` | Open a **truncating** write handle (create-or-truncate). |
| `append` | `(p: PathLike) -> Result[Writer]` | Open an **append** write handle (create-if-absent, never truncates). |
| `stdout` | `() -> Writer` | A fresh write handle over the process stdout sink (same sink as `print`). |
| `stderr` | `() -> Writer` | A fresh write handle over the process stderr sink (same sink as `eprint`). |
| `buffered` | `(w: Writer, size: int = 8192) -> Writer` | Wrap a writer so writes accumulate in-VM and reach the host in **one** call per `flush` / buffer-full / `close` (the Go `bufio.NewWriter` escape hatch; 8 KiB default). |
| `open` | `(p: PathLike) -> Result[Reader]` | Open a **read-only** file handle for line/chunk streaming (past the 64 MB whole-file `read_file` cap). A directory is an `Err` **at the call** (Python `IsADirectoryError`), same message as `read_file` — never an `Ok(Reader)` whose every read fails. |

**`Writer` (R2) — write-only file / stream handle.** A sendable native handle (like `Socket`), the
buffered-output escape hatch Chezzi's unbuffered stdout default was missing. Two openers, not a mode
string (`create` = truncate, `append` = append). Text vs binary is **per-call**. `stdout()`/`stderr()`
route through the *same* sink as `print`/`eprint` (never a raw fd), so a `Writer` over stdout captures,
streams, and parity-checks identically. `buffered(...)` batches host/fd writes.

| Method | Signature | Notes |
|--------|-----------|-------|
| `write` | `(data: str) -> Result[int]` | UTF-8-encode + write; returns bytes written. |
| `write_bytes` | `(data: bytes) -> Result[int]` | Write raw bytes; returns bytes written. Byte-exact on **every** backing — a file, `stdout()`/`stderr()`, or a `buffered` chain over either — so `stdout().write_bytes(b"\xff\xfe")` puts `ff fe` on fd 1, matching Python's `sys.stdout.buffer.write` and Go's `os.Stdout.Write`. |
| `flush` | `() -> Result[nil]` | Drain a `buffered` writer's in-VM buffer **and** flush every core beneath it (one host/fd write per level). On a **file**-backed chain (`buffered(create(p))`, nested `buffered` included) an `Ok` means the bytes are on the fd — visible to an in-process `io.read_file`, a `process.run` child, a sibling process. Like `fs.atomic_write` this is **not** `fsync`'d: observer visibility, **not** crash/power-loss durability. A no-op on unbuffered `stdout`/`stderr`; on a `buffered(stdout())` writer the drained bytes go to the same background stdout queue as `print` (nothing in the program ever waits on that consumer) — `Ok` there means *queued*, not *written*. |
| `close` | `() -> Result[nil]` | Flush (same full-chain guarantee as `flush`) + close the handle. Use-after-close is a clean `Err`, never a fault. |

- **An explicit `flush()`/`close()` on a file-backed buffered writer always persists** (Python
  `open(p,'wb',buffering=n)` / Go `bufio` semantics), including after a write larger than the buffer,
  which drains mid-write.
- **Flushing/writing *through* a handle whose inner writer was closed is a clean `Err`** naming the inner
  (`the inner writer this buffer drains into is closed`) — a flush that persisted nothing never reports
  `Ok`, and `close()` does not mask it either. (`w0 := io.create(p)?; w := io.buffered(w0, 8); w0.close()`
  ⇒ `w.flush()` is that `Err`.)
- **Forgetting `flush`/`close` on a `buffered` writer loses the tail** — Go's footgun. Mitigated
  best-effort: a **file**-backed buffered writer flushes its tail when the handle is dropped (program
  exit / GC), a nested `buffered(buffered(create(p)))` chain included (each level cascades into the one
  below). A **stdout/stderr**-backed buffered writer's tail is *not* recovered on drop — call
  `flush()`/`close()` explicitly. A plain `create`/`append` writer never loses data (its `BufWriter`
  flushes on drop).
- **Cross-task write ordering to one shared `Writer` is unspecified** (Go's `bufio`-not-goroutine-safe
  rule). Each single `write`/`write_bytes` is one atomic critical section, but the *order* of separate
  writes from different tasks is not guaranteed — join and write from one task if you need order.
- Out of scope (v1): seek / random-access.

**`Reader` (R2b) — read-only file handle.** The read twin of `Writer` (same sendable native handle,
opened by `open(path)`): stream a large file line- or chunk-by-chunk instead of slurping it whole (the
64 MB `read_file`/`read_bytes` cap does not apply). Sendable across the airlock like `Writer`.

| Method | Signature | Notes |
|--------|-----------|-------|
| `read_line` | `() -> Option[str]` | **Three** outcomes, not the two the `Option` spells: one line (trailing `\n`, and a preceding `\r`, **stripped**; matches the module-level `read_line()`), `None` at EOF, or a clean **fault** on a mid-read I/O error / non-UTF-8 line, pointing at `read_bytes` (an `Option` can't carry the error, like `read_file`). The non-UTF-8 fault is **non-destructive** — see the carry rule below. |
| `read_bytes` | `(n: int) -> Result[bytes]` | At-most-`n` bytes (exactly `n` until a short final chunk); **empty bytes = EOF**; `Err` on closed / I/O. The binary + error-distinguishing escape hatch. `n <= 0` → `Ok(b"")`. Drains a pending **carry** first, without touching the fd. |
| `close` | `() -> Result[nil]` | Release the fd, and discard any carry. Idempotent; a read after `close` is a clean `Err` (`read_bytes`) / fault (`read_line`), never a panic. |
| `lines` | `() -> Iterator[str]` | **Lazy** line stream — `for ln in r.lines():` (Python `for l in f` / Go `bufio.Scanner` / Rust `BufRead::lines`). A generator over `read_line()`: each line is fetched on demand (the file is **not** snapshotted; an early `break` stops reading), trailing `\n`/`\r` stripped, ends at EOF. A mid-read non-UTF-8 fault surfaces exactly as `read_line`, carry included. |

- **The non-UTF-8 fault is NON-DESTRUCTIVE (W7-9)** — recovery actually works. The line `read_line`
  could not decode is **carried**: its raw bytes, line terminator included, are retained on the reader,
  and `read_bytes` hands them back **byte-exactly** as a carry-only *short* read (the fd is not touched
  until the carry is empty, so `read_bytes(100)` after the fault yields exactly the failed line, and the
  *next* `read_bytes` continues the file). Same rule, same reason as `Socket.read`'s carry: a
  recoverable `Err` that silently drops already-received payload is just a different flavour of data
  loss. Consequences, both deliberate:
  - **Sticky.** While a carry is pending, `read_line` re-decodes it and re-faults — it never skips
    ahead. So `for ln in r.lines():` cannot step over a bad line: drain it with `read_bytes` (or
    `close`) to make progress. `lines()` inherits the carry and the stickiness, being a generator over
    `read_line`.
  - **Self-healing.** A *partial* `read_bytes` that drains the invalid prefix leaves the rest carried;
    if that remainder decodes, the next `read_line` returns it as the line.
  - `close()` discards the carry (closed is closed), and a carry is never served after `close` or
    resurrected after EOF.
  - A **mid-line I/O error** carries too: whatever the read delivered before the error is retained the
    same way, so `read_bytes` gets it back instead of it vanishing with the fault. That carry is
    **not** self-healing — an interrupted line is a *truncated* one, so `read_line` re-raises the I/O
    error until the bytes are drained rather than handing back a fragment as if it were a whole line.

  **Why sticky rather than skip-ahead — the ancestors, measured** (2026-08-17, on
  `b"alpha\nbad\xff\xfebytes\ngamma\n"`). CPython 3.14 `open(p,'r')` is the *worse* of the two options,
  not the better one: the **first** `readline()` raises `UnicodeDecodeError` — `"alpha"` is lost too,
  because `TextIOWrapper` decodes a whole chunk at a time — and every `readline()` after that returns
  `''`, a **silent EOF claim** over `"gamma"`, which is still in the file. Go's `bufio.Scanner` does
  advance (`Scan()` returns true three times, `Text()` gives back `"bad\xff\xfebytes"` verbatim with
  `sc.Err() == nil`), but only because a Go `string` may hold invalid UTF-8; a Chezzi `str` may not, so
  that option is not available here. Chezzi returns `Some("alpha")`, then faults — loudly, repeatedly,
  and with every byte still recoverable through `read_bytes`. That is `correct > silent > wrong`, so the
  stickiness **stays**: it is not a bug to be fixed into a skip.

  ```chezzi
  # /tmp/bin.dat == b"line1\nA\xffB\nline3\n"
  r := io.open("/tmp/bin.dat")?
  r.read_line()                     # Some(line1)
  x := recover: r.read_line()       # Err: stream did not contain valid UTF-8 — read binary files with Reader.read_bytes
  r.read_bytes(100)                 # Ok(b'A\xffB\n')   <- the refused line, byte-exact
  r.read_bytes(100)                 # Ok(b'line3\n')
  ```

- **Cross-task read ordering to one shared `Reader` is unspecified** — two tasks reading one handle race
  the file offset (Go's `bufio`-not-goroutine-safe rule). Each single read is one atomic critical section;
  read from one task if you need order.
- Out of scope (v1): seek / random-access.

**Output contract (`chezzi run`).** The CLI **streams**: output appears when it happens (a prompt before
its read; a long-running program prints incrementally; a killed program keeps what it printed; a spawned
task's line is visible before its nursery joins). Three rules follow:

- One `print(...)` call is **ONE locked write → line-atomic**: two tasks can never garble a single
  `print`'s output. But `print(x, end="")` fragments from two tasks **can** interleave mid-line
  (Python-identical).
- Concurrent tasks' prints interleave **nondeterministically** — cross-task order is NOT a guarantee, on
  either engine. Want ordered output from concurrency? **Join and print the results yourself** (as in
  Python/Go/Rust). (The per-task buffer + task-order flush is a *test-harness* property of the captured
  sink the lib helpers use — not a user-facing guarantee.)
- stdout and stderr are **separately locked**, so a task's `print` and `eprint` may reorder relative to
  each other (Python-identical).
- Output is **unbuffered**: a `print(x, end="")` progress marker appears immediately, without an
  `io.flush()` (that is why `flush` has nothing left to do).
- **Nothing in the program ever waits on stdout's consumer.** A `print` hands the line to a background
  writer thread (one per stream) and returns; `flush` / `read_line` / `input` never wait on that thread
  either. So a stalled/slow consumer (`chezzi run x.chz | (sleep 60; cat)`) can never pin a core worker
  and starve the other tasks.
- A **failed write is not silent**, and the writer thread never decides the program's fate. A closed
  stdout reader (`chezzi run x.chz | head -1`) makes the next `print` raise the ordinary runtime fault
  `stdout closed (broken pipe)` — so an endless printer stops instead of spinning on a dead pipe, and
  the run exits **non-zero** with a trace on stderr (still live: `| head` closes only stdout). Python
  raises `BrokenPipeError` here for the same reason. The halt fires **only where stdout was actually
  written**: the printing job faults, and an `Executor` sibling that never printed runs to completion
  (a file write, a computation) — matching CPython's `ThreadPoolExecutor`, which runs every submitted
  job. A `parallel:`/`spawn` nursery is different **by design**: structured concurrency aborts
  siblings on any fault, broken pipe included (`docs/concurrency.md` §8). A dead stdout deliberately
  does **not** halt via
  the `os.exit` channel: that channel outranks a fault, so borrowing it made a *crashing* program under
  `| head -1` report **exit 0 with no trace**. Any other stdout I/O error (`ENOSPC`, `EIO`, a closed fd)
  additionally prints `chezzi run: cannot write stdout: …`: a truncated redirect never reports success.
  A failure on **stderr** is swallowed — it is a diagnostic channel, and a dead `2>` reader is no reason
  to kill a healthy program.
- **All of the above covers the VM's own sink only.** Bytes an FFI call writes to the descriptor
  itself (`extern "libc.so.6": fn puts`) are outside every guarantee here — not line-atomic against
  `print`, not unbuffered, not ordered with it, and invisible to the broken-pipe halt, so a `| head -1`
  loop of C writes never faults. `ctypes` and `cgo` behave identically (measured); see
  [`syntax.md` §12b](syntax.md).

**Input contract (`read_line` / `read_all` / `read_char` / `input`).** stdin is **ONE source, shared
by every task** — exactly Go's `os.Stdin` and Python's `sys.stdin`. Any task may read it (`spawn:`/
nursery, `Executor.submit`, the entry task); no task is ever handed a false EOF. `read_all` and
`read_char` are siblings of `read_line`: same shared source, same task behavior — `read_all` drains
the whole remainder (so a later read in any task sees EOF), `read_char` consumes one scalar at a time.

- A line goes to **exactly one** task: never duplicated, never dropped.
- **Which** task gets a given line is **nondeterministic** — concurrent readers race
  for lines, like Go/Python. Want a deterministic distribution? Have the entry task read and fan the
  lines out over a `Channel[str]` — the same "order it yourself" answer as concurrent `print`.
- `None` means stdin is **genuinely exhausted** (a real EOF).
- Concurrent `io.input(prompt)` calls may interleave prompt and answer (Python-identical): the prompt
  write and the read are not one atomic unit.
- **v1 limit:** `read_line`/`read_all`/`read_char`/`input` are not offloaded, so a task blocked in a
  read **pins an M:N core worker** — K blocked readers occupy K workers until stdin produces lines.

### `std.os`
| Function | Signature | Notes |
|----------|-----------|-------|
| `args` | `() -> List[str]` | Program args (the positionals after the script path). Decoded **lossily** — see the decoding note. |
| `env` | `(key: str) -> Option[str]` | Environment variable (reads the injected env — see note). |
| `environ` | `() -> Map[str, str]` | ALL environment variables, **sorted by key** (deterministic across runs + engines). Same source as `env`. Keys + values are decoded **lossily** — see the decoding note. |
| `setenv` | `(key: str, value: str) -> nil` | Set an env var. Observed by both `env` and `environ`, and **visible across tasks** (the env map is shared by all M:N workers — process-global, like Python `os.environ` / Go `os.Setenv`). Writes the injected env map — **not** a child's real env; `process.cmd` still inherits the real process env. |
| `getpid` | `() -> int` | Current process id. |
| `platform` | `() -> str` | OS name: `"linux"` / `"macos"` / `"windows"` / … (`std::env::consts::OS`). |
| `hostname` | `() -> Option[str]` | System hostname (`None` on the rare `gethostname` syscall failure). |
| `home_dir` | `() -> Option[str]` | User home (`$HOME`; `None` if unset). Unix-focused. **Stays `str`** — unlike `getcwd`/`temp_dir` it reads the HostConfig env map, which is a deliberately lossy surface (see the argv/env rule above). |
| `temp_dir` | `() -> path.Path` | System temp directory, as **raw OS bytes** wrapped in a [`path.Path`](#pathpath) (W7-8) — same reason as `getcwd`: `$TMPDIR` need not be valid UTF-8, and decoding it would leave a path-returning API that can hand back a name that names nothing. |
| `getcwd` | `() -> Result[path.Path]` | Current working directory (real process cwd), as **raw OS bytes** wrapped in a [`path.Path`](#pathpath) (W7-8) — a non-UTF-8 cwd used to come back `U+FFFD`-substituted, naming nothing. No type argument, no turbofish. `import std.path` to name the type. |
| `chdir` | `(p: PathLike) -> Result[nil]` | Change the **real process cwd** (`Err` on failure). **Process-global** — shared by all M:N workers, so a task's `chdir` shifts sibling tasks' relative paths (Python/Go have the same ceiling). |
| `exit` | `(code: int) -> never` | Hard, uncatchable halt, unwinding past any `recover:`. **Does NOT run `defer`s.** The process status is the **low 8 bits** of `code` (`code & 0xff`), exactly like POSIX `exit(3)` / bash / Python / Go: `os.exit(-1)` → **255**, `os.exit(300)` → **44**, `os.exit(0)` → `0`. (It is a *mask*, not a clamp — a negative code must never report SUCCESS.) |

**Env source:** `env` / `environ` / `setenv` all read/write the engine's injected env config (deterministic + testable). The env map is **shared** across M:N workers (an `Arc<Mutex<…>>`, not a per-worker copy), so a `setenv` from inside a task is visible to the parent + siblings — process-global, matching Python/Go. `environ` sorts by key so its output is deterministic. A `setenv` is **not** seen by a child spawned via `process.cmd` (which inherits the real process env). `getpid` / `platform` / `hostname` / `home_dir` / `temp_dir` are plain queries.

**Non-UTF-8 argv / env (v1 decoding rule):** the OS hands argv and the environment over as raw bytes,
which need not be valid UTF-8. Chezzi `str` is UTF-8, so the CLI decodes both **lossily** at startup —
an invalid byte becomes `U+FFFD` (`args()` returns `"A�B"` where the shell passed `A\xffB`).
This is like Python's `surrogateescape` except it is **not reversible**: the original bytes are gone,
and two raw env keys that decode to the same string collide (last one wins). The guarantee that
matters is that hostile bytes **never crash the CLI** — reading them used to abort the process with a
Rust panic (rc=101) before the program started, where `recover:` could not see it.

<a id="pathpath-input"></a>
**`PathLike` — the path INPUT position (W7-8).** A reserved universe protocol, sole method
`as_path(self) -> bytes`. `str` / `bytes` / `bytearray` satisfy it **intrinsically**; `path.Path`
satisfies it structurally. Every path-taking fn in `std.fs` / `std.io` / `std.os` / `std.path` takes
one, so a raw byte path never has to round-trip through the validated-UTF-8 `str` that cannot
represent it. (This closed the last unswept member of the lossy-byte family — the `fs`/`os` path
DECODE. `argv`/`env` below are a separate, still-lossy surface by design.)

**A path is never taken from a lossy decode.** Because `U+FFFD` substitution is *not* injective, a raw
path `sc\xffipt.chz` and a real file literally named `sc\u{FFFD}ipt.chz` decode to the same string —
opening the alias would silently run a *different* program with rc=0, strictly worse than the panic it
replaced. So any path argument containing `U+FFFD` is **refused** (`cannot use '…' as a path — it
contains U+FFFD …`, rc=1), on `run` / `check` / `ast` / `tokens` / `test`. The check is on the
character, not on the original bytes, so a file *genuinely* named with a literal `U+FFFD` is refused
too — safe direction, and the price of a one-line guard.
**v1 ceiling:** a script whose *path* is not valid UTF-8 therefore cannot be run at all (it fails
cleanly, never a panic, and never runs the wrong file); supporting one needs `OsString` threaded
through the resolver and module graph — its own milestone, tracked in `docs/gaps.md` (W7-6).

### `std.fs`

Every syscall here is [uninterruptible while in flight](#blocking-calls-cannot-be-interrupted).

**Every path argument is a [`PathLike`](#pathpath-input) and every path RESULT is a
[`path.Path`](#pathpath) (W7-8).** A bare `str` literal still works with no annotation and no
turbofish; `bytes`, `bytearray` and `path.Path` work too. The returned `Path` carries the **raw OS
bytes**, so a filename that is not valid UTF-8 round-trips (`fs.exists(fs.list_dir(d)?[0])` is
`true`) instead of coming back `U+FFFD`-substituted and naming nothing.

> **Internal byte seam.** Each path-taking native is declared once, `_`-prefixed and typed `bytes`
> (`_exists`, `_list_dir`, `_read_file`, `_getcwd`, …); the public name is a bodied pure-Chezzi
> wrapper that does `_native(p.as_path())` and re-wraps a returned path into `path.Path`. The `_` is
> **convention only** — there is no privacy mechanism, so `import _exists from std.fs` works. Call the
> public name.

**Queries:** `list_dir(p) -> Result[List[Path]]` (entry names, sorted by raw bytes) ·
`exists(p) -> bool` ·
`is_file(p) -> bool` · `is_dir(p) -> bool` · `size(p) -> Result[int]` ·
`glob(pattern) -> Result[List[Path]]` (`*`/`?` in the final path component; matched over **raw
bytes**, so an ASCII pattern still matches a non-UTF-8 filename — and `?` counts one **Unicode
scalar** wherever the name is valid UTF-8, like Go `filepath.Match` / Python `fnmatch`, falling back
to one byte only for a byte that begins no valid sequence. `*` **matches dotfiles** — Go
`filepath.Glob` semantics, measured: `gt/*` over a dir containing `.hidden` + `visible.txt` returns
both. This differs from Python's `glob.glob`, which excludes dotfile matches from a bare `*` unless
the pattern itself starts with a dot) ·
`canonicalize(p) -> Result[Path]` — resolve symlinks + `.`/`..` against the **real filesystem** to
an absolute real path. Unlike the purely lexical `path.normalize` (no I/O), this hits the filesystem
and so **requires the path to exist** (`Err` on a nonexistent path) ·
`stat(path) -> Result[FileInfo]` — read filesystem metadata into a
`struct FileInfo { size: int, mtime: int, mode: int, is_dir: bool, is_file: bool, is_symlink: bool }`.
`size` is bytes; `mtime` is Unix-epoch **seconds** (`0` if pre-epoch/unsupported); `mode` is the raw
unix `st_mode` (permission + type bits — `0` on non-unix). `stat` **follows symlinks** for
size/mtime/mode/is_dir/is_file (matching `stat`/Python `os.stat`); `is_symlink` is reported separately
(so a symlink-to-file has `is_file == true` **and** `is_symlink == true`). `Err` on a missing/unreadable
path (a broken symlink included). `FileInfo` is **owned by `std.fs`** — read its fields off a returned
value with no import, but to name the type you must `import std.fs` (or `import FileInfo from std.fs`) ·
`walk(p) -> Result[List[Path]]` — recursively list **every** entry (files + dirs) strictly under
`p` as full paths, in a **deterministic** order: each directory's entries are sorted by name,
a directory is listed before its children (pre-order). A **symlinked directory is listed but not
descended** (cycle guard). `Err` on an unreadable root. (The sorted order is required for
determinism.)

**Mutations** (all `Result[nil]` — a permission-denied / missing-parent failure is a catchable `Err`,
never a panic):
`mkdir(path) -> Result[nil]` — create a directory **recursively** (like `mkdir -p`: missing parents
are created, an existing dir is a no-op/idempotent) ·
`remove_file(path) -> Result[nil]` — delete a file (`Err` if missing or a directory) ·
`remove_dir(path) -> Result[nil]` — delete an **empty** directory; **non-recursive** (`Err` on a
non-empty dir — there is intentionally no silent `rm -rf`) ·
`rename(from, to) -> Result[nil]` — move/rename a path ·
`copy(from, to) -> Result[nil]` — copy a file's contents (file-only; the byte count is dropped).
**`Err`s, leaving the file untouched, when `from` and `to` are the SAME FILE** — the same path, or two
names reaching one inode via a symlink or a hardlink (identity is `dev`+`ino`, not a string compare).
The destination is opened truncating, so without the guard a self-copy would silently wipe the file;
Python `shutil.copyfile` raises `SameFileError` and coreutils `cp a a` errors the same way ·
`append(path, contents) -> Result[nil]` — append a string to a file, creating it if absent and
**never truncating** (complements `std.io.write_file`, which overwrites) ·
`chmod(path, mode: int) -> Result[nil]` — set unix permission bits (e.g. `0o755`). **Unix-only** (on a
non-unix target it `Err`s `"chmod is unix-only"`); `mode` is passed unmasked to the OS ·
`atomic_write(path, contents) -> Result[nil]` — write `contents` to a temp file in the **same
directory** as `path`, then `rename` it over `path` (atomic within one filesystem). A concurrent
reader sees either the old contents or the new, never a half-written file, and an existing target's
permission bits are preserved across the swap (a fresh temp would otherwise widen a `0o600` file). It
is **not** `fsync`'d, so this is concurrent-observer atomicity, **not** crash/power-loss durability
(same as `write_file`).

**Limit (v1):** recursive directory removal (`remove_dir_all` / `rm -rf`) is intentionally **not**
provided — `remove_dir` is empty-only to avoid an accidental recursive wipe. `fs.walk` (reverse the
list) + `remove_file`/`remove_dir` in Chezzi if you need it.

### `std.time`
`now() -> int` (Unix epoch seconds, UTC) · `monotonic() -> float` (seconds, immune to clock changes) ·
`sleep_ms(ms: int) -> nil` · `format(epoch: int) -> str` (`"YYYY-MM-DD HH:MM:SS"`, UTC).
Also licenses the opcode-backed `timer(ms) -> Channel[bool]` builtin (one-shot timeout channel; see
[§3](#3-runtime-types-concurrency--iteration) and `concurrency.md §6c`): `import std.time` (whole-module)
or `import timer from std.time` (per-name; `timer` cannot be renamed on import).
Both `sleep_ms` and a `timer(ms)` `recv` are **continuous cancellation checkpoints**: the deadline is
the runtime's own, so a scope cancel or an `Executor.shutdown_now()` ends the wait within ~5 ms instead
of after it, and the task still runs its `defer`s. `chezzi test --timeout` rides the same checkpoint and
reaches every timer wait, including one parked in a `parallel:` nursery with no runnable sibling
(`concurrency.md` §cancellation points; `gaps.md` **W7-16**/**W7-17**). **The same ~5 ms bound covers a
nested `Executor` JOIN** — a job parked in an inner `shutdown()`/`shutdown_now()` observes an outer
cancel and `--timeout` on the join's own poll, not only when its innermost wait happens to be a
checkpoint (measured 2026-08-14: cancel 6 011 ms → **213 ms**, `--timeout=500` 9 010 ms → **509 ms**
over a job running `process.run`, which has no checkpoint of its own; `gaps.md` **W7-60**). What the
bound still cannot cross is the [uninterruptible in-flight call](#blocking-calls-cannot-be-interrupted)
itself: the join unwinds within ~5 ms, the child process it was waiting on runs to completion.

### `std.process`
A running child is [uninterruptible while in flight](#blocking-calls-cannot-be-interrupted) — nothing
in-language kills it; bound it with `timeout N …` in the command line itself.

`cmd(line: str) -> Result[str]` — run `sh -c <line>`, capture stdout; `Err(stderr)` on non-zero exit
(on failure stdout is discarded — use `run` for the full result).
`run(line: str) -> Result[ProcResult]` — run `sh -c <line>` and return the **structured** result:
`struct ProcResult { stdout: str, stderr: str, code: int }`. A non-zero exit is a normal
`Ok(ProcResult)` with `code != 0` (both streams kept); **only a spawn failure** (no such program,
permission denied) is `Err`. A signal-killed process has no exit code and reports `code = -1`.
`run_args(prog: str, args: List[str]) -> Result[ProcResult]` — run `prog` directly with `args` as the
argv vector, **NO shell** — so metacharacters in `args` (`$(...)`, `;`, `&&`, …) are passed literally
and are **injection-safe**. Same `Ok`/`Err` contract as `run`. Prefer `run_args` over `run`/`cmd` when
any argument comes from untrusted input.
**Text vs binary — the `str` seam is a LOSSY VIEW, the bytes twins are exact.** `ProcResult`'s fields
(and `cmd`'s return) are `str`, so `cmd`/`run`/`run_args` decode the child's output as UTF-8 *lossily*:
an undecodable byte is rendered `U+FFFD`. That is deliberate, and it is why the twins exist:
`run_bytes(line: str) -> Result[bytes]` / `run_args_bytes(prog: str, args: List[str]) -> Result[bytes]`
hand back the child's stdout **byte-exactly**. Reach for them for any binary output. Their `Ok`/`Err`
partition is **`cmd`'s, not `run`'s**: `Result[bytes]` has no status channel, so **any failed child is
`Err`** — a non-zero exit (message = the child's stderr, or `command exited with status N` if it wrote
none) as well as a spawn failure. `Ok(bytes)` therefore means "the command succeeded and these are its
bytes"; a failure can never pose as a successful command that printed nothing (the same rule
`request.get_bytes` follows for a non-2xx). A command that legitimately exits non-zero **and** has
meaningful stdout (`grep`, `diff`) belongs on `run`/`run_args`, which carry `code` + both streams (or, on
the shell form, `run_bytes("cmd; exit 0")`). (Why not fail the *text* call the way `Socket.read` does?
`Socket.read` can only afford that because the undecodable bytes stay carried on the socket for
`read_bytes` to return; a finished child has no carry, so Err-ing `run` would DESTROY the captured
stdout, stderr and exit code. Same shape as `request.get`'s lossy `body` + byte-exact
`request.get_bytes`.) The bytes path carries **stdout only** — there is no byte-exact stderr on either
form.
All five are blocking subprocess I/O (offloaded under the OS-thread engine). `ProcResult` is **owned
by `std.process`**: you can read its fields (`.stdout`/`.stderr`/`.code`) off a returned value with no
import, but to name the type or construct it directly (`p: ProcResult` / `ProcResult(...)`) you must
import the module (`import std.process`, then `ProcResult(...)` or qualified `process.ProcResult(...)`;
or `import ProcResult from std.process`). It is **not** a reserved program-global name — a user
`struct ProcResult` (without the import) is your own type.
**Security:** `cmd`/`run` hand `line` to the shell — never interpolate untrusted input (shell-injection
risk); use `run_args` instead.
**Not yet:** stdin piping, output streaming, per-process env/cwd overrides, a bytes-carrying structured
result (binary stdout *plus* stderr *plus* the code in one value).

### `std.rand`
Pseudo-random scalars (SplitMix64 PRNG). `seed(n: int) -> nil` (reseed deterministically) ·
`float() -> float` (uniform in `[0, 1)`) · `int(lo: int, hi: int) -> int` (uniform in the half-open
`[lo, hi)`; **faults** `rand.int(lo, hi): hi must be > lo` if `hi <= lo`) · `bool() -> bool`.
The stream auto-seeds on first use; call `seed(n)` to make it reproducible. Draws are
inline CPU (not I/O). Generic collection helpers (`shuffle`/`choice`/`sample`) live in `std.iter` —
the native seam carries only scalars, so it cannot return a generic `List[T]`.
**Platform (same source as `crypto`):** the auto-seed reads OS entropy on **every unix** — Linux
`getrandom(2)`, `/dev/urandom` elsewhere (macOS, the BSDs, Android, iOS). Only where that read fails
(a non-unix target, or `/dev/urandom` unreadable) does it fall back to a mix of wall-clock nanos, a
static's address and a process counter, which is guessable. The **auto-seed** never faults — like
CPython's `random`, an unseedable stream is still a stream. (The draws themselves can: `rand.int(lo, hi)`
faults on `hi <= lo`, as above.)
**Security:** `std.rand` is **not cryptographically secure** on any platform — the generator is a
64-bit SplitMix64 whose output function is a bijection, so a single observed raw draw recovers the state
*and* the seed and predicts the whole stream in both directions (see `std.uuid`'s **seeded** stream,
which draws from the same step — unseeded `uuid.v4()` does not: it reads the OS CSPRNG per call and
holds no PRNG state). This carries to `iter.shuffle` / `iter.choice` / `iter.sample`, which call `rand.int`. For a
secret — a token, a session id, a shuffle an adversary must not predict — use `crypto.token_hex(n)` /
`crypto.secure_bytes(n)` instead.
**Limit (not a bug):** the PRNG state is a single process-global, so *concurrent* draws from multiple
tasks interleave nondeterministically. *Sequential* draws are deterministic and byte-identical across
runs once seeded — draw in one task, or guard with a
`Shared`/lock, when you need reproducibility under concurrency.

### `std.regex`
Returns use `struct Match { text: str, start: int, end: int, groups: List[str] }` (**codepoint**
offsets, like Python's `re` — so `subject[m.start:m.end] == m.text` holds on non-ASCII input, Chezzi
slicing being codepoint-indexed; `groups` are capture groups 1..n; a non-participating optional group
is `""`).
`is_match(pattern, subject) -> Result[bool]` · `find(pattern, subject) -> Result[Option[Match]]` ·
`find_all(pattern, subject) -> Result[List[Match]]` · `replace_all(pattern, subject, repl) -> Result[str]` ·
`split(pattern, subject) -> Result[List[str]]`. A bad pattern is `Err`.

**Write patterns as RAW strings — `r"\d{4}"`, not `"\\d{4}"`.** Interpolation is always on in a normal
string, so `{4}` is an interpolation **hole**, not a quantifier: `"\\d{4}-\\d{2}"` becomes `\d4-\d2`,
which is still a *valid* regex (a digit then a literal `4`), so it compiles, matches nothing, and
reports no problem anywhere. In `r"…"` a backslash is literal and braces are inert, which is also why
this is the one place the doubled-backslash form (`"\\d+"`, `"\\."`) is a trap rather than a style
choice. (`docs/gaps.md` **W8-1**.)

**The dialect is RE2 (the Rust `regex` crate), not Python's `re`.** Four differences bite in order of
how often:

- **Replacement is `$1` / `${name}`, Rust-style — Python's `\1` is NOT an escape and emits literal
  backslashes with no error.** `replace_all(r"(\d+)-(\d+)", "10-20", r"$2/$1")` → `Ok('20/10')`;
  the same call with `r"\2/\1"` → `Ok('\\2/\\1')`. Silent, `Ok`, plausible-looking. (**W8-6**.)
- **Argument order is `(pattern, subject, repl)`** — the *replacement is last*, the reverse of
  `re.sub(pattern, repl, subject)`. Both are `str`, so swapping them type-checks.
- **No lookaround and no backreferences** (RE2 has linear-time guarantees precisely because it drops
  them): `r"(?<=a)b"` and `r"(a)\1"` each come back `Err("regex parse error: … not supported")`.
  Rewrite with an explicit group + `Match.groups`. **The payoff is no ReDoS**, and it is large: the
  classic catastrophic-backtracking case needs a subject that *fails* to match, so `"a" * 30 + "b"`
  against `r"(a+)+$"` measures **0.000077 s** here and **86.42 s** in CPython `re` — a factor of ~10^6.
  (Against exactly `"a" * 30`, with no trailing `b`, both are instant: the pattern matches and never
  backtracks. Test ReDoS with a non-matching tail or you will measure nothing.)
- **`split` drops capture groups, Python keeps them**: `split(r"(,)", "a,b")` → `Ok(['a', 'b'])` where
  `re.split` gives `['a', ',', 'b']`. Named groups `(?<name>…)` may be *written* but there is no
  read-by-name accessor — index `Match.groups` positionally.

### `std.request`
Returns use `struct Response { status: int, body: str, headers: Map[str, str] }` (header names
lowercased). A ≥400 status is **not** an error — the code rides in `Response.status`; only
transport/DNS/TLS failures become `Err`. Blocking (offloaded under the OS-thread engine) and
[uninterruptible while in flight](#blocking-calls-cannot-be-interrupted).
`Match`, `Response`, and `ProcResult` are **module-owned** struct types (of `std.regex`, `std.request`,
and `std.process` respectively), **not** reserved program-global names. Field access on a returned value
(`.text`/`.status`/`.code`, …) works with **no import**; naming or constructing the type (`m: Match` /
`Match(...)`) requires importing the owning module (whole-module `import std.regex` exposes `Match` bare
and as `regex.Match(...)`; or `import Match from std.regex`). The names are therefore free for user
types — a user `struct Response` without `import std.request` is their own type. But importing the type
**and** declaring a same-named `struct` in the same module is a collision, rejected at check (`type
'Response' is already defined`) — never accept-then-trap.
`get(url, timeout_ms?: int) -> Result[Response]` · `get_bytes(url, timeout_ms?: int) -> Result[bytes]` ·
`post(url, body, timeout_ms?: int) -> Result[Response]` ·
`put(url, body)` · `patch(url, body)` · `delete(url)` · `head(url)` ·
`request(method, url, body, headers: Map[str, str], timeout_ms?: int) -> Result[Response]` (method in UPPERCASE).
The optional trailing `timeout_ms` sets a **per-request total deadline** that overrides the agent's
default caps (connect 10s / read 30s / write 30s) for that one call; `timeout_ms <= 0` or omitted falls
back to the defaults. A timeout (like any transport failure) surfaces as a recoverable `Err`, never a
panic. Build a query string with `std.encoding.query_encode` and compose `url + "?" + query_encode(params)`.
**Binary download:** `get_bytes` fetches the body as raw `bytes` (byte-exact, no UTF-8 decode — the
same immutable `bytes` value `Socket.read_bytes`/`io.read_bytes` return), so an image/zip/pdf survives
where the text `get`'s `Response.body: str` would lossily mangle it. It is GET-only and body-only: unlike
`get` (which models a `>= 400` as a normal `Response` for you to inspect), a non-2xx status is an `Err`
here — so a 404/500 error page can't masquerade as a successful download — and headers are dropped. It
caps a download at 64MB (a larger body is an `Err`); for status/headers on a text response, use `get`.

### `std.net`
Non-blocking TCP (scheduler-aware). `connect(addr: "host:port") -> Result[Socket]` ·
`listen(addr: "host:port") -> Result[Listener]` — **both return `Result`** (bind/DNS/refused failures
are the `Err`); match or `?` them, the bare handle is never handed back. Socket/Listener methods are in
§3. See `concurrency.md`.
The `Socket`/`Listener` TYPE names require `import std.net` to use bare in an annotation (whole-module,
or `import Socket from std.net`) — they are reserved names, not global builtins.
**Text and binary:** `Socket.read -> Result[str]` decodes UTF-8 and never lossily (see §3) — a split
codepoint is carried across reads and reassembled exactly, while a **binary** payload is a clear `Err`
(never silent U+FFFD). For binary, use `Socket.read_bytes` / `write_bytes` (§3): they never decode, and
`read_bytes` drains any carry, so bytes a str `read` refused are recovered rather than stranded.
**`connect` and the engines.** Inside an eager `Executor` job `connect` returns
`Err("connect would block: an Executor job doesn't own its thread — blocking here would starve
every other job and \`parallel:\` nursery sharing the pool. Do this socket op inside \`spawn:\` or a
\`parallel:\` nursery instead, where it parks rather than blocking a shared thread.")` — a job runs
on the bounded, process-wide pool with no scheduler under it, so blocking there steals width from
every other job and every `parallel:` nursery (measured at `CHEZZI_THREADS=1`: a 10 s pin on a
black-hole address). That measurement is **unaffected by `W8-8`** (the `--threads=1` two-runner fix,
2026-08-18) and structurally so — the job pool is `vm::pool`, sized straight off `worker_count()`, while
W8-8's extra runner lived in the nursery enlist/owner path; re-derived on the 1-wide binary the same day,
`Err` returned in 0.006 s, no hang.
**Everywhere else `connect` blocks and succeeds** — a `spawn`/`parallel:` fiber parks on the netpoller,
and top-level `main` blocks its thread, matching both ancestors (CPython `socket.connect` 0.1 ms,
Go `net.Dial` 314 µs, each from the sole/main thread). `connect` is admitted where
`accept`/`read`/`read_bytes`/`write` are not, because those wait on a **Chezzi peer fiber** that can
only run on the very thread they would block, while a `connect` handshake is completed by the
**kernel** and starves no one. The wait honours `--timeout`, cancellation and
`os.exit` (`concurrency.md`; `gaps.md` **W7-40**/**W7-59**).

### `std.ffi`
C-ABI vocabulary for `extern "lib":` blocks (see the FFI section of `syntax.md`).
`null() -> ptr` · `is_null(p: ptr) -> bool`. Also exports the marshalling **type names**: the opaque
pointer handle `ptr` plus the eight fixed-width integers `int8`, `int16`, `int32`, `int64`, `uint8`,
`uint16`, `uint32`, `uint64`. None of these are global builtins — a module that uses `ptr` or a width
type (in an annotation **or an `extern` signature**) must import it from `std.ffi`: whole-module
`import std.ffi` (which also licenses `ptr`) or per-name `import ptr, int32 from std.ffi`. (FFI type
names cannot be renamed on import — the backends key off the literal surface name.)
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
  precondition is a well-formed, NUL-terminated string — there is no max-length cap). The bytes are
  **validated**: a Chezzi `str` is UTF-8, so a non-UTF-8 buffer is a clean **fault** naming the bad
  offset, never a silently mangled string (same contract as `Socket.read`). Read raw/binary bytes
  with `load_uint8_at` instead. The `str`/`owned_str`/`str?` **extern return** paths validate alike.

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
>
> **The contract covers stream lifecycle too.** A C function that writes stdout writes the file
> descriptor directly, so its bytes do not interleave with `print` and the broken-pipe halt cannot see
> them — a `chezzi run x.chz | head -1` loop of C writes never faults, where the same loop of `print`s
> exits in milliseconds. `ctypes` and `cgo` were measured doing exactly the same; the C function's own
> return value is your only error channel. Full contract + runnable examples:
> [`syntax.md` §12b](syntax.md).

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
buf := ffi.alloc(data.len() * 8)         # one int64 slot per element
defer ffi.free(buf)                      # manual free — never auto-freed
for i in range(data.len()):
    ffi.store_int64_at(buf, i * 8, data[i])
qsort(buf, data.len(), 8, cmp)           # sorts in place, calling back into `cmp`
for i in range(data.len()):
    print(ffi.load_int64_at(buf, i * 8)) # 1 2 5 7 9
```

### `std.encoding`
Reversible text codecs. Every function takes a `str` and operates on its **UTF-8 bytes** (like
`s.encode()`); encoders return `str` (infallible), decoders return `Result[str]`
(malformed input — or decoded bytes that aren't valid UTF-8 — is a recoverable `Err`, never a panic).
*All members are pure CPU str transforms (no I/O); they run inline on every engine.*
- base64 (RFC 4648): `base64_encode(s) -> str` / `base64_decode(s) -> Result[str]` (std `+/` alphabet,
  `=` padding) · `base64_encode_url(s) -> str` / `base64_decode_url(s) -> Result[str]` (URL-safe `-_`
  alphabet). The std decoder rejects `-_`; the URL decoder rejects `+/`.
- base64 of **raw bytes** (R1): `base64_encode_bytes(b: bytes) -> str` ·
  `base64_decode_bytes(s: str) -> Result[bytes]` (std alphabet). These do not
  UTF-8-validate, so **arbitrary binary round-trips** (an image, a gzip body). Not added: URL-safe or
  hex bytes twins (say so and they are ~6 lines each).
- hex: `hex_encode(s) -> str` (lowercase) · `hex_decode(s) -> Result[str]` (rejects odd length /
  non-hex digits).
- URL percent-encoding (RFC 3986 **component** form): `url_encode(s) -> str` keeps the unreserved set
  `A-Za-z0-9-._~` literal and `%XX`-escapes everything else (uppercase hex) · `url_decode(s) ->
  Result[str]` reverses it. **Strict 3986** — `+` is *not* treated as a space (that's
  `application/x-www-form-urlencoded`, a different scheme).
- query string builder: `query_encode(params: Map[str, str]) -> str` assembles a `k=v&k2=v2` query
  string — both key and value are percent-encoded with the same `url_encode` escaper. Keys are
  **sorted by their RAW (pre-encoding) value** so the output is deterministic regardless of map
  iteration order (a stable golden). An empty map yields `""` (no leading `?`);
  `{"k": ""}` yields `"k="`. Compose `url + "?" + query_encode(params)`.
- query parser (read-half): `query_decode(q: str) -> Map[str, str]` reverses `query_encode` for
  single-valued keys. Strips one leading `?`; splits on `&` (empty segments skipped); each segment
  splits on the FIRST `=` (a no-`=` segment maps its key to `""`); both key and value are
  percent-decoded with `+` → space (the `x-www-form-urlencoded` rule — note this is *looser* than
  `url_decode`, which leaves `+` literal). DUPLICATE keys are **last-wins** — a `Map[str,str]` cannot
  hold Python `parse_qs` value lists, so this is the Go `url.Values.Get` analog (ceiling). A malformed
  `%`-escape (or non-UTF-8 result) keeps the field's RAW substring — best-effort, never a fault.
- URL splitter (read-half): `url_parse(u: str) -> Map[str, str]` LEXICALLY decomposes a URL into the
  keys `scheme`, `host`, `port`, `path`, `query`, `fragment` (missing components → `""`). It does **not**
  percent-decode the components (matching Python `urlsplit` / Go `net/url` — call `url_decode` /
  `query_decode` on the pieces you need). `port` is a **string** (`""` when absent — the map is
  str→str, the Go `url.Port()` / Python analog). Best-effort, never faults. Ceilings: the last-`:`
  host:port split folds userinfo (`user:pass@host`) and IPv6 (`[::1]:8080`) into `host`, and a `//`-less
  scheme (`mailto:x`) lands the remainder in `path`.

**Seam note:** the `str` members UTF-8-validate their decoded output, so a non-UTF-8 result is an `Err`
(that is the *str* contract, not a limitation). Arbitrary binary round-trips through
`base64_encode_bytes`/`base64_decode_bytes` (R1 widened the native seam to carry raw `bytes`). No
gzip/zlib yet (a new dependency).

### `std.crypto`
Hand-rolled digests + HMAC (zero dependencies). Each `str`-taking fn hashes the str's UTF-8 bytes and
returns the lowercase-hex digest as a `str` (always valid UTF-8 → infallible, no `Result`); the
`_bytes` twins hash raw `bytes` (e.g. `io.read_bytes(p)` → hash a file).
`sha256(s) -> str` / `sha256_bytes(b: bytes) -> str` (FIPS 180-4) ·
`sha1(s) -> str` / `sha1_bytes(b: bytes) -> str` (FIPS 180-4) ·
`sha512(s) -> str` / `sha512_bytes(b: bytes) -> str` (FIPS 180-4) ·
`md5(s) -> str` (RFC 1321) ·
`hmac_sha256(key: bytes, msg: bytes) -> str` (HMAC-SHA-256, RFC 2104 — keyed message authentication;
convert a `str` key/msg with a `b"..."` literal).
**CSPRNG** (Python `secrets`): `secure_bytes(n: int) -> bytes` returns `n` cryptographically-secure
random bytes; `token_hex(n: int) -> str` returns `n` secure random bytes as a `2n`-char lowercase-hex
`str`. Both draw from the OS entropy source (`getrandom(2)`, else `/dev/urandom` — which must be a
character device, so a planted regular file is refused) and **fail closed** — if the OS can't supply
entropy they raise a **recoverable fault** (catchable by `recover:`), never weak or degraded bytes.
`n` must be `0..=1048576` (a 1 MiB cap); a negative or oversized `n` faults. Output is
**non-deterministic** — two draws differ, so it has no fixed golden. (`token_urlsafe` (base64url) is a
deferred follow-up.)
**Platform:** entropy is portable across **unix** — Linux `getrandom(2)`, falling back to
`/dev/urandom` (the same kernel CSPRNG) on macOS, the BSDs, Android, iOS, and on Linux itself if
`getrandom` is unavailable. Only on a **non-unix** target (Windows — already unsupported) do both fns
**fail closed** with a recoverable fault whose message ends `no secure entropy source on this
platform`, rather than falling back to a weaker source.
**Security:** MD5 **and SHA-1** are **cryptographically broken** — use them only for checksums / git
object ids / legacy interop, never for passwords, signatures, or integrity against an adversary.
Password hashing (bcrypt/argon2) is not yet provided.
*Pure CPU / a fast entropy syscall (no blocking I/O); inline on every engine.*

### `std.uuid`
RFC 4122 version-4 (random) UUIDs. `v4() -> str` returns a fresh random UUID as the canonical 36-char
`8-4-4-4-12` lowercase-hex string (version nibble `4`, variant in `8/9/a/b`). `uuid_seed(n: int) -> nil`
switches the stream to a reproducible one (for golden runs).
**By default `v4()` is secure:** it draws 16 fresh bytes from the OS CSPRNG per call — the same source
as `crypto.secure_bytes`, and the same shape as CPython's `uuid.uuid4()`
(`int.from_bytes(os.urandom(16))`) — with no PRNG state at all, so an id is unpredictable, one leaked
id reveals nothing about any other, and a `v4()` draw never perturbs a program's `rand` sequence. It
**fails closed** like `crypto` (a recoverable fault) if the OS can't supply entropy; see `std.crypto`
for the platform matrix. *Inline on every engine (a fast entropy syscall, no blocking I/O).*
**`uuid_seed(n)` opts OUT of that**, permanently: it switches `v4()` to a reproducible 64-bit
SplitMix64 stream, and the switch is **process-global and sticky** — every later `v4()` anywhere in the
process is seeded, there is no way back. Use it only for reproducible/golden runs, e.g.
`examples/uuid_shape.chz`.
**Security (seeded stream only):** the seeded `v4()` is **not cryptographically secure** — the generator
is a 64-bit SplitMix64 whose output function is a bijection, and `v4` overwrites only **6** of the 128
drawn bits (4 version + 2 variant). **ONE observed UUID is enough:** inverting its first half over the
16 version candidates and filtering on the second half's surviving 62 bits leaves exactly one state,
which also **recovers the seed** — so every later UUID *and* every earlier one is predictable
byte-exactly (measured against this repo's own frozen seed-42 vector). One leaked id compromises the
whole seeded stream, so never call `uuid_seed` in a program whose ids are tokens or session ids.
**Limit (not a bug, same as `std.rand`):** the *seeded* stream is a single process-global, so under
`--parallel` *concurrent* `v4()` draws interleave nondeterministically; an EXACT seeded value is
reproducible only for *sequential* draws. (Unseeded draws are independent — no shared state.)

---

## 5. Pure-Chezzi modules

Written in Chezzi (`std/*.chz`); same `import std.<name>` surface.

### `std.string` — string helpers
`is_empty(s)` · `repeat(s, n)` · `reverse(s)` · `pad_left(s, width, fill)` · `split_lines(s)` ·
`ends_with(s, suffix)` · `index_of(s, sub) -> int` (or `-1`) · `count(s, sub) -> int` ·
`replace(s, old, new)` · `strip_prefix(s, p)` · `strip_suffix(s, p)`.

**Ergonomics (free-fn-only — NOT receiver-method aliases, unlike the set above; Python `str` semantics):**
- `capitalize(s) -> str` — first char upper, rest lower (`"hello WORLD"` → `"Hello world"`); `""` unchanged.
- `title(s) -> str` — upper the first cased char of each word, lower the rest; any uncased char (space, digit, apostrophe) is a word boundary (`"they're"` → `"They'Re"`).
- `swapcase(s) -> str` — flip the case of each cased char; uncased chars unchanged.
- `find(s, sub, from_index) -> int` — first codepoint index of `sub` at or after `from_index`, `-1` if absent. Negative `from_index` counts from the end (`len + from_index`, clamped to `0`); `from_index` past the end → `-1` (empty `sub` → `from_index` up to `len`). `index_of(s, sub)` is exactly `find(s, sub, 0)`.
- `split(s, sep, maxsplit = -1) -> List[str]` — split from the left into at most `maxsplit + 1` pieces; `maxsplit < 0` (default) is unlimited. Empty `sep` raises a recoverable `split: sep must not be empty` fault (Python `ValueError`).
- `rsplit(s, sep, maxsplit = -1) -> List[str]` — as `split` but from the RIGHT; unlimited `maxsplit` is identical to `split`. Empty `sep` faults.
- `split_whitespace(s) -> List[str]` — split on runs of whitespace, dropping empty pieces (Python no-arg `str.split()`): `"  a  b "` → `["a", "b"]`, `""` → `[]`.

The case fns are ASCII-guaranteed; exotic full-Unicode case-folding follows Rust (e.g. `ß`→`SS`) and may differ from Python. `split_whitespace`'s blank class is Rust's Unicode `White_Space` (native `trim`), byte-identical to Python on ASCII whitespace.

`is_empty` aside, the FIRST list (`repeat`…`strip_suffix`) is also available as receiver methods on `str` (no import needed): `s.ends_with(x)` ≡ `text.ends_with(s, x)`. See the `str` method table in §2. The ergonomics fns above are `std.string`-only.

### `std.csv` — RFC 4180 CSV read/write (pure Chezzi)
`parse(text: str) -> List[List[str]]` · `format(rows: List[List[str]]) -> str`. A pure-Chezzi module
(no native seam — it is string-scanning over the core `str` primitives).

- `parse` — RFC 4180 quote state machine. Fields separated by `,`, records by CRLF **or** LF (both
  accepted). A double-quote-wrapped field may contain commas, CR, LF and escaped quotes (`""` inside a
  quoted field → one literal `"`). Leading/trailing spaces are significant (never trimmed). A trailing
  record separator produces **no** spurious empty final record. Empty input → `[]`. A blank interior
  line → a single-empty-field record `[""]` (this differs from Python's `csv`, which maps a blank line
  to `[]` — chosen so `parse(format(rows)) == rows` holds).
- **Bare quotes (W7-10).** A `"` opens a quoted field **only at FIELD START**. Anywhere else it is an
  ordinary character kept **literally**: `parse("a,b\"c")` → `[["a", "b\"c"]]`,
  `parse("a,b\"c\"d")` → `[["a", "b\"c\"d"]]`, `parse("a,b\"\"c")` → `[["a", "b\"\"c"]]` — **two**
  literal quotes there, because `""` collapses to one only *inside* a quoted field. This is CPython
  `csv.reader` parity; Go's `bare " in non-quoted-field` **error** was rejected because `parse`
  returns a bare `List[List[str]]` with no error channel (an error would be a signature change). A
  quote that *starts* the field still opens a quoted one, so `parse("a,\"b\"c")` → `[["a", "bc"]]`
  and `parse("\"a\"b,c")` → `[["ab", "c"]]`.
- `format` — the inverse. A field is quoted **iff** it contains a `,`, `"`, CR, or LF; embedded quotes
  are doubled. Each record is **terminated** by CRLF (`\r\n`, per RFC 4180) — not separator-joined —
  so `format([["a","b"]])` == `"a,b\r\n"`; `parse` accepts CRLF or LF either way. `format([])` → `""`.
- **Round-trip guarantee:** `parse(format(rows)) == rows` is **total** — proven for rows covering every
  hard case (embedded comma, escaped quote, embedded newline, empty field, unicode) **including** a sole
  or trailing all-empty record `[""]` (`format([[""]])` == `"\r\n"`, `parse("\r\n")` == `[[""]]`).
  CRLF-*termination* (vs joining) plus parse's "a trailing separator yields no spurious record" rule is
  what makes the empty-record case round-trip.
- **Deferred v1 follow-ups** (YAGNI): streaming/Reader-based parsing, header→`Map` row mapping, and a
  custom-delimiter/TSV `parse_sep(text, sep)`.

### `std.path` — unix lexical path manipulation
Pure lexical ops on **unix `/` paths** — **NO filesystem I/O** (that is `std.fs`). Separator policy:
`/` only; there is no Windows `\` handling. Edge-case semantics follow Python `os.path` (basename/
dirname/split/splitext) and Go `path.Clean` (`normalize`). `import std.path` (or `as p`).

**`PathLike` in, `Path` out.** Every helper takes a `PathLike` (a bare `str` literal, a `bytes`, a
`bytearray`, or another `Path` — no annotation, no turbofish) and returns a **`path.Path`**. The
algorithms operate on the **raw OS bytes**, so a filename that is not valid UTF-8 survives a
`basename`/`join`/`normalize` that a `str`-typed layer could not even represent. Ops therefore
**chain** — `path.with_ext(path.join(parts), "txt")` — and you convert **once at the end**
(`.str()` lossy display / `.decode()` exact / `.bytes()` raw; see [`path.Path`](#pathpath) below).
The table's `-> str` examples show the value of `.str()` on the returned `Path`.

| fn | signature | semantics |
| --- | --- | --- |
| `is_abs` | `(p: PathLike) -> bool` | `p` starts with `/`. `""` → `false`. |
| `is_rel` | `(p: PathLike) -> bool` | `not is_abs(p)`. |
| `basename` | `(p: PathLike) -> Path` | Final component (after the last `/`), on the **raw** string. A trailing slash yields `""`: `basename("a/b/")` → `""`, `basename("a/b")` → `"b"`, `basename("/")` → `""`, `basename("")` → `""`, `basename("a")` → `"a"`. |
| `dirname` | `(p: PathLike) -> Path` | Everything before the final component; the head's trailing slash is stripped **unless** the head is all slashes. `dirname("a/b")` → `"a"`, `dirname("a/b/")` → `"a/b"`, `dirname("/a")` → `"/"`, `dirname("a")` → `""`, `dirname("/")` → `"/"`, `dirname("")` → `""`. |
| `split` | `(p: PathLike) -> (Path, Path)` | `(dirname(p), basename(p))` as a 2-tuple, so `d, b := path.split(p)`. `split("a/b/")` → `("a/b", "")`. |
| `ext` | `(p: PathLike) -> Path` | Final extension of the basename, **including the leading dot**. A leading-dot-only hidden file has **no** ext, and only the basename is inspected: `ext("a/b.tar.gz")` → `".gz"`, `ext("a.txt")` → `".txt"`, `ext("README")` → `""`, `ext(".bashrc")` → `""`, `ext("a.")` → `"."`, `ext("dir.d/file")` → `""`. |
| `stem` | `(p: PathLike) -> Path` | `basename` with its `ext` removed: `stem("a/b.tar.gz")` → `"b.tar"`, `stem(".bashrc")` → `".bashrc"`, `stem("a.txt")` → `"a"`. |
| `with_ext` | `(p: PathLike, e: PathLike) -> Path` | Replace the final ext with `e`; `e` is normalized to exactly one leading dot when non-empty (`"md"` ≡ `".md"`), `""` strips it: `with_ext("a/b.txt", ".md")` → `"a/b.md"`, `with_ext("a/b", ".md")` → `"a/b.md"`, `with_ext("a/b.txt", "")` → `"a/b"`. |
| `normalize` | `(p: PathLike) -> Path` | Go `path.Clean` lexical clean (no filesystem): collapse `//`, drop `.`, resolve `..` against the preceding real element. `""` → `"."`; leading `..` is **preserved** on a relative path but a `..` past root on an **absolute** path is dropped. `normalize("/")` → `"/"`, `normalize("//")` → `"/"`, `normalize("..")` → `".."`, `normalize("a/b/../c")` → `"a/c"`, `normalize("a/./b")` → `"a/b"`, `normalize("a/b/")` → `"a/b"`, `normalize("./a")` → `"a"`, `normalize("/..")` → `"/"`, `normalize("/a/../../b")` → `"/b"`, `normalize("a/../../b")` → `"../b"`. |
| `join` | `[T](parts: List[T]) -> Path where T: PathLike` | **Go `path.Join` style** (NOT Python's absolute-resets-earlier behavior): drop empty parts, join with `/`, then `normalize`. All-empty → `""`: `join(["a","b","c"])` → `"a/b/c"`, `join(["a/","b"])` → `"a/b"`, `join(["","b"])` → `"b"`, `join([])` → `""`, `join(["a","","c"])` → `"a/c"`, `join(["/a","b"])` → `"/a/b"`. **Generic over the element type, not `List[PathLike]`** — Chezzi containers are invariant, so a `List[PathLike]` parameter would take a list *literal* and nothing else (no `List[str]` variable, not even `fs.list_dir`'s own `List[Path]`). Any **homogeneous** list works: `path.join(xs)` for `xs: List[str]`, `path.join(names)` for `names: List[Path]`. A *heterogeneous* literal (`[a_path, "sub"]`) has no single element type and is rejected at the literal, as for any other list. |

<a id="pathpath"></a>
#### `path.Path` — the OUTPUT position of the filesystem surface (W7-8)

An **ordinary Chezzi struct** over the raw OS bytes (`raw: bytes`). It is what `fs.list_dir`/`walk`/
`glob`/`canonicalize`, `os.getcwd()` and every `std.path` helper hand back, and it satisfies
`PathLike` structurally — so `fs.exists(p)` takes it directly, with no conversion.

| method | signature | semantics |
| --- | --- | --- |
| `bytes` | `(self) -> bytes` | The raw OS bytes. Byte-exact, never faults. |
| `decode` | `(self) -> str` | **EXACT** conversion. Recoverable **fault** on a non-UTF-8 path (`invalid UTF-8 in decode()`) — the same fault `bytes.decode()` raises. |
| `str` | `(self) -> str` | **LOSSY display** (`Stringable`): each maximal invalid UTF-8 subsequence → one `U+FFFD`. **Never faults**, so `print(p)` / interpolation always work. |
| `as_path` | `(self) -> bytes` | `PathLike` conformance. |

`Path` is DISPLAY (`str`) vs CONVERSION (`decode`) split on purpose — Rust makes the same split (its
`Path` implements no `Display`). Construct one directly with `path.Path(b"…")`; convert to a mutable
buffer with `bytearray(p.bytes())` (there is no `bytearray` method).

> **Residual hazard, documented not prevented:** `fs.exists(p.str())` on a non-UTF-8 path re-creates
> the W7-8 bug by hand — the lossy display names a *different* (usually nonexistent) file. It is
> mitigated by the fact that `PathLike` accepts a `Path` **directly**, so the natural spelling
> (`fs.exists(p)`) never round-trips through `str` at all. Only reach for `.str()` when you are
> *displaying*.

### `std.datetime` — civil-calendar date/time (UTC-only)
Pure-Chezzi civil-calendar decomposition / construction / duration arithmetic layered on the native
`std.time` clock (`time.now()` only). Built from pure integer math (Howard Hinnant's branch-free
civil-calendar algorithms), so it is **identical across runs**. `import std.datetime`
(or `as dt`).

**CONTRACT — load-bearing semantics (these are contractual, not incidental):**
- **UTC-ONLY (v1).** Every `DateTime` is UTC. There is **NO timezone / DST handling and NO tz
  database** — timezones/DST are explicitly **deferred** to a future milestone.
- **WEEKDAY ORIGIN = `Sunday=0`, Monday=1, …, Saturday=6.** This matches the native `std.time` civil
  math (epoch 0 == 1970-01-01 is a **Thursday == weekday 4**). NOTE: this differs from Python's
  `datetime.weekday()` (Monday=0) — chosen for consistency with `std.time`, not Python.
- **NEGATIVE epochs (pre-1970) are correct.** Chezzi `/` and `%` truncate **toward zero**, which is
  wrong for splitting a negative epoch into days+seconds, so all such splits route through the
  internal `fdiv`/`fmod` **floor-division** helpers. `from_epoch(-1)` → 1969-12-31 23:59:59
  (Wednesday); `to_epoch` round-trips it.
- **DURATION arithmetic operates on epoch INTS (seconds), not `DateTime`** — calendar-aware work goes
  through `from_epoch`/`to_epoch`.

```chezzi
struct DateTime:
    year: int; month: int; day: int
    hour: int; minute: int; second: int
    weekday: int    # 0=Sunday .. 6=Saturday (contractual)
```

| fn | signature | semantics |
| --- | --- | --- |
| `from_epoch` | `(epoch: int) -> DateTime` | Decompose Unix epoch-seconds (UTC) into a `DateTime`. Negative epochs floored. |
| `to_epoch` | `(dt: DateTime) -> int` | Recompose to Unix epoch-seconds. `to_epoch(from_epoch(e)) == e`. |
| `now` | `() -> DateTime` | Current UTC date/time (`from_epoch(time.now())`) — the only clock use. |
| `days_from_civil` | `(y, m, d) -> int` | Days since 1970-01-01 (Hinnant). `(1970,1,1)`→0, `(1969,12,31)`→-1, `(2024,2,29)`→19782. |
| `civil_from_days` | `(z) -> (int, int, int)` | Inverse: `(year, month, day)` tuple. `0`→`(1970,1,1)`, `-1`→`(1969,12,31)`. |
| `is_leap_year` | `(y) -> bool` | Proleptic Gregorian: `2000`→true, `1900`→false, `2024`→true. |
| `days_in_month` | `(y, m) -> int` | Leap-aware. `(2024,2)`→29, `(2023,2)`→28, `(2024,4)`→30. A month outside `1..12` is a domain violation and **faults** (recoverable via `recover:`), like Python `calendar.monthrange` — it never returns a plausible-looking 31. |
| `weekday` | `(epoch: int) -> int` | Weekday (Sunday=0..Saturday=6) of an epoch value. `weekday(0)`→4 (Thu). |
| `weekday_name` | `(wd: int) -> str` | English name: `weekday_name(0)`→`"Sunday"`, `weekday_name(4)`→`"Thursday"`. |
| `to_iso8601` | `(dt) -> str` | `"YYYY-MM-DDTHH:MM:SSZ"`. `from_epoch(0)`→`"1970-01-01T00:00:00Z"`. |
| `to_date_string` | `(dt) -> str` | `"YYYY-MM-DD"`. |
| `to_time_string` | `(dt) -> str` | `"HH:MM:SS"`. |
| `to_string` | `(dt) -> str` | `std.time.format` style `"YYYY-MM-DD HH:MM:SS"`. |
| `parse_iso8601` | `(s: str) -> Result[DateTime]` | The **inverse** of `to_iso8601`: parse ISO-8601 / RFC-3339 — a **strict SUBSET** of Python's `datetime.fromisoformat`, which is looser: three common forms Python accepts are an `Err` here — `"2024-01-01T12:34"` (no seconds), `"20240101T123456Z"` (basic/compact format), and `"+0530"` (offset without a colon). Only the accepted-forms list that follows is a contract. Accepts `"YYYY-MM-DD"` (date-only, midnight), `"YYYY-MM-DDTHH:MM:SS"` (naive == UTC), a `'T'` **or** `' '` date/time separator, an optional trailing `'Z'` or `'+HH:MM'`/`'-HH:MM'` offset (**normalized to UTC**, per Go `time.Parse`), and an optional `.fff` fractional part (**validated then truncated** — `DateTime.second` is an int, no sub-second storage). Malformed or out-of-range fields (month 13, day 32, hour 25, second 60, non-digits, wrong widths) are a **clean `Err`**, never a fault. Every field is **width-checked**: month/day/time are exactly 2 digits and the year is **4+** digits (mirroring `to_iso8601`, which pads to 4 and emits more for an extended year) — so `"24-01-01"` is an `Err`, not year 24. Round-trips: `parse_iso8601(to_iso8601(dt)) == dt` for every year of 9 digits or fewer (a wider year — only reachable from an epoch near the `int` limit — exceeds the parser's overflow bound and `Err`s). |
| `add_seconds` | `(epoch, n) -> int` | `epoch + n`. |
| `add_days` | `(epoch, n) -> int` | `epoch + n*86400` (negative `n` subtracts). |
| `diff_seconds` | `(a, b) -> int` | `a - b`. |
| `diff_days` | `(a, b) -> int` | Whole days `b`→`a`, **floored**: `diff_days(-1, 0)` → -1. |

The string→`DateTime` half is `parse_iso8601` (above); `strftime`-pattern formatting and a general
`strptime`/`from_string` are still deferred (no format-token vocabulary in v1). Two `parse_iso8601`
ceilings, both deliberate under the UTC-only contract: sub-second precision is dropped (`.fff` is
truncated), and a non-`Z` offset normalizes to a UTC epoch rather than round-tripping to itself.
The `DateTime` struct lives in the module
(`datetime.DateTime`); a user program also defining its own top-level `struct DateTime` could collide
— use the module-qualified name.

### `std.collections` — generic single-threaded data structures
Pure-Chezzi generic structs over `T` built on the builtin `list`/`map` — no native surface of their
own. `import std.collections` (or `as col`).

**EMPTY SEMANTICS (load-bearing, consistent):** every removal/peek returns `Option[T]` — an empty
container yields `None`, never a fault, matching the builtin `list.pop() -> Option[T]`.

**`Heap[T]`** — a binary heap over a backing `List[T]` with a comparator **closure**. The comparator
contract (the footgun): `less(a, b) == true` means `a` is **more extreme** than `b`, so `a` pops
**first**. Pass `fn(a,b): a < b` for a **min-heap** (smallest first) and `fn(a,b): a > b` for a
**max-heap** — a "reverse" heap is just the flipped comparator. This generalises to any `T` (floats,
custom priorities, `(priority, item)` tuples) with **no `Comparable` impl** required. With an empty
backing list the element type `T` cannot come from the data, so it is taken from the binding/return/
parameter **annotation**: `h: Heap[int] = Heap([], fn(x, y): x < y)` type-checks (the `Heap[int]`
pins `T=int`, which gives the comparator `x, y: int`); a turbofish `Heap[int]([], …)` or annotated
comparator params (`fn(x: int, y: int): …`) work too.

| member | signature | semantics / complexity |
| --- | --- | --- |
| `Heap` | `Heap(data: List[T], less: fn(T,T)->bool)` | Raw constructor; `Heap([], cmp)` for an empty heap with comparator `cmp`. |
| `min_heap` | `() -> Heap[int]` | Int min-heap factory (`a < b`). |
| `max_heap` | `() -> Heap[int]` | Int max-heap factory (`a > b`). |
| `from_list` | `(xs, less) -> Heap[T]` | Heapify (push-loop, **O(n log n)**, NOT bottom-up O(n)); `xs` untouched. |
| `.push(x)` | `(T) -> nil` | Sift-up. **O(log n)**. |
| `.pop()` | `() -> Option[T]` | Remove+return the extremum (sift-down), `None` if empty. **O(log n)**. |
| `.peek()` | `() -> Option[T]` | The extremum without removing, `None` if empty. **O(1)**. |
| `.len()` / `.is_empty()` | `() -> int` / `-> bool` | **O(1)**. |

**`Deque[T]`** — double-ended queue, **amortized O(1) at both ends** via the **two-stack** design
(`front`/`back` backing lists; a pop whose near stack is empty drains the far stack into it once, so
each element moves between stacks at most once). `peek` reads the head/tail without rebalancing, so
peek is worst-case O(1). Construct directly: **`Deque([], [])`** — `T` is inferred from the first
`push_front`/`push_back`. (No `deque()` factory: a no-argument generic factory cannot bind `T`.)

| member | signature | semantics / complexity |
| --- | --- | --- |
| `.push_front(x)` / `.push_back(x)` | `(T) -> nil` | **O(1)**. |
| `.pop_front()` / `.pop_back()` | `() -> Option[T]` | Remove+return the head/tail, `None` if empty. **Amortized O(1)**. |
| `.peek_front()` / `.peek_back()` | `() -> Option[T]` | Head/tail without removing, `None` if empty. **O(1)**. |
| `.len()` / `.is_empty()` | `() -> int` / `-> bool` | **O(1)**. |

**`Counter[T: Hashable + Eq]`** — a frequency table over `Map[T, int]` (`T` must be `Hashable + Eq`,
like any map key — `Hashable` alone does not imply `Eq`, `docs/gaps.md` W7-53). Construct directly:
**`Counter({})`** — `T` is inferred from the first `add`/`count`. (No `counter()` factory, same
`T`-binding reason as `Deque`.)

| member | signature | semantics / complexity |
| --- | --- | --- |
| `.add(x)` | `(T) -> nil` | `add_n(x, 1)`. **O(1)**. |
| `.add_n(x, n)` | `(T, int) -> nil` | Increment by `n` (creates the entry if absent; `n` may be negative). **O(1)**. |
| `.count(x)` | `(T) -> int` | Count of `x`, **0 if never added**. **O(1)**. |
| `.total()` | `() -> int` | Sum of all counts. **O(n)**. |
| `.most_common(k)` | `(int) -> List[(T, int)]` | Top `k` `(item, count)` pairs by **descending count**; `k` clamped to `[0, len]` (`k<=0`→`[]`, `k>=len`→all). **O(n log n)**. |

**Counter tie-break:** equal counts keep **insertion order** — guaranteed because `map.keys()` yields
insertion order **and** the list `sort_by` is a **stable** merge sort. This is a
load-bearing dependency on stable sort; do not swap `sort_by` to an unstable sort.

**No ordered-map wrapper (intentional):** the builtin `map` is **already insertion-ordered**
(`map.keys()`/`values()`/`for k,v in m` all iterate in insertion order; `std.json`'s round-trip relies
on it). Use the builtin `map` directly — there is no `OrderedMap` here (and no move-to-end / LRU
`popitem` primitive; out of scope).

### `std.concurrency.collection` — thread-safe collections over `RwShared`
Pure-Chezzi generic structs wrapping the `RwShared[Map[...]]` runtime cell (many concurrent readers
**or** one exclusive writer) — no native surface of their own.
`import std.concurrency.collection` (or `as col`). This is the **first nested std
module** — the dotted path resolves to `std/concurrency/collection.chz` with no special-casing.

**Why over raw `RwShared`:** raw `read`/`write` closures are verbose, and the **compound** mutations
(insert-if-absent, increment a count) MUST happen inside a **single** `write` lock or they race — these
wrappers bake the correct single-lock idiom in.

**Airlock sharing (load-bearing):** a struct whose only field is an `RwShared` crosses the
`spawn`/`parallel:` airlock as a **shared `Arc` handle, NOT a deep copy** — so a mutation a spawned
task makes is visible to the parent after the join. (`RwShared` is sendable and shares; a struct of
all-sendable fields is too.)

**Construction (no factory):** there is **no** `new_Map()`/`new_counter()` — a no-argument generic
factory cannot bind `K`/`V` (turbofish does not propagate into the inner `RwShared({})`). Construct
**directly** at the use site: **`ConcurrentMap(RwShared({}))`** / **`ConcurrentCounter(RwShared({}))`**;
`K`/`V` are deferred from the empty `{}` and stay `Unknown` on the wrapper (the methods operate on the
`ConcurrentMap`, they do not pin the inner map type). That is fine for the wrapper itself, but a value
*derived* from it whose type surfaces the map — e.g. `snap := m.snapshot()` (`-> Map[K, V]`) — lands an
unpinned `Map[Unknown, Unknown]` local, which the empty-collection rule flags; annotate it
(`snap: Map[str, int] = m.snapshot()`). Note the use-site `RwShared({})` means user code also needs **`import std.concurrency`**
in addition to `import std.concurrency.collection` (the latter, a len-3 submodule, does **not** license
the bare `RwShared` ctor — only the whole-module `import std.concurrency` does).

**Reentrancy:** like raw `RwShared`, a `read`/`write` closure must not re-enter the **same** box. Every
wrapper method is flat (no nested locking), so user code is safe as long as it does not call a wrapper
method from inside another wrapper's closure.

**`ConcurrentMap[K: Hashable + Eq, V]`** — thread-safe map over `RwShared[Map[K, V]]`. `get`/`contains`/
`len`/`snapshot` are **concurrent reads**; `set`/`remove`/`get_or_insert` take the **exclusive write
lock**.

| member | signature | concurrency / semantics |
| --- | --- | --- |
| `.get(key)` | `(K) -> Option[V]` | **concurrent read**. `Some(v)` / `None`. |
| `.set(key, val)` | `(K, V) -> nil` | **exclusive write**. Insert or overwrite. |
| `.remove(key)` | `(K) -> nil` | **exclusive write**. No-op if absent. |
| `.contains(key)` | `(K) -> bool` | **concurrent read**. |
| `.len()` | `() -> int` | **concurrent read**. |
| `.get_or_insert(key, default)` | `(K, V) -> V` | **COMPOUND-ATOMIC**: the check, the insert, AND capturing the value to return all happen inside ONE **exclusive write** lock (the value is stashed into a captured shared box by the write closure) — so there is no second lock, and no window in which a concurrent `remove` could delete the just-inserted key. Returns the existing value, or `default` if it was absent. |
| `.snapshot()` | `() -> Map[K, V]` | **concurrent read** returning a **copy** independent of later mutations. |

**`ConcurrentCounter[K: Hashable + Eq]`** — thread-safe frequency table over `RwShared[Map[K, int]]`.
`count`/`total` are **concurrent reads**; `increment`/`add` take the **exclusive write lock** and do
their read-modify-write inside **one** closure, so N tasks each incrementing the same key produce an
**exact** final count (no lost updates — the classic race-free concurrent counter).

| member | signature | concurrency / semantics |
| --- | --- | --- |
| `.increment(key)` | `(K) -> nil` | **exclusive write**, atomic RMW `+1` (created at 1 if absent). |
| `.add(key, n)` | `(K, int) -> nil` | **exclusive write**, atomic RMW `+n` (`n` may be negative; created at `n` if absent). |
| `.count(key)` | `(K) -> int` | **concurrent read**, **0 if absent**. |
| `.total()` | `() -> int` | **concurrent read**, sum of all counts. |

**Not provided (intentional):** a concurrent **queue** is already `Channel[T]`; an **atomic scalar** is
already `Atomic`. There is no `ConcurrentList`/`ConcurrentSet`/`ConcurrentQueue`.

### `std.concurrency.pmap` — scoped parallel map
`import pmap from std.concurrency.pmap` (or `import std.concurrency.pmap`). Pure Chezzi over a
`parallel:` nursery + `Channel[T]`, so it runs byte-identically on every engine.

| function | signature | semantics |
| --- | --- | --- |
| `pmap` | `pmap[T, U](xs: List[T], f: fn(T) -> U) -> List[U]` | spawn one task per element, run `f` on each in parallel, return the results in **submission order** (`[f(xs[0]), f(xs[1]), …]`). |
| `pmap_limited` | `pmap_limited[T, U](xs: List[T], f: fn(T) -> U, limit: int) -> List[U]` | same, but at most `limit` tasks run `f` at once (a channel-as-semaphore token bucket). `limit > 0` required (`limit <= 0` deadlocks — no permits). |

Determinism comes from reassembling by submission index (a `sort_by_key` on the tagged results),
**never** completion order — so a run that finishes tasks in a different order still returns the
identical `List[U]`. The nursery lives inside the helper and joins before the collect, so a task can
never outlive the call (structured concurrency); `f` crosses the airlock into each task by value.
`pmap_limited` is also the standard concurrency limiter — cap parallel calls into a rate-limited
resource with it instead of hand-rolling a semaphore each time.

### `std.concurrency.task` — result handles for `Executor` work
`import submit_task from std.concurrency.task` (or `import std.concurrency.task`). Pure Chezzi over a
cap-1 `Channel[T]` (a one-shot result slot), so it runs byte-identically on every engine. Fills the
gap that bare `Executor.submit(f)` is fire-and-forget (returns nothing).

| item | signature | semantics |
| --- | --- | --- |
| `submit_task` | `submit_task[T](ex: Executor, f: fn() -> T) -> Task[T]` | submit `f` to `ex` for detached execution and get a handle for its result. The work STARTS at the submit and is waited for by `shutdown()` (or the program-exit join). |
| `Task.get` | `get(self) -> T` | block until the result is available, then return it. **Memoized** — idempotent, safe to call repeatedly (a second call returns the cache, not a second `recv`). |
| `Task.done` | `done(self) -> bool` | whether the result has landed yet. Never blocks. |

Canonical shape: submit every task, `shutdown()`, then `.get()` each. **Determinism rule:** a `Task`'s
value is deterministic (it is `f()`); only *when* it runs varies at runtime (the OS-thread workers race)
— so `.get()` is byte-identical across runs **as long as you await in a fixed (e.g. submission) order**.
There is deliberately no `join_next()`/select-on-completion API — completion order is nondeterministic
and would break that determinism.

### `std.cmp` — ordering generics (`Comparable`)
`max[T: Comparable](a, b) -> T` · `min[T: Comparable](a, b) -> T` ·
`clamp[T: Comparable](x, lo, hi) -> T`.
`Comparable` embeds `Eq`, but a struct/enum satisfies `Eq` **structurally**, so defining
`compare(self, other: Self) -> int` is enough — an `eq` is optional and only needed to *override* `==`
(int/float/str satisfy `Comparable` intrinsically, with no method to write). M23's "must also define
`eq`" rule was dropped 2026-08-11; see `docs/gaps.md` **W7-41**. Its `compare` is **total on floats**: a `NaN` operand
returns an ordering int (never a fault), using the same total order `List.sort()`/`sort_by_key`/`min`/
`max` use (`f64::total_cmp`, `NaN` to one end). The `<`/`<=`/`>`/`>=` *operators* stay IEEE (`false` for
every `NaN` comparison) — that is the one divergence. These three `std.cmp` fns are written with `<`, so
they follow the **operator** rule, not the total order: with a `NaN` argument `min`/`max` return whichever
side the `false` comparison selects and `clamp` likewise — filter `NaN` first if that matters.

### `std.bisect` — binary search & sorted-insert (Python `bisect`)
Over an ascending-sorted `List[T: Comparable]` (compares with `<` → dispatches through `Comparable`).
`bisect_left(xs, x) -> int` returns the leftmost insertion index (before equal elements);
`bisect_right(xs, x) -> int` (a.k.a. `bisect(xs, x)`) the rightmost (after equal elements).
`insort_left(xs, x)` / `insort_right(xs, x)` insert `x` in place keeping `xs` sorted (O(n)
grow-then-shift, same cost as Python's `insort` — `List` has no native insert).
**v1 limits (not bugs):** no `key: fn(T) -> K` variant and no bare `insort` alias (YAGNI — one-line
adds on demand). `xs` MUST already be sorted ascending; results are undefined otherwise.

### `std.memoize` — result caching (`functools.cache`)
`memoize1(f: fn(K) -> V) -> fn(K) -> V` wraps `f` so each distinct argument is computed once and the
result cached in a captured `Map[K, V]` (`K: Hashable + Eq` — a map key needs both, `docs/gaps.md`
W7-53). The cache is a native reference type, so it
persists across every call to the wrapped fn; `f` runs at most once per distinct arg.
**v1 limit (not a bug):** single-argument only. A general N-arg cache would key a `Map[tuple, V]` on
the argument tuple, but tuples aren't Hashable map keys yet — until then curry, or pack args into a
struct with `hash` and memoize the single-arg wrapper.

### `std.duration` — Go-like first-class time spans
Pure-Chezzi (no native seam). `import std.duration`. `Duration` (access as `duration.Duration`) is a
plain struct over a single int of **milliseconds**.
- **Constructors** (free fns): `millis(n)`, `seconds(n)`, `minutes(n)`, `hours(n)` → `Duration`.
- **Accessors** (methods): `d.as_millis() -> int`, `d.as_seconds()/as_minutes()/as_hours() -> float`.
- **Arithmetic** (methods): `d.add(o)`, `d.sub(o)`, `d.scale(k: int)` → `Duration`.
- **`d.to_string() -> str`** — Go `time.Duration.String()` decimal-seconds shape: `"0s"`, `"250ms"`,
  `"1.5s"`, `"1m30s"`, `"1h0m0s"`, negatives prefixed `"-"` (`"-1.5s"`).
- **`parse(s: str) -> Result[Duration]`** — inverse of `to_string`; also accepts Go's looser forms:
  optional leading `+`/`-`, one or more `<number><unit>` groups (units `h`/`m`/`s`/`ms`, unordered and
  summed), decimal magnitudes (`"1.5h"`, `".5s"`, `"0.25s"`), and a bare `"0"`. Malformed input (empty,
  no unit, unknown unit, multiple dots, trailing dot, oversized magnitude) is a **clean `Err`**, never a
  fault. Round-trips exactly (`parse(d.to_string())` ⇒ `d`) for every magnitude the parser accepts —
  **but the accept bound is a DIGIT COUNT, not a value, so the round-trip breaks at the `int` extremes
  and the bound is unit-blind.** The guard is `intpart.len() > 12` (`std/duration.chz:142`), sized for
  the worst unit (`h`, ×3 600 000) and applied to every unit, so `"100000000000h"` (12 digits,
  3.6e17 ms) is `Ok` while the five-orders-smaller `"2562047788015ms"` (13 digits, 2.5e12 ms) is
  `Err("duration out of range")`. Consequently `parse(millis(i64::MAX).to_string())` — whose text is
  `"2562047788015h12m55.807s"` — `Err`s. Go's `time.ParseDuration` round-trips
  `time.Duration(math.MaxInt64)` fine. (`docs/gaps.md` **W8-9**.)
- **`since(start: float) -> Duration`** — elapsed since a `time.monotonic()` reading (imports native
  `std.time`; floors to whole ms). **`sleep(d: Duration)`** — delegates to native `sleep_ms`.

**Why milliseconds (and the sub-ms ceiling):** ms matches `sleep_ms`/`timer(ms)` and overflows an i64
only at ~292 **million** years (a Go nanos i64 caps at ~292 years). The trade is that microseconds/
nanoseconds are **unrepresentable** — `parse("1us")`/`parse("1ns")`/`parse("1µs")` are a clean `Err`,
and a fractional literal below 1ms (e.g. `"0.0005s"`) floors to `0ms`.

### `std.flag` — Go-style CLI arg parsing
Pure-Chezzi CLI parser over `os.args()` (already the program args **without** argv[0], so
`fs.parse(os.args())` is the direct Go `flag.Parse(os.Args[1:])` analog). `import std.flag`.

`new() -> FlagSet` builds an empty set; register flags on it, then `parse` a `List[str]`:
```chezzi
fs := flag.new()
fs.str_flag("name", "world", "who to greet")   # (name, default, help)
fs.int_flag("count", 1, "how many times")
fs.bool_flag("verbose", false, "chatty output")
match fs.parse(os.args()):
    Ok(rest): ...                               # rest = the leftover positionals
    Err(e):   print(e.message())
```
Register: `str_flag(name, default, help)` · `bool_flag(name, default, help)` ·
`int_flag(name, default, help)` (each mutates the set). Parse: `parse(args: List[str]) ->
Result[List[str]]` — `Ok(positionals)` on success (folds Go's `Parse()` + `Args()` into one), a clean
`Err` on an unknown flag / missing value / non-int (**never faults** on bad user input). Read back:
`get_str(name) -> str` · `get_bool(name) -> bool` · `get_int(name) -> int` (the registered default
until parse overwrites it; **panics** on an *unregistered* name — a programmer error, not a
user-input path; closer to Python argparse's `AttributeError` on an unregistered destination than to
Go, whose `flag.Lookup` returns `nil` for an unknown name and never panics — measured, Go 1.26) ·
`positionals() -> List[str]` · `usage() -> str` (Go `PrintDefaults`-style, one line
per flag in registration order).

Recognised syntax (Go conventions): `--name value` / `--name=value` / `--verbose` (bool presence) /
`--verbose=false` (explicit; the `=`-value accepts Go's `strconv.ParseBool` set —
`1 t T TRUE true True` / `0 f F FALSE false False`) / `--` terminator (every later token is a positional). A leading run of
dashes is stripped, so a flag named `n` answers to **both** `-n` and `--n` — a deliberate v1
simplification vs strict Go (which registers each spelling separately); a lone `-` is a positional.
Deferred (not built): required-flag enforcement, subcommands, duplicate-registration detection.

### `std.log` — leveled logging
Pure-Chezzi leveled logger over `std.io` (Go `log`/`slog` + Python `logging`). `import std.log`.

Levels (Go `slog` order + NAMES — `WARN`, not Python's `WARNING`): `DEBUG(0) < INFO(1) < WARN(2) <
ERROR(3)`, exposed as module fns (`log.DEBUG()` … `log.ERROR()`) so callers pass them explicitly.

```chezzi
lg := log.new()              # min level INFO, output to stderr (both are the Python/Go defaults)
lg.debug("noisy")            # DROPPED — below INFO
lg.info("served")            # → stderr: "INFO served"
lg.warn("careful")           # → stderr: "WARN careful"
lg.set_level(log.DEBUG())    # now debug() passes
```
`new(min_level: int = 1, to_stderr: bool = true) -> Logger` (default min = `INFO`, output to
**stderr** — the anti-drift default of both Python `logging` and Go `log`/`slog`; pass
`to_stderr=false` to route to stdout). Methods (mutable-self): `debug/info/warn/error(msg)` format
`"LEVEL message"` and write to the target, gated by the min level (a message below it is dropped);
`set_level(level)` · `set_prefix(p)`.

**Timestamps are opt-in and injectable, never baked in** — a live clock makes output
non-deterministic (ungoldenable). The core is a pure, deterministic `format_line(level, msg) -> str`
("LEVEL message") that a golden pins. For a real timestamp, `set_prefix(stamp)` with a **caller-owned**
value (e.g. from `std.time` / `std.datetime`) — it is prepended (with a space) to every line;
`set_prefix("")` clears it. std.log itself imports no clock, so the default path stays deterministic.

Deferred (not built): handlers/formatters, hierarchical (named) loggers, structured key/value fields —
the full Python `logging` / Go `slog` machinery.

### `std.iter` — list/iterator helpers
`enumerate(xs) -> List[(int, T)]` · `zip(xs, ys) -> List[(A, B)]` · `map(xs, f)` · `filter(xs, pred)` ·
`fold(xs, init, f)` · `reduce(xs, f) -> T` (**non-empty**: with no seed there is no accumulator to
start from, so an empty list faults `reduce: empty list with no initial value` — a recoverable fault,
catchable by `recover:`; seed it with `fold(xs, init, f)` if the list can be empty) ·
`sum(xs: List[int]) -> int` (empty → `0`;
**int-only** — the free-function form of the `xs.sum()` method so it can sit on the right of a pipe;
for a float list use the method, which is generic) · `take(xs, n)` · `drop(xs, n)` ·
`any(xs, pred) -> bool` · `all(xs, pred) -> bool` · `find(xs, pred) -> Option[T]` ·
`flatten(xss) -> List[T]`.

**Lazy adapters (itertools)** — return a lazy `Iterator[T]` (a generator, not a `List`), pulling from
their source only as the consumer pulls, so an infinite source composed under `islice` terminates:
`count(start=0, step=1) -> Iterator[int]` (infinite arithmetic counter) ·
`repeat(x, n=-1) -> Iterator[T]` (`x` forever if `n<0`, else `n` times) ·
`cycle(xs) -> Iterator[T]` (endlessly repeat a list's elements; **empty list = empty/immediately-done**,
not an infinite spin) · `chain(a, b) -> Iterator[T]` (all of `a` then all of `b`; **two-arg only** in
v1) · `islice(it, stop) -> Iterator[T]` (the first `stop` elements of any iterator; `stop<=0` = empty)
· `imap(it, f) -> Iterator[U]` / `ifilter(it, pred) -> Iterator[T]` (the **lazy** siblings of the eager
`map`/`filter` — named `imap`/`ifilter` since Chezzi has no overloading). The `it`-taking adapters
(`islice`/`imap`/`ifilter`) accept **any iterable** — a list, set, str, user `next()` struct, a
`.iter()` cursor, or another generator — via the `[S: Iterable[T], T]` bound.

Random helpers (call `std.rand`; seed via `rand.seed(n)` for reproducibility — these are pure-Chezzi
because the native seam can't return a generic `List[T]`). They inherit `std.rand`'s security
properties: the PRNG is **predictable from its output**, so never use these to shuffle or pick
something an adversary must not guess — see the `std.rand` **Security** note above:
`shuffle(xs) -> List[T]` (new randomly-permuted list, Fisher–Yates, non-mutating) ·
`choice(xs) -> Option[T]` (`None` on empty) ·
`sample(xs, k) -> List[T]` (`k` elements without replacement; `k` clamped to `[0, len]`).

### `std.json` — JSON
```chezzi
enum Json:
    Null
    Bool(bool)
    Num(float)
    Str(str)
    Arr(List[Json])
    Obj(Map[str, Json])
```
`parse(s) -> Result[Json]` · `stringify(j) -> str` · `is_null(j) -> bool` ·
`as_bool(j) -> Option[bool]` · `as_float(j) -> Option[float]` · `as_int(j) -> Option[int]` ·
`as_str(j) -> Option[str]` · `as_object(j) -> Option[Map[str, Json]]` · `as_array(j) -> Option[List[Json]]` ·
`get(j, key) -> Option[Json]` · `at(j, i) -> Option[Json]` · `len(j) -> int` (faults on
`Null`/`Bool`/`Num` — a scalar has no length, matching CPython's `len(None)` `TypeError`).

> **Nesting depth is capped at `MAX_NEST_DEPTH = 2000` on both `parse` and `stringify`.** `std.json`
> is recursive-descent in pure Chezzi, so nesting depth becomes recursion depth in your program.
> `parse`'s `Result` is now **total**: past the cap it returns `Err("exceeded max depth")` instead of
> aborting the process, matching Go's `encoding/json` (`exceeded max depth`) and CPython's catchable
> `RecursionError`. `stringify` has no `Result` to put an error in — a document built directly in
> memory (no `parse` involved) that nests past the cap instead **faults** with
> `json.stringify: exceeded max depth` (recoverable under `recover:`), the same shape CPython's
> `json.dumps` uses (`RecursionError`).
>
> **Chezzi's cap is stricter than either ancestor's, and that is structural rather than a preference.**
> Measured 2026-08-18: CPython's `json.loads` accepts nesting 50 000 and raises `RecursionError` at
> 100 000; Go's `encoding/json` accepts 5 000 and reports `exceeded max depth` at 10 001 (its cap is
> 10 000). Chezzi cannot reach either, because `std.json` runs *on the VM*: each nesting level costs
> **2 VM frames** against the engine's 10 000-frame call-depth cap, so the absolute ceiling is ~5 000
> even with zero caller frames — measured, a `[`-only document with nothing above it dies at exactly
> that. `MAX_NEST_DEPTH = 2000` is that ceiling minus headroom for the frames a real caller has above
> `parse`/`stringify`. Raising it trades that headroom for depth nobody has asked for; the way to
> genuinely match Go would be an iterative parser, which is a rewrite, not a constant. Documents
> nesting deeper than 2 000 are rejected **cleanly** — before this cap they killed the process.
> `docs/gaps.md` **W8-5**.

> **There is no `encode`/`dumps` inverse of `decode[T]`.** Serialization goes the long way: build a
> `Json.Obj`/`Json.Arr` tree by hand, then `stringify` it. A struct → JSON round-trip is therefore
> asymmetric (`decode[T]` one line in, hand-built variants out) — `docs/gaps.md` **W8-20**.

Every JSON number is stored as an f64, so `as_int` and `json.decode[int]` are **total** at the
float→int boundary — neither ever saturates silently to a wildly-wrong value nor faults: a number
clearly outside the `int` (i64) range (e.g. `1e30`, `18446744073709551615`) or non-finite yields
`None` from `as_int` and an `Err` from `json.decode[int]`, and `i64::MAX` / `i64::MIN` still
round-trip. **f64-model
caveat:** because integers are held as f64 (53-bit mantissa), values within ~one ULP of `±2^63` are
indistinguishable from the boundary — so an input that rounds to exactly `±2^63` (this includes
`i64::MAX`/`i64::MIN` themselves and their just-out-of-range neighbours like `9223372036854775808`)
decodes to `i64::MAX`/`i64::MIN` rather than `Err`/`None`. This residual is inherent to the f64 JSON
number model, not a saturation of arbitrary large values. `as_int` truncates a fractional number
(`as_int(2.5)` → `Some(2)`).

**Non-finite floats:** standard JSON has no `NaN`/`Infinity`, so `stringify` **faults** — recoverable,
catchable under `recover:` — with the message `cannot serialize non-finite float to JSON` when a
`Json.Num` holds a non-finite float (`NaN`, `+inf`, `-inf`). This is the Go
`encoding/json` policy (error out) rather than Python's non-standard `NaN`/`Infinity` tokens: it never
emits malformed output that Chezzi's own `parse` would reject. Symmetrically, `parse` **rejects at
decode** any numeral whose magnitude overflows f64 to a non-finite value (`1e400` → +inf,
`-1e400` → -inf): it returns `Err("invalid number: value out of range")` rather than manufacturing a
`Json.Num(inf)` that `stringify` would then refuse — so `parse`→`stringify` round-trips for every
value `parse` accepts. Finite floats of any magnitude (including e.g. `1e300`, far outside the
int-collapse range) parse and stringify normally and round-trip; underflow to `0.0` (`1e-400`) is
finite and stays accepted.

**Control chars & number grammar (RFC-8259):** `stringify` `\u00XX`-escapes control characters
`U+0000..U+001F` that lack a shorthand escape (the Go `encoding/json` policy) rather than emitting the
raw byte, so its output is always valid JSON. Symmetrically `parse` **rejects** a raw control char
inside a string literal (`Err("invalid control character in string")`) and a leading-zero integer
(`01`, `007`, `-01`) with `Err("invalid number: leading zero")` — a `0` must be a lone `0`/`-0` or
followed by `.`/`e`, matching Python's `json.loads`. (`0.5`, `0e1`, `10` stay valid.)

For a known shape, `json.decode[T](s) -> Result[T]` (a `std.json` member, the one type-argument
method-call form — not a global builtin) deserializes straight into a struct / `Map[str, V]` /
`List[T]` / scalar: `Option` fields accept null-or-absent, extra keys are ignored, and
recursive/generic struct targets are rejected (use the `Json` enum for those).

A JSON *literal in Chezzi source* clashes with string interpolation, so use a raw string
(`r"""{"k": 1}"""`, verbatim — preferred) or double the braces (`"{{ }}"`); a bare `{…}` in a normal
string is interpolation.

### `std.cancel` — cooperative cancellation & timeouts
`struct Token` with methods `cancelled() -> bool` · `reason() -> str?` · `cancel() -> nil` ·
`done() -> Channel[bool]` (use in `wait:`) · `deadline_at() -> float` · `derive() -> Token` (linked child).
Constructors: `manual() -> Token` · `timeout(ms: int) -> Token` · `derive(parent: Token) -> Token`.
Registration is Go `context.WithCancel`'s: `derive()` is **O(1)** — it registers the child into its
IMMEDIATE parent only (no ancestor walk) — and `cancel()` cascades **downward**, draining each node's
child registry and marking every transitive descendant it reaches. A child also keeps a `parent` link
that `cancelled()`/`reason()` fall back to, so an interrupted cascade cannot lose a subtree: those two
are therefore **O(depth)** (0.30 µs at depth 0, ~+0.29 µs per ancestor), and a `Token` from a tree
deeper than **4 999** faults `maximum structural depth (10000) exceeded` when it crosses the airlock.
See `concurrency.md` for the cancellation model. Cancellation is **cooperative**: a task blocked in a
`std.io`/`std.fs`/`std.process`/`std.request` call observes the token only once that call returns —
those are [uninterruptible while in flight](#blocking-calls-cannot-be-interrupted).

---

> Where this lives: native modules are Rust under `src/native/*.rs`; the pure-Chezzi modules are real
> `.chz` files under `std/`. Built-in type methods and global builtins are dispatched by the checker
> (`src/checker/mod.rs`) and the VM.
