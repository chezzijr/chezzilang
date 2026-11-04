# Chezzi — Standard library & builtin reference

This reference covers everything that Chezzi user code can call: global builtins, methods on the
built-in types, the runtime types and the `std.*` modules. Language **syntax** lives in
[`syntax.md`](../syntax/01-lexical-and-literals.md). This folder is the **library** surface.

Conventions:
- Signatures use Chezzi types: `int`, `float`, `bool`, `str`, `None`, `List[T]`, `Map[K, V]`,
  `Set[T]`, `tuple` (`(A, B)`), `bytes`, `bytearray`, `T?`, `T!` / `T!E`,
  `fn(A) -> B` (function values).
- "*mutates*" means the call changes the receiver in place and returns `None`. Other methods return
  a fresh value and leave the receiver untouched.
- `import std.X`, then call `X.func(...)`. Global builtins and type methods need no import.

---

## 1. Global builtins (no import)

| Function | Signature | Notes |
|----------|-----------|-------|
| `print` | `print(...args: Any, sep: str = " ", end: str = "\n") -> None` | Write each argument (any type) to stdout. Variadic: declared as `native fn print(...args: Any, sep, end)` in `std/prelude.chz`; `Any` is the top type, so every value is accepted. The args are joined by `sep` (default `" "`) and `end` (default `"\n"`) is appended. Both are `str` and keyword-only (after the variadic). `print("a", end="")` emits `a` with no newline; `print("a","b", sep="-", end="!")` emits `a-b!`. As a value, `print` has the type `fn(...Any, str = …, str = …) -> None` and fits a `fn(...Any) -> None` slot: `p := print; p(1, 2, sep="-")` emits `1-2`, and `fs := [print]; fs[0](1, 2)` emits `1 2`. Only a certain binding (`p := print`) takes `sep=`/`end=`; any other value call uses the defaults. A `fn(Any) -> None` slot does not accept `print` (Go rejects `fmt.Println` as a `func(any)` too). |
| `Range` | `a..b` / `a..=b` / `..b` / `a..` / `..` / `Range(start, end, step = 1, inclusive = false)` | A range of ints as a VALUE (`std/prelude.chz`, in scope everywhere). Fields `start: int?`, `end: int?`, `step: int`, `inclusive: bool`. `Iterable[int]` (iterate it any number of times), `Contains[int]` (`x in r`), `len()` (needs both bounds), Eq and Hashable. A negative `step` counts down (`Range(10, 0, -1)` yields `10,9,…,1`); `step == 0` faults at construction. There is no `range()`: `range(n)` is `0..n`, `range(a, b)` is `a..b`, `range(a, b, c)` is `Range(a, b, c)`, a list is `List.from_iter(0..n)`. |
| `RecvError` | `RecvError()` | The error of `Channel[T].recv() -> T!RecvError` on a closed, drained channel (`std/prelude.chz`, in scope everywhere, no fields). `message()` is `receiving on a closed channel` (Rust `std::sync::mpsc::RecvError`). See [`02-channel.md`](../concurrency/02-channel.md#closing-a-channel). |
| `int` | `int() -> int` | The zero value `0` (a call on a scalar type's name sets no field). A conversion is a method: `f.to_int() -> int?` (truncates; `None` for NaN, ±inf and outside i64), `s.to_int() -> int!` (the error names the cause). An argument is a type error that names the method ([conversions](../spec/05-conversions.md)). |
| `float` | `float() -> float` | The zero value `0.0`. Convert with `n.to_float() -> float` (an `int`), `s.to_float() -> float!`. |
| `bool` | `bool() -> bool` | The zero value `false`. There is no truthiness cast (Go, Rust): test `x != 0` or `s.len() > 0`. |
| `str` | `str() -> str` | The zero value `""`. Display any value with interpolation, `"{x}"` (Go `fmt.Sprint`). Every type satisfies the `Stringable` protocol (see `syntax.md`), so `[T: Stringable]` generics accept `List`, `Map`, structs, handles and the rest. A string NESTED inside a container, struct field or enum payload renders as its Python `repr`, quoted and escaped (`"{xs}"` with `xs := ["a", "b"]` is `['a', 'b']`); a bare string stays its own characters. One residual difference: a codepoint that CPython's Unicode build does not assign escapes there but prints literally here (a Unicode-version skew, see [`gaps.md`](../../contributing/gaps.md)). See `syntax.md` §"A nested `str` is quoted". |
| `ord` | `ord(s) -> int` | Unicode codepoint of `s`, which must be exactly **one character** (`ord("é")` → 233; `ord("ab")` faults, like Python). |
| `chr` | `chr(code) -> str` | One-character string for codepoint `code`. |
| `panic` | `panic(msg) -> never` | Raise a recoverable fault (caught by the nearest `recover:`, else aborts). Bottom-typed. |

`int(x)`, `float(x)`, `str(x)`, `bool(x)` and `bytes(x)` are type errors for every argument; the help
names the form for the argument's type (`bytes.from_iter(xs)` / `bytes.zeroed(n)` for `bytes`).

`Map` and `Set` test membership with `contains`. `bytes` and `bytearray` take `+`, `<` and `in` (an
int byte, or a bytes/bytearray subsequence: `Contains[int]` and `Contains[bytes]`).

The universe builtins `ord`/`chr`/`panic` (first-class `native fn`) and `int`/`float`/`bool`/`str`/`bytes`/
`bytearray` (`native ctor`, no parameter, not first-class) declare their signatures in Chezzi, in the always-linked
`std/prelude.chz`; the engine binds their bodies by name. `print` (variadic) and the empty `List`/`Map`/`Set`
constructors are engine-synthetic. The static methods below (`List.from_iter`, `bytes.zeroed`) are
`native fn` decls with no `self` inside each type's `native struct` in the prelude. A `native fn`/`native ctor` declaration is legal only in the prelude
and std, never in user code (see `syntax.md` §12c).

### Container constructors

A call on a builtin type's name makes only an empty value. A value built from an iterable comes from
the type's static method `T.from_iter(xs)` (Rust `FromIterator::from_iter`, Python `list(xs)`). A call
on the name with an argument is a type error whose help names the static: `List([1])` reports
`List() takes no argument` with `help: build a List from an iterable with \`List.from_iter(xs)\``.
`from_iter` takes any for-iterable whose elements are the type's elements. The elements keep their
own type: `List[int?].from_iter([5])` is a type error (Rust rejects the same program, E0271), and
`List[int?].from_iter([?5])` checks.

| Form | Gives | Notes |
|------|--------|-------|
| `List[T]()` / `List()` / `List.from_iter(xs)` | `List[T]` | Empty list (`List[T]()` pins the element type; bare `List()` takes its type from the expected type on the same statement, like `Set()`; an untyped `xs := List()` is an error on that line) / convert an iterable to a list. List literal: `[a, b, c]`. `List[T].from_iter(xs)` checks `xs`'s elements against `T`. |
| `Map[K, V]()` / `Map()` / `{}` / `Map.from_iter(pairs)` | `Map[K, V]` | Empty map (`Map[K, V]()` pins the key/value types; bare `Map()` takes its type from the expected type on the same statement; an untyped `m := Map()` is an error on that line) / a map from an iterable of `(K, V)` pairs, last wins on a repeated key; a `Map` iterates its pairs, so `Map.from_iter(m)` copies `m`. Map literal: `{k: v, ...}`. |
| `Set[T]()` / `Set()` / `Set.from_iter(xs)` | `Set[T]` | Empty set (`Set[T]()` pins the element type; `{}` is the empty **map**, not a set; bare `Set()` takes its type from the expected type on the same statement; an untyped `s := Set()` is an error on that line) / set from an iterable. `Set[T].from_iter(xs)` checks elements against `T`. |
| `bytes.zeroed(n)` / `bytes.from_iter(xs)` | `bytes` | `n` zero bytes (Python `bytes(n)`; a negative `n` faults `bytes.zeroed() size -1 must be non-negative`, a size that cannot be allocated faults `bytes.zeroed() size ... is too large to allocate`) / the bytes of any iterable of `int` (a `bytes`, a `bytearray`, a `List[int]`, a `Range`, a generator; each element 0–255, else the fault `bytes.from_iter() element 300 out of range (must be 0..=255)`, as Python `bytes(it)`). To UTF-8 encode a `str`, use `s.encode()` (Python's `bytes(str)` also errors without an encoding). Literal: `b"..."`. `bytes()` is a type error. |
| `bytearray()` / `bytearray.zeroed(n)` / `bytearray.from_iter(xs)` | `bytearray` | Empty growable byte buffer / `n` zero bytes / a mutable buffer of any iterable of `int` (a copy of a `bytes` or `bytearray`). There is no `bytearray` literal. |

### Which failures fault, and why they are not `T!E`s

The calls below **fault** instead of returning a `T?`/`T!E` carrier. Each one matches the ancestor
language that owns it: the reference language raises there too, so a carrier would be the divergence.
Do not convert an entry to `T!E` without new ancestor evidence. Every fault here is recoverable
(`r := recover: <expr>`), **except** the `std.ffi` row at the end. The table names a carrier sibling
where one exists. "Measured" means **CPython 3.14** (3.14.7), unless the row names another ancestor;
the concurrency rows use **Go 1.26** (go1.26.6), because Chezzi's concurrency primitives are Go's.

| Chezzi | Fault message | Ancestor (measured) | Carrier alternative |
|--------|---------------|----------------------|---------------------|
| `xs[i]`, `s[i]` out of range | `index 5 out of bounds (len 3)` | `IndexError: list index out of range` / `string index out of range` | — (`first`/`last`/`get`) |
| `m[k]`, key absent | `key not found: 'zz'` | `KeyError: 'zz'` | **`m.get(k) -> V?`** (Python `dict.get` → `None`) |
| `xs.remove(i)` out of range | `index 9 out of bounds (len 3)` | `IndexError: pop index out of range` | — |
| `xs.chunk(n)` / `xs.windows(n)`, `n <= 0` | `chunk/window size must be positive, got 0` | `ValueError: n must be at least one` (`itertools.batched`) | — |
| `chr(code)` out of range | `chr(): -1 is not a valid Unicode codepoint` | `ValueError: chr() arg not in range(0x110000)` | — |
| `ord(s)` on `""` | `ord() of an empty string` | `TypeError: ord() expected a character, but string of length 0 found` | — |
| `ord(s)` on a multi-**character** string | `ord() expects a 1-character str, got 2 characters` | `TypeError: ord() expected a character, but string of length 2 found` | — |
| `s.split("")` | `split: sep must not be empty` | `ValueError: empty separator` | — |
| `rand.int(lo, hi)`, `hi <= lo` | `rand.int(lo, hi): hi must be > lo` | `ValueError: empty range in randint(5, 1)` | — |
| `a / b`, `a % b` with `b == 0` | `division by zero` / `modulo by zero` (a constant right operand is a type error at check time) | `ZeroDivisionError: division by zero` | — |
| `a << n` with `n` out of range | `shift amount -1 out of range (0..64)` (a constant right operand is a type error at check time) | `ValueError: negative shift count` | — |
| integer overflow | `integer overflow in Add` | *(upward divergence: Python ints are arbitrary-precision; Chezzi's i64 faults recoverably instead of wrapping)* | — |
| `crypto.secure_bytes(n)`, `n` out of range | `secure_bytes: n must be >= 0, got -1` | *(deliberate fail-closed: a crypto primitive must not silently produce fewer bytes)* | — |
| `Range(a, b, 0)`, step is zero | `Range() step cannot be zero` | `ValueError: range() arg 3 must not be zero` | — |
| iterating a `Range` with no start | `a range with no start cannot be iterated` | *(Rust: `RangeTo` is not an iterator, a compile error)* | — |
| `r.len()` with a missing bound | `len() needs a range with a start and an end` | *(Rust: `RangeFrom` has no `len`)* | — |
| `math.lcm`/`math.gcd`/`math.abs(int)`, i64 overflow | `integer overflow in lcm` (also `in gcd` / `in abs`) | *(upward divergence: arbitrary-precision in Python; same family as the `integer overflow` row)* | — |
| `math.trunc(x)`, non-finite or out of i64 range | `trunc(): NaN is out of integer range` / `trunc(): inf is out of integer range` / `trunc(): 9223372036854776000 is out of integer range` | `ValueError: cannot convert float NaN to integer` / `OverflowError: cannot convert float infinity to integer` | — |
| `math.divmod(a, 0)` | `division by zero` | `ZeroDivisionError: division by zero` | — |
| `[...].sum()`, i64 overflow across the fold | `integer overflow in Add` | *(upward divergence: Python's `sum()` is arbitrary-precision)* | — |
| `s.repeat(n)` / `std.string.repeat(s, n)`, capacity overflow | `string repeat capacity overflow` | `OverflowError: repeated string is too long` (`"x" * n`) | — |
| `s.pad_left(w, fill)`, empty `fill` | `pad_left: fill must not be empty` | `TypeError: The fill character must be exactly one character long` (`str.ljust`) | — |
| `s.pad_left(w, fill)` / `string.pad_left(s, w, fill)`, huge `w` (capacity overflow) | `string pad capacity overflow` | `MemoryError` (`str.ljust` with a huge width tries to allocate) | — |
| `std.string.split(s, "")` / `std.string.rsplit(s, "")` | `split: sep must not be empty` / `rsplit: sep must not be empty` | `ValueError: empty separator` | — (free-fn siblings of the `s.split("")` row) |
| `crypto.token_hex(n)`, `n` out of range | `token_hex: n must be >= 0, got -1` / `token_hex: n exceeds the 1048576-byte cap, got …` | `ValueError: negative argument not allowed` (`secrets.token_hex`; no upper cap in Python) | — |
| `uuid.v4()`, OS entropy unavailable | *(not measured: no way to force `getrandom`/`/dev/urandom` to fail here)* | `OSError` from `os.urandom` (the entropy source `crypto.secure_bytes`/`token_hex` share) | — |
| `io.read_all()`, non-UTF-8 stdin | `stdin: stream is not valid UTF-8` | `UnicodeDecodeError` (`sys.stdin.read()`) | — (a mid-read I/O error is a separate, rarer fault on the same call). Its siblings `read_line`/`read_char`/`input` return `str?` **and can still fault**: see `Reader` in `std.io` |
| `datetime.days_in_month(y, m)`, `m` outside 1..12 | `days_in_month: month out of range: 13` | `calendar.IllegalMonthError: bad month number 13; must be 1-12` | — |
| `datetime.weekday_name(wd)`, `wd` outside 0..6 | `weekday_name: wd must be 0..6 (0=Sunday), got 7` | `IndexError: list index out of range` (`calendar.day_name[7]`) | — |
| `datetime.fdiv(a, 0)` / `datetime.fmod(a, 0)` | `division by zero` | `ZeroDivisionError: division by zero` | — (thin wrappers over `/`/`%`) |
| `datetime.to_uint(s)`, overflow on a long digit string | `integer overflow in Mul` | *(upward divergence: Python's `int()` is arbitrary-precision)* | — |
| `Atomic[T].add`/`sub`, `AtomicInt.add`/`sub`, i64 overflow | `integer overflow in Add` / `integer overflow in Sub` | Go 1.26: **wraps**, never panics (`atomic.Int64.Add(MAX, 1)` → `MIN`). A stated divergence, from Chezzi's language-wide recoverable-overflow rule | — |
| `Channel[T].send(v)`, closed channel | `send on a closed channel` | Go 1.26: `panic: send on closed channel` (identical) | `try_send(v) -> bool` |
| `json.stringify(j)`, depth ceiling | `json.stringify: exceeded max depth` | CPython's `json.dumps` raises `RecursionError` on a tree past its recursion limit | — |
| `json.stringify(j)`, `Json.Num` holding NaN/±inf | `cannot serialize non-finite float to JSON` | CPython's `json.dumps` emits a bare `NaN`/`Infinity` (invalid JSON, no error); Go's `json.Marshal` errors `json: unsupported value: NaN`. Chezzi matches Go | — |
| `fs.get_str`/`get_bool`/`get_int(name)` on an unregistered name | `flag: unregistered str flag --nope` (also `bool`/`int`) | CPython: `AttributeError: 'Namespace' object has no attribute 'nope'` (`argparse`); Go's `flag.Lookup` returns `None` and has no name-keyed typed getter | — |
| `ffi.load_*`/`ffi.load_*_at`/`ffi.free`, a dangling/misaligned/OOB (non-null) `ptr` | **SIGSEGV, not a recoverable Chezzi fault** | `ctypes`/`cgo` are identically unsafe | — (there is no carrier for undefined behavior) |

The last row is **not recoverable**: a bad `ptr` dereference in `std.ffi` is a raw C memory access. It
segfaults the process (or corrupts memory) before any Chezzi fault machinery runs.

Two entries **swallow** an error that the owning ancestor reports:

* **`net.Socket.close()` / `net.Listener.close()`** return `None` and drop any flush error. CPython's
  `socket.close()` also returns `None`; **Go's `Close() error` reports it** (a divergence). Contrast
  `io.Writer.close(self) -> None!`.
* **`os.setenv(key, value)`** returns `None` for any `key`/`value`, also for one that Python/Go reject
  (a key that contains `=`). Chezzi writes an in-process `Map`, not the real OS environment, so there is
  nothing to validate against. Go's `os.Setenv` returns `error`; CPython raises
  `ValueError: illegal environment variable name` (both a divergence).

Other edge cases:

- `",".join(xs)` on a non-`str` element is a **static type error** (`expected List[str], found
  List[int]`), where Python raises `TypeError` at runtime.
- `ord(s)` counts **characters, not bytes**: `ord("é")` is `233` (one char, two bytes) and `ord("éa")`
  faults.
- `chr(0xD800)` (a lone surrogate) faults, where CPython returns `'\ud800'`: a Chezzi `str` is UTF-8 and
  cannot hold a lone surrogate. Negative and `> 0x10FFFF` codes fault in both languages.
- `Reader.read_line()` on non-UTF-8 data faults on every call until you drain the line; Python raises
  once and then reports EOF. See `Reader` in `std.io`.
- `encoding.query_decode(q)` / `encoding.url_parse(u)` on a malformed percent-escape (`%FF`) do not
  fault: `query_decode("a=%FF")` keeps the raw `"%FF"` text, where CPython's `parse_qs` substitutes
  U+FFFD. `url_parse` keeps percent-escapes un-decoded, like CPython's `urlsplit`.
