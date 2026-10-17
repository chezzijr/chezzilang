## 5. Pure-Chezzi modules

These modules are written in Chezzi (`std/*.chz`) and use the same `import std.<name>` surface.

### `std.string` — string helpers
`is_empty(s)` · `repeat(s, n)` · `reverse(s)` · `pad_left(s, width, fill)` · `pad_right(s, width, fill)` · `split_lines(s)` ·
`ends_with(s, suffix)` · `index_of(s, sub) -> int` (or `-1`) · `count(s, sub) -> int` ·
`replace(s, old, new)` · `strip_prefix(s, p)` · `strip_suffix(s, p)`.

Except `is_empty`, this first set is also available as receiver methods on `str` (no import needed):
`s.ends_with(x)` ≡ `text.ends_with(s, x)`. See the `str` method table in §2.

**Free functions only** (no receiver-method alias; Python `str` semantics):
- `capitalize(s) -> str`: first char upper, rest lower (`"hello WORLD"` → `"Hello world"`); `""` unchanged.
- `title(s) -> str`: upper the first cased char of each word, lower the rest; any uncased char (space, digit, apostrophe) is a word boundary (`"they're"` → `"They'Re"`).
- `swapcase(s) -> str`: flip the case of each cased char; uncased chars unchanged.
- `find(s, sub, from_index) -> int`: first codepoint index of `sub` at or after `from_index`, `-1` if absent. Negative `from_index` counts from the end (`len + from_index`, clamped to `0`); `from_index` past the end → `-1` (empty `sub` → `from_index` up to `len`). `index_of(s, sub)` is exactly `find(s, sub, 0)`.
- `split(s, sep, maxsplit = -1) -> List[str]`: split from the left into at most `maxsplit + 1` pieces; `maxsplit < 0` (default) is unlimited. Empty `sep` raises a recoverable `split: sep must not be empty` fault (Python `ValueError`).
- `rsplit(s, sep, maxsplit = -1) -> List[str]`: as `split` but matching from the RIGHT (CPython `str.rsplit`): a separator that overlaps itself splits at the right end (`rsplit("aaa", "aa")` → `["a", ""]`). Empty `sep` faults.
- `split_whitespace(s) -> List[str]`: split on runs of whitespace, dropping empty pieces (Python no-arg `str.split()`): `"  a  b "` → `["a", "b"]`, `""` → `[]`.

The case fns are guaranteed for ASCII; full-Unicode case mapping follows Rust (e.g. `ß`→`SS`) and may
differ from Python. `split_whitespace`'s blank class is the native `trim`'s, which matches Python's
`str.split()` blank class exactly.

### `std.csv` — RFC 4180 CSV read/write (pure Chezzi)
`parse(text: str) -> List[List[str]]` · `format(rows: List[List[str]]) -> str`. String scanning over
the core `str` primitives; no native seam.

- `parse`: RFC 4180 quote state machine. Fields are separated by `,`, records by CRLF **or** LF. A
  double-quote-wrapped field may contain commas, CR, LF and escaped quotes (`""` inside a quoted field
  → one literal `"`). Leading/trailing spaces are significant (never trimmed). A trailing record
  separator produces **no** empty final record. Empty input → `[]`. A blank interior line → an **empty**
  record `[]`, matching CPython `csv.reader`. A record with one empty field (`[""]`) is written as a
  quoted empty field (`""`), which `format` emits and `parse` reads back distinctly from a blank line.
- **Bare quotes.** A `"` opens a quoted field **only at FIELD START**. Anywhere else it is an ordinary
  character kept **literally**: `parse("a,b\"c")` → `[["a", "b\"c"]]`,
  `parse("a,b\"c\"d")` → `[["a", "b\"c\"d"]]`, `parse("a,b\"\"c")` → `[["a", "b\"\"c"]]` (**two**
  literal quotes, because `""` collapses to one only *inside* a quoted field). This matches CPython
  `csv.reader`; Go errors (`bare " in non-quoted-field`), but `parse` has no error channel. A quote that
  *starts* the field still opens a quoted one, so `parse("a,\"b\"c")` → `[["a", "bc"]]` and
  `parse("\"a\"b,c")` → `[["ab", "c"]]`.
- `format`: the inverse. A field is quoted **iff** it contains a `,`, `"`, CR, or LF; embedded quotes
  are doubled. Each record is **terminated** by CRLF (`\r\n`, per RFC 4180), not separator-joined, so
  `format([["a","b"]])` == `"a,b\r\n"`. `parse` accepts CRLF or LF; a lone CR is field data
  (`parse("a\rb")` → `[["a\rb"]]`, Go `encoding/csv`), except a final CR at end of input, which ends the
  record. `format([])` → `""`.
- **Round-trip guarantee:** `parse(format(rows)) == rows` holds for all rows, also with an embedded
  comma, escaped quote, embedded newline, empty field, unicode, an empty record `[]` and a sole or
  trailing single-empty-field record `[""]`: `format([[]])` == `"\r\n"` and `parse("\r\n")` == `[[]]`,
  while `format([[""]])` == `"\"\"\r\n"` and `parse("\"\"\r\n")` == `[[""]]`. This matches CPython's
  `csv`.
- Not provided: streaming/Reader-based parsing, header→`Map` row mapping, and a custom-delimiter/TSV
  `parse_sep(text, sep)`.

### `std.path` — unix lexical path manipulation
Pure lexical ops on **unix `/` paths**, with **NO filesystem I/O** (that is `std.fs`). Separator: `/`
only; there is no Windows `\` handling. Edge cases follow Python `os.path` (basename/dirname/split/
splitext) and Go `path.Clean` (`normalize`). `import std.path` (or `as p`).

**`PathLike` in, `Path` out.** Every helper takes a `PathLike` (a bare `str` literal, a `bytes`, a
`bytearray`, or another `Path`; no annotation, no turbofish) and returns a **`path.Path`**. The
algorithms work on the **raw OS bytes**, so a filename that is not valid UTF-8 survives a
`basename`/`join`/`normalize`. Ops therefore **chain** (`path.with_ext(path.join(parts), "txt")`) and
you convert **once at the end** (`.str()` lossy display / `.decode()` exact / `.bytes()` raw; see
[`path.Path`](#pathpath) below). The `-> str` examples in the table show the value of `.str()` on the
returned `Path`.

| fn | signature | semantics |
| --- | --- | --- |
| `is_abs` | `(p: PathLike) -> bool` | `p` starts with `/`. `""` → `false`. |
| `is_rel` | `(p: PathLike) -> bool` | `not is_abs(p)`. |
| `basename` | `(p: PathLike) -> Path` | Final component (after the last `/`), on the **raw** string. A trailing slash yields `""`: `basename("a/b/")` → `""`, `basename("a/b")` → `"b"`, `basename("/")` → `""`, `basename("")` → `""`, `basename("a")` → `"a"`. |
| `dirname` | `(p: PathLike) -> Path` | Everything before the final component; the head's trailing slash is stripped **unless** the head is all slashes. `dirname("a/b")` → `"a"`, `dirname("a/b/")` → `"a/b"`, `dirname("/a")` → `"/"`, `dirname("a")` → `""`, `dirname("/")` → `"/"`, `dirname("")` → `""`. |
| `split` | `(p: PathLike) -> (Path, Path)` | `(dirname(p), basename(p))` as a 2-tuple, so `d, b := path.split(p)`. `split("a/b/")` → `("a/b", "")`. |
| `ext` | `(p: PathLike) -> Path` | Final extension of the basename, **including the leading dot**. A leading-dot-only hidden file has **no** ext, and only the basename is inspected: `ext("a/b.tar.gz")` → `".gz"`, `ext("a.txt")` → `".txt"`, `ext("README")` → `""`, `ext(".bashrc")` → `""`, `ext("a.")` → `"."`, `ext("dir.d/file")` → `""`. |
| `stem` | `(p: PathLike) -> Path` | `basename` with its `ext` removed: `stem("a/b.tar.gz")` → `"b.tar"`, `stem(".bashrc")` → `".bashrc"`, `stem("a.txt")` → `"a"`. |
| `with_ext` | `(p: PathLike, e: PathLike) -> Path` | Replace the final ext with `e`. A non-empty `e` gets exactly one leading dot (`"md"` ≡ `".md"`; a run of leading dots collapses to one, `"..d"` → `".d"`); `""` strips it: `with_ext("a/b.txt", ".md")` → `"a/b.md"`, `with_ext("a/b", ".md")` → `"a/b.md"`, `with_ext("a/b.txt", "")` → `"a/b"`. **Faults** on an extension that is all dots or contains `/` (CPython `PurePath.with_suffix` raises `ValueError: Invalid suffix` only for the separator case; the all-dots fault is Chezzi's own choice). Trailing `/` and `/.` are trimmed first, like `pathlib` (`with_ext("a/", "txt")`, `("a//", …)`, `("a/.", …)` → `"a.txt"`; `("/a/", …)` → `"/a.txt"`; `("a/..", …)` → `"a/...txt"`), and a path with **no name** (`""`, `"."`, `"./"`, `"/"`, `"//"`, `"/."`) **faults** `with_ext: path has an empty name: '…'` (CPython `ValueError: … has an empty name`). |
| `normalize` | `(p: PathLike) -> Path` | Go `path.Clean` lexical clean (no filesystem): collapse `//`, drop `.`, resolve `..` against the preceding real element. `""` → `"."`; a leading `..` is **kept** on a relative path, but a `..` past root on an **absolute** path is dropped. `normalize("/")` → `"/"`, `normalize("//")` → `"/"`, `normalize("..")` → `".."`, `normalize("a/b/../c")` → `"a/c"`, `normalize("a/./b")` → `"a/b"`, `normalize("a/b/")` → `"a/b"`, `normalize("./a")` → `"a"`, `normalize("/..")` → `"/"`, `normalize("/a/../../b")` → `"/b"`, `normalize("a/../../b")` → `"../b"`. |
| `join` | `[T](parts: List[T]) -> Path where T: PathLike` | **Go `path.Join` style** (NOT Python's absolute-resets-earlier behavior): drop empty parts, join with `/`, then `normalize`. All-empty → `""`: `join(["a","b","c"])` → `"a/b/c"`, `join(["a/","b"])` → `"a/b"`, `join(["","b"])` → `"b"`, `join([])` → `""`, `join(["a","","c"])` → `"a/c"`, `join(["/a","b"])` → `"/a/b"`. **Generic over the element type, not `List[PathLike]`**: Chezzi containers are invariant, so a `List[PathLike]` parameter would accept only a list *literal*. Any **homogeneous** list works: `path.join(xs)` for `xs: List[str]`, `path.join(names)` for `names: List[Path]`. A *heterogeneous* literal (`[a_path, "sub"]`) has no single element type and is rejected at the literal. |

<a id="pathpath"></a>
#### `path.Path` — the OUTPUT position of the filesystem surface

An **ordinary Chezzi struct** over the raw OS bytes (`raw: bytes`). `fs.list_dir`/`walk`/`glob`/
`canonicalize`, `os.getcwd()` and every `std.path` helper return it, and it satisfies `PathLike`
structurally, so `fs.exists(p)` takes it directly.

| method | signature | semantics |
| --- | --- | --- |
| `bytes` | `(self) -> bytes` | The raw OS bytes. Byte-exact, never faults. |
| `decode` | `(self) -> str` | **EXACT** conversion. Recoverable **fault** on a non-UTF-8 path (`invalid UTF-8 in decode()`), the fault `bytes.decode()` raises. |
| `str` | `(self) -> str` | **LOSSY display** (`Stringable`): each maximal invalid UTF-8 subsequence → one `U+FFFD`. **Never faults**, so `print(p)` / interpolation always work. |
| `as_path` | `(self) -> bytes` | `PathLike` conformance. |

`Path` splits DISPLAY (`str`) from CONVERSION (`decode`), as Rust does (its `Path` implements no
`Display`). The constructor takes any `PathLike` (`str`, `bytes`, `bytearray`, another `Path`):
`path.Path("/a/b")` and `path.Path(b"/a/b")` build the same value. Convert to a mutable buffer with
`bytearray(p.bytes())` (there is no `bytearray` method).

> **Hazard:** `fs.exists(p.str())` on a non-UTF-8 path names a *different* (usually nonexistent) file,
> because the display is lossy. `PathLike` accepts a `Path` **directly**, so write `fs.exists(p)`. Use
> `.str()` only to *display* a path.

### `std.datetime` — civil-calendar date/time (UTC-only)
Pure-Chezzi civil-calendar decomposition, construction and duration arithmetic on top of the native
`std.time` clock (`time.now_ms()` only). It uses pure integer math (Howard Hinnant's branch-free
civil-calendar algorithms), so results are **identical across runs**. `import std.datetime` (or `as dt`).

**Contract:**
- **UTC ONLY.** Every `DateTime` is UTC. There is **NO timezone / DST handling and NO tz database**.
- **WEEKDAY ORIGIN = `Sunday=0`, Monday=1, …, Saturday=6.** This matches the native `std.time` civil
  math (epoch 0 == 1970-01-01 is a **Thursday == weekday 4**). It differs from Python's
  `datetime.weekday()` (Monday=0).
- **NEGATIVE epochs (pre-1970) are correct.** Chezzi `/` and `%` truncate **toward zero**, so every
  split of an epoch into days+seconds uses the internal `fdiv`/`fmod` **floor-division** helpers.
  `from_epoch(-1)` → 1969-12-31 23:59:59 (Wednesday); `to_epoch` round-trips it.
- **DURATION arithmetic operates on epoch INTS (seconds), not `DateTime`**. Calendar-aware work goes
  through `from_epoch`/`to_epoch`.

```chezzi
struct DateTime:
    year: int
    month: int
    day: int
    hour: int
    minute: int
    second: int
    weekday: int    # 0=Sunday .. 6=Saturday (contractual)
    milli: int = 0  # 0..999, milliseconds past `second`; APPENDED, so 7-arg construction still works
```

| fn | signature | semantics |
| --- | --- | --- |
| `from_epoch` | `(epoch: int) -> DateTime` | Decompose Unix epoch-seconds (UTC) into a `DateTime`. Negative epochs floored. |
| `to_epoch` | `(dt: DateTime) -> int` | Recompose to Unix epoch-seconds. `to_epoch(from_epoch(e)) == e`. |
| `to_epoch_ms` | `(dt: DateTime) -> int` | Recompose to Unix epoch **milliseconds**: `to_epoch(dt) * 1000 + dt.milli`. `to_epoch` stays seconds. |
| `now` | `() -> DateTime` | Current UTC date/time, millisecond precision (`from_epoch(time.now_ms()` seconds `)` plus `milli`); the only clock use. |
| `days_from_civil` | `(y, m, d) -> int` | Days since 1970-01-01 (Hinnant). `(1970,1,1)`→0, `(1969,12,31)`→-1, `(2024,2,29)`→19782. A month outside `1..12` or a day outside `1..days_in_month(y, m)` **faults** (`days_from_civil: month out of range: 13`), like CPython `date()`; it never normalizes `(2023,13,1)` to 2024-01-01. `from_epoch`/`to_epoch` round-trip the whole i64 range, including within 86 399 s of `i64::MIN`. |
| `civil_from_days` | `(z) -> (int, int, int)` | Inverse: `(year, month, day)` tuple. `0`→`(1970,1,1)`, `-1`→`(1969,12,31)`. |
| `is_leap_year` | `(y) -> bool` | Proleptic Gregorian: `2000`→true, `1900`→false, `2024`→true. |
| `days_in_month` | `(y, m) -> int` | Leap-aware. `(2024,2)`→29, `(2023,2)`→28, `(2024,4)`→30. A month outside `1..12` **faults** (recoverable via `recover:`), like Python `calendar.monthrange`. |
| `weekday` | `(epoch: int) -> int` | Weekday (Sunday=0..Saturday=6) of an epoch value. `weekday(0)`→4 (Thu). |
| `weekday_name` | `(wd: int) -> str` | English name: `weekday_name(0)`→`"Sunday"`, `weekday_name(4)`→`"Thursday"`. |
| `to_iso8601` | `(dt) -> str` | `"YYYY-MM-DDTHH:MM:SSZ"`; `".SSS"` goes before the `Z` **only when `milli != 0`** (CPython `isoformat`'s rule): `"2026-09-21T12:34:56.789Z"`. `from_epoch(0)`→`"1970-01-01T00:00:00Z"`. |
| `to_date_string` | `(dt) -> str` | `"YYYY-MM-DD"`. |
| `to_time_string` | `(dt) -> str` | `"HH:MM:SS"`. |
| `to_string` | `(dt) -> str` | `std.time.format` style `"YYYY-MM-DD HH:MM:SS"`. |
| `parse_iso8601` | `(s: str) -> DateTime!` | The **inverse** of `to_iso8601`. See below. |
| `add_seconds` | `(epoch, n) -> int` | `epoch + n`. |
| `add_days` | `(epoch, n) -> int` | `epoch + n*86400` (negative `n` subtracts). |
| `diff_seconds` | `(a, b) -> int` | `a - b`. |
| `diff_days` | `(a, b) -> int` | Whole days `b`→`a`, **floored**: `diff_days(-1, 0)` → -1. |

`parse_iso8601` parses ISO-8601 / RFC-3339, a SUBSET of Python's `datetime.fromisoformat`:
- Date: `"YYYY-MM-DD"` **or** the ISO basic `"YYYYMMDD"` (date-only means midnight).
- Time: `"HH:MM:SS"`, `"HH:MM"` (seconds default 0), `"HHMMSS"` or `"HHMM"`, after a `'T'` **or** `' '`.
- Offset (optional): `'Z'`, `'+HH:MM'`/`'-HH:MM'`, or `'+HHMM'`/`'-HHMM'`, **normalized to UTC** (Go
  `time.Parse`).
- Fraction (optional): `.fff`. The first three digits become `DateTime.milli`; a shorter fraction is
  right-padded (`.5` → 500); digits past the third are **truncated**, not rounded (`.9999` → 999), as
  CPython `fromisoformat`.
- Malformed or out-of-range fields (month 13, day 32, hour 25, second 60, non-digits, wrong widths) are
  a **clean `!e`**, never a fault. Every field is **width-checked**: month/day/time are exactly 2 digits
  and the year is **4+** digits (as `to_iso8601` pads), so `"24-01-01"` is an `!e`, not year 24.
- Round-trips: `parse_iso8601(to_iso8601(dt)) == dt` for every year of 9 digits or fewer (a wider year,
  reachable only from an epoch near the `int` limit, `!e`s).
- CPython 3.14.7 accepts more: an hour-only time (`"...T12"`), a `"+HH"` offset, a `"+HH:MM:SS"` offset,
  and ISO week dates (`"2024W011"`) are all `!e` here.
- Two limits under the UTC-only contract: precision below one millisecond is truncated, and a non-`Z`
  offset normalizes to a UTC epoch instead of round-tripping to itself.

Not provided: `strftime`-pattern formatting and a general `strptime`/`from_string`. The `DateTime`
struct lives in the module (`datetime.DateTime`); if your program also defines a top-level
`struct DateTime`, use the module-qualified name.

### `std.collections` — generic single-threaded data structures
Pure-Chezzi generic structs over `T` on top of the builtin `list`/`map`, with no native surface.
`import std.collections` (or `as col`).

**Empty semantics:** every removal/peek returns `T?`. An empty container yields `None`, never a fault,
like the builtin `list.pop() -> T?`.

**`Heap[T]`**: a binary heap over a backing `List[T]` with a comparator **closure**. The comparator
contract: `less(a, b) == true` means `a` is **more extreme** than `b`, so `a` pops **first**. Pass
`fn(a,b): a < b` for a **min-heap** (smallest first) and `fn(a,b): a > b` for a **max-heap**. This works
for any `T` (floats, custom priorities, `(priority, item)` tuples) with **no `Comparable` impl**
required. With an empty backing list the element type `T` cannot come from the data, so the binding,
return or parameter **annotation** supplies it: `h: Heap[int] = Heap([], fn(x, y): x < y)` type-checks
(the `Heap[int]` pins `T=int`, which gives the comparator `x, y: int`); a turbofish
`Heap[int]([], …)` or annotated comparator params (`fn(x: int, y: int): …`) work too.

| member | signature | semantics / complexity |
| --- | --- | --- |
| `Heap` | `Heap(data: List[T], less: fn(T,T)->bool)` | Raw constructor; `Heap([], cmp)` for an empty heap with comparator `cmp`. |
| `min_heap` | `() -> Heap[int]` | Int min-heap factory (`a < b`). |
| `max_heap` | `() -> Heap[int]` | Int max-heap factory (`a > b`). |
| `from_list` | `(xs, less) -> Heap[T]` | Heapify (push-loop, **O(n log n)**, NOT bottom-up O(n)); `xs` untouched. |
| `.push(x)` | `(T) -> None` | Sift-up. **O(log n)**. |
| `.pop()` | `() -> T?` | Remove+return the extremum (sift-down), `None` if empty. **O(log n)**. |
| `.peek()` | `() -> T?` | The extremum without removing, `None` if empty. **O(1)**. |
| `.len()` / `.is_empty()` | `() -> int` / `-> bool` | **O(1)**. |

**`Deque[T]`**: double-ended queue, **amortized O(1) at both ends** with the **two-stack** design
(`front`/`back` backing lists; a pop whose near stack is empty drains the far stack into it once, so
each element moves between stacks at most once). `peek` reads the head/tail without rebalancing, so
peek is worst-case O(1). Construct directly, with the type on the same statement:
**`d: Deque[int] = Deque([], [])`** or **`Deque[int]([], [])`**; an untyped `d := Deque([], [])` is an
error on that line. (There is no `deque()` factory: a no-argument generic factory cannot bind `T`.)

| member | signature | semantics / complexity |
| --- | --- | --- |
| `.push_front(x)` / `.push_back(x)` | `(T) -> None` | **O(1)**. |
| `.pop_front()` / `.pop_back()` | `() -> T?` | Remove+return the head/tail, `None` if empty. **Amortized O(1)**. |
| `.peek_front()` / `.peek_back()` | `() -> T?` | Head/tail without removing, `None` if empty. **O(1)**. |
| `.len()` / `.is_empty()` | `() -> int` / `-> bool` | **O(1)**. |

**`Counter[T: Hashable]`**: a frequency table over `Map[T, int]` (`T` must be `Hashable`,
like any map key; `Hashable` embeds `Eq`). Construct directly:
**`c: Counter[str] = Counter({})`** or **`Counter[str]({})`**; an untyped `c := Counter({})` is an
error on that line. (No `counter()` factory, for the same reason as `Deque`.)

| member | signature | semantics / complexity |
| --- | --- | --- |
| `.add(x)` | `(T) -> None` | `add_n(x, 1)`. **O(1)**. |
| `.add_n(x, n)` | `(T, int) -> None` | Increment by `n` (creates the entry if absent; `n` may be negative). **O(1)**. |
| `.count(x)` | `(T) -> int` | Count of `x`, **0 if never added**. **O(1)**. |
| `.total()` | `() -> int` | Sum of all counts. **O(n)**. |
| `.most_common(k)` | `(int) -> List[(T, int)]` | Top `k` `(item, count)` pairs by **descending count**; `k` clamped to `[0, len]` (`k<=0`→`[]`, `k>=len`→all). **O(n log n)**. |

**Counter tie-break:** equal counts keep **insertion order**, because `map.keys()` yields insertion
order **and** the list `sort_by` is a **stable** merge sort. Do not swap `sort_by` to an unstable sort.

**No ordered-map wrapper:** the builtin `map` is **already insertion-ordered**
(`map.keys()`/`values()`/`for k,v in m` iterate in insertion order; `std.json`'s round-trip relies on
it). Use the builtin `map`. There is no `OrderedMap` (and no move-to-end / LRU `popitem` primitive).
