### `std.flag` — Go-style CLI arg parsing
Pure-Chezzi CLI parser over `os.args()` (which already omits argv[0], so `fs.parse(os.args())` is the
direct Go `flag.Parse(os.Args[1:])` analog). `import std.flag`.

`new() -> FlagSet` builds an empty set; register flags on it, then `parse` a `List[str]`:
```chezzi
fs := flag.new()
fs.str_flag("name", "world", "who to greet")   # (name, default, help)
fs.int_flag("count", 1, "how many times")
fs.bool_flag("verbose", false, "chatty output")
match fs.parse(os.args()):
    ?rest: ...                               # rest = the leftover positionals
    !e:   print(e.message())
```

| Member | Behaviour |
|---|---|
| `str_flag(name, default, help)` · `bool_flag(name, default, help)` · `int_flag(name, default, help)` | Register a flag (each mutates the set). |
| `parse(args: List[str]) -> List[str]!` | `?positionals` on success (Go's `Parse()` + `Args()` in one); a clean `!e` on an unknown flag / missing value / non-int (**never faults** on bad user input). |
| `get_str(name) -> str` · `get_bool(name) -> bool` · `get_int(name) -> int` | The registered default until `parse` overwrites it. **Panics** on an *unregistered* name (a programmer error), closer to Python argparse's `AttributeError` than to Go, whose `flag.Lookup` returns `None`. |
| `positionals() -> List[str]` | The positionals of the last `parse`. |
| `usage() -> str` | Go `PrintDefaults` style, one line per flag in registration order. It prints each flag's **registered** default (Go's `Flag.DefValue`), never the parsed value: Go 1.26.6 still prints `how many (default 1)` after `Parse(["--count","3"])`. |

`std.flag` keeps parsing flags **after** the first positional, unlike Go:
`fs.parse(["a", "--name", "x", "b"])` gives `?(["a", "b"])` with `name = "x"`. Go 1.26.6
`flag.FlagSet.Parse` stops at the first non-flag (`name` unchanged, `Args() = [a --name x b]`).
CPython 3.14.7 `argparse.parse_known_args(["a","--name","x","b"])` gives
`(Namespace(name='x'), ['a', 'b'])`, which is what this parser does. Go's rule would silently stop
parsing flags that working programs pass.

Recognized syntax (Go conventions):
- `--name value` / `--name=value` / `--verbose` (bool presence) / `--verbose=false` (the `=`-value
  accepts Go's `strconv.ParseBool` set: `1 t T TRUE true True` / `0 f F FALSE false False`).
- `--` terminator: every later token is a positional.
- `--help` and `-h` return `!("flag: help requested\n" + usage())` (the `ContinueOnError` analog of Go's
  `ErrHelp`; never prints or exits). A user-registered `help`/`h` flag wins over this, as in Go.
- One or two leading dashes are stripped, so a flag named `n` answers to **both** `-n` and `--n` (strict
  Go registers each spelling separately); a lone `-` is a positional. A third dash, `-=x` or `--=x` is
  Go's `!"bad flag syntax: <tok>"`.
- An **int** flag parses Go's `strconv.ParseInt(s, 0, 64)` syntax (what Go's `flag.Int` runs): a
  `0x`/`0o`/`0b` prefix (any case) picks the base, a leading `0` is **octal** (`-n 010` → 8), `_` is
  allowed between digits (`0_10` → 8, `0x_1f` → 31, `1_000` → 1000), and `" 3"`, `"3 "`, `08`, `1__0`,
  `0x`, `+-5` are an `!e` (`flag --n: cannot parse '08' as an integer`); a value outside the i64 range
  is an `!e` naming the range.
- `parse` **replaces** the positionals on every call (Go `FlagSet.Parse`), but never resets flag
  **values**: those persist across calls, as in Go.

Not provided: required-flag enforcement, subcommands, duplicate-registration detection.

### `std.log` — leveled logging
Pure-Chezzi leveled logger over `std.io` (Go `log`/`slog` + Python `logging`). `import std.log`.

Levels (Go `slog` order and names: `WARN`, not Python's `WARNING`): `DEBUG(0) < INFO(1) < WARN(2) <
ERROR(3)`, exposed as module fns (`log.DEBUG()` … `log.ERROR()`) so callers pass them explicitly.

```chezzi
lg := log.new()              # min level INFO, output to stderr (both are the Python/Go defaults)
lg.debug("noisy")            # DROPPED — below INFO
lg.info("served")            # → stderr: "INFO served"
lg.warn("careful")           # → stderr: "WARN careful"
lg.set_level(log.DEBUG())    # now debug() passes
```
`new(min_level: int = 1, to_stderr: bool = true) -> Logger` (default min = `INFO`, output to
**stderr**, the default of both Python `logging` and Go `log`/`slog`; pass `to_stderr=false` to route
to stdout). Methods (mutable self): `debug/info/warn/error(msg)` format `"LEVEL message"` and write to
the target, gated by the min level (a message below it is dropped); `set_level(level)` ·
`set_prefix(p)`.

**Timestamps are opt-in and injectable.** A live clock makes output non-deterministic, so the core is a
pure `format_line(level, msg) -> str` ("LEVEL message"). For a real timestamp, call `set_prefix(stamp)`
with a value you own (e.g. from `std.time` / `std.datetime`); it is prepended (with a space) to every
line, and `set_prefix("")` clears it. `std.log` imports no clock.

Not provided: handlers/formatters, hierarchical (named) loggers, structured key/value fields.

### `std.iter` — list/iterator helpers
`enumerate(xs) -> List[(int, T)]` · `zip(xs, ys) -> List[(A, B)]` · `map(xs, f)` · `filter(xs, pred)` ·
`fold(xs, init, f)` · `reduce(xs, f) -> T` · `sum(xs: List[int]) -> int` · `take(xs, n)` ·
`drop(xs, n)` · `any(xs, pred) -> bool` · `all(xs, pred) -> bool` · `find(xs, pred) -> T?` ·
`flatten(xss) -> List[T]`.

- `reduce` needs a **non-empty** list: with no seed there is no accumulator, so an empty list faults
  `reduce: empty list with no initial value` (recoverable, catchable by `recover:`). Use
  `fold(xs, init, f)` if the list can be empty.
- `sum` is **int-only** (empty → `0`): the free-function form of `xs.sum()` for the right side of a
  pipe. For a float list use the method, which is generic.

**Lazy adapters (itertools)** return a lazy `Iterator[T]` (a generator, not a `List`) and pull from
their source only as the consumer pulls, so an infinite source under `islice` terminates:
- `count(start=0, step=1) -> Iterator[int]`: infinite arithmetic counter.
- `repeat(x, n=-1) -> Iterator[T]`: `x` forever if `n<0`, else `n` times.
- `cycle(xs) -> Iterator[T]`: endlessly repeat a list's elements; an **empty list is immediately done**.
- `chain(a, b) -> Iterator[T]`: all of `a` then all of `b` (**two args only**).
- `islice(it, stop) -> Iterator[T]`: the first `stop` elements of any iterator; `stop<=0` = empty.
- `imap(it, f) -> Iterator[U]` / `ifilter(it, pred) -> Iterator[T]`: the **lazy** siblings of the eager
  `map`/`filter` (Chezzi overloads no function by argument type; only a type's `convert` may repeat).

`islice`/`imap`/`ifilter` accept **any iterable** (a list, set, str, user `next()` struct, a `.iter()`
cursor, or another generator) through the `[S: Iterable[T], T]` bound.

Random helpers call `std.rand` (seed with `rand.seed(n)` for reproducibility); they are pure Chezzi
because the native seam cannot return a generic `List[T]`. The PRNG is **predictable from its
output**, so never use them to shuffle or pick something an adversary must not guess (see `std.rand`):
`shuffle(xs) -> List[T]` (new randomly-permuted list, Fisher–Yates, non-mutating) ·
`choice(xs) -> T?` (`None` on empty) ·
`sample(xs, k) -> List[T]` (`k` elements without replacement; `k` clamped to `[0, len]`).

### `std.json` — JSON
```chezzi
enum Json:
    Null
    Bool(bool)
    Int(int)
    Num(float)
    Str(str)
    Arr(List[Json])
    Obj(Map[str, Json])
```
`parse(s) -> Json!` · `stringify(j) -> str` · `encode(x) -> str` · `is_null(j) -> bool` ·
`as_bool(j) -> bool?` · `as_float(j) -> float?` · `as_int(j) -> int?` ·
`as_str(j) -> str?` · `as_object(j) -> Map[str, Json]?` · `as_array(j) -> List[Json]?` ·
`get(j, key) -> Json?` · `at(j, i) -> Json?` · `len(j) -> int` (faults on
`Null`/`Bool`/`Num`/`Int`: a scalar has no length, like CPython's `len(None)` `TypeError`).

> **Nesting depth is capped at `MAX_NEST_DEPTH = 2000` on both `parse` and `stringify`.** Both count the
> same way (the outermost array/object is level 1 and a scalar adds no level), so every document
> `stringify` (and `encode`) returns parses back. Past the cap, `parse` returns `!"exceeded max depth"`
> (as Go's `encoding/json`; CPython raises a catchable `RecursionError`). `stringify` has no `T!E`, so a
> document built in memory past the cap **faults** with `json.stringify: exceeded max depth`
> (recoverable under `recover:`), as CPython's `json.dumps` raises `RecursionError`.
>
> The cap is stricter than CPython's (accepts 50 000) and Go's (10 000). `std.json` runs on the VM:
> each nesting level costs **2 VM frames** against the engine's 10 000-frame call-depth cap, so the
> absolute ceiling is ~5 000. `2000` leaves headroom for the caller's frames. Deeper documents are
> rejected **cleanly**. An iterative parser would lift the limit.

> **`encode(x) -> str` is the `dumps`-shaped inverse of `decode[T]`:** `decode[T](encode(x))` gives back
> `x`. It accepts bool, int, float, str, `List`, tuple, `Map` with `str` keys, a struct (in declaration
> field order), `?v`/`None`, and an already-built `Json` value (passed through unchanged). (There is no
> bare `None` *value*: `None` is a type spelling only, and `json.encode(None)` is `unknown name 'None'`;
> the JSON null you get back out is `Json.Null`, produced by `None`.) It **faults** (recoverably) on a
> `T!E` (``json.encode: cannot encode a result value (`T!E`)``), on any other enum
> (`json.encode: cannot encode enum <name>`), and on any other object (`json.encode: cannot encode <type>`).
> It has its own nesting-depth cap of 2 000, separate from `stringify`'s, because a struct is a
> reference value and may be cyclic. The cap counts BRACKETS, as `parse`/`stringify` do: the outermost
> `List`/`Map`/`Struct` is level 1; a scalar, `None`, an already-built `Json` value and a `?v` wrapper
> add no level, so a `T?`-linked chain reaches the depth a plain nested object does; an empty container
> still occupies its own level.

**Integers.** An integer-shaped JSON numeral inside the i64 window decodes to `Json.Int` and round-trips
byte-exact: `as_int` and `json.decode[int]` read it directly, never through f64, so a 19-digit id
survives intact. A numeral outside that window (or a fractional/exponent numeral) falls back to
`Json.Num` (an f64), because Chezzi has no bignum. **`as_int` and `json.decode[int]` stay total at that
fallback's float→int boundary**: they never saturate to a wrong value or fault. A number clearly
outside the `int` (i64) range (e.g. `1e30`, `18446744073709551615`) or non-finite yields `None` from
`as_int` and an `!e` from `json.decode[int]`; `i64::MAX` / `i64::MIN` still round-trip. **f64 caveat
(the `Json.Num` fallback only):** a fallback integer is an f64 (53-bit mantissa), so an input that
rounds to exactly `±2^63` (this includes `i64::MAX`/`i64::MIN` themselves and neighbours like
`9223372036854775808`) decodes to `i64::MAX`/`i64::MIN` instead of `!e`/`None`. `as_int` truncates a
fractional number (`as_int(2.5)` → `?2`).

**Non-finite floats:** standard JSON has no `NaN`/`Infinity`, so `stringify` **faults** (recoverable)
with `cannot serialize non-finite float to JSON` when a `Json.Num` holds `NaN`, `+inf` or `-inf` (Go's
`encoding/json` policy, not Python's non-standard tokens). `parse` **rejects** any numeral whose
magnitude overflows f64 (`1e400` → +inf, `-1e400` → -inf) with an `invalid number: value out of range`
error (located), so `parse`→`stringify` round-trips for every value `parse` accepts. Finite floats of
any magnitude (e.g. `1e300`) parse and stringify normally; underflow to `0.0` (`1e-400`) is accepted.

**Control chars & number grammar (RFC-8259):** `stringify` `\u00XX`-escapes control characters
`U+0000..U+001F` that lack a shorthand escape (Go's policy), so its output is always valid JSON.
`parse` **rejects** a raw control char inside a string literal (`invalid control character in string`,
located) and a leading-zero integer (`01`, `007`, `-01`) with `invalid number: leading zero` (located):
a `0` must be a lone `0`/`-0` or followed by `.`/`e`, as in Python's `json.loads`. (`0.5`, `0e1`, `10`
are valid.)

**Located parse errors:** every `parse` `!e` names a position: `<message> at line L column C (char N)`,
CPython's exact shape. A single-character error names that character; a multi-character token error
names where the TOKEN BEGAN: an unterminated string names its opening quote, a bad `true`/`false`/`null`
names the word's first letter, a malformed fraction names its `.`, a malformed exponent names its
`e`/`E`. `json.parse` on the two-line document `{"a": 1,\n "b": }` returns
`!"unexpected character '}' at line 2 column 7 (char 15)"`, the position CPython's `json.loads` gives.
The two `exceeded max depth` returns carry no location (CPython's `RecursionError` carries none). Three
cases differ from CPython 3.14: its trailing-comma diagnostics point at the comma where Chezzi points at
the bracket; an invalid string escape names the escape character where CPython names the backslash;
and `invalid number: leading zero` names the numeral's start where CPython names its second digit.

**`json.decode[T](s) -> T!`** (a `std.json` member called with one type argument, not a global builtin)
deserializes into a struct / `Map[str, V]` / `List[T]` / tuple / scalar:
- A missing key whose struct field has a default takes that default, evaluated afresh as `S(...)`
  evaluates it (a default that faults is a fault, not an `!e`).
- A missing key with no default is `!e` (`decode: missing key 'f' at $`).
- A `T?` field with no default accepts null-or-absent. `null` for any non-`T?` field is `!e`, also when
  it has a default (serde's rule; Go's `json.Unmarshal` would keep the default).
- Extra keys are ignored. Recursive/generic struct targets are rejected (use the `Json` enum).
- A **tuple** target takes a JSON array of exactly the tuple's arity (what `encode` emits), so
  `decode[(int, str)](encode((1, "x")))` round-trips (also as a struct field or a `List` element). A
  shorter or longer array is an `!e` (`decode: expected an array of 2 elements at $, found 1`).

`decode[T]` means JSON only on the `std.json` module itself (`import std.json`, or an alias such as
`import std.json as j`). Elsewhere `x.decode[T](s)` is an ordinary method call: a user type's own
`decode[T]` method runs, a local that shadows `json` is that local, and a receiver with no `decode`
method is a type error.

`json.decode[T]` is also a value of type `fn(str) -> T!` (Rust's `str::parse::<i32>` as a value):
`d := json.decode[int]` then `d("5")` is `?5`, and `["1", "2"].map(json.decode[int])` is `[?1, ?2]`.
A typed position pins `T` (`d: fn(str) -> int! = json.decode`). An unpinned `d := json.decode` is
rejected with the instantiate hint (`json.decode[<T>]`), also `[].map(json.decode)`, because each value
is compiled for its `T`. `import decode from std.json` is an error: decode has no runtime member.

A JSON *literal in Chezzi source* clashes with string interpolation, so use a raw string
(`r"""{"k": 1}"""`, verbatim, preferred) or double the braces (`"{{ }}"`); a bare `{…}` in a normal
string is interpolation.

### `std.cancel` — cooperative cancellation & timeouts
`struct Token` with methods `cancelled() -> bool` · `reason() -> str?` · `cancel() -> None` ·
`done() -> Channel[bool]` (use in `wait:`) · `deadline_at() -> float` · `derive() -> Token` (linked child).
Constructors: `manual() -> Token` · `timeout(ms: int) -> Token` · `derive(parent: Token) -> Token`.

- Registration follows Go's `context.WithCancel`: `derive()` is **O(1)**: it registers the child in its
  IMMEDIATE parent only (no ancestor walk). `cancel()` cascades **downward**, draining each node's child
  registry and marking every transitive descendant it reaches.
- A child also keeps a `parent` link that `cancelled()`/`reason()` fall back to, so an interrupted
  cascade cannot lose a subtree. Those two are therefore at best **O(depth)**, and measured end to end
  they are **QUADRATIC** in depth: 0.53 µs at depth 0, 19 µs at 25, 56 µs at 50, 178 µs at 100, 622 µs
  at 200, 2360 µs at 400 (release binary; 1 000 polls per depth). The GC mark pass re-walks the deep
  chain, so the per-ancestor cost grows with depth (see [`benchmarks.md`](../../contributing/benchmarks.md)).
  A deep `derive()` chain polled in a CPU loop degrades quadratically; Go's context error check is a
  flat O(1) at every depth.
- Cancellation is **cooperative** (see `concurrency.md`): a task blocked in a
  `std.io`/`std.fs`/`std.process`/`std.request` call sees the token only once that call returns; those
  are [uninterruptible while in flight](04-native-core.md#blocking-calls-cannot-be-interrupted).

---

> Source: native modules are Rust under `src/native/*.rs`; the pure-Chezzi modules are `.chz` files
> under `std/`. The checker and the VM dispatch built-in type methods and global builtins.
