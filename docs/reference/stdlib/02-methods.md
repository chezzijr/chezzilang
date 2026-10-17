## 2. Methods on built-in types

### `str`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | Character (codepoint) count. |
| `upper` / `lower` | `() -> str` | Case-mapped copy. |
| `trim` | `() -> str` | Strip leading/trailing whitespace: Unicode `White_Space` plus U+001C..U+001F, matching CPython's `str.isspace()`. |
| `split` | `(sep: str) -> List[str]` | Split on `sep`. Yields `separators + 1` pieces, so the empty string splits to a one-element list holding `""` (`"".split(",")` → `[""]`, length 1), matching Python/Go/Rust/JS. An empty `sep` raises a recoverable `split: sep must not be empty` fault (Python `ValueError`; matches `std.string.split`). |
| `chars` | `() -> List[str]` | One-character strings. |
| `starts_with` | `(prefix: str) -> bool` | |
| `ends_with` | `(suffix: str) -> bool` | Empty suffix is always true. |
| `contains` | `(sub: str) -> bool` | Substring test. |
| `join` | `(xs: List[str]) -> str` | Join `xs` with the receiver as the separator. |
| `replace` | `(old: str, new: str) -> str` | Replace every non-overlapping `old`; empty `old` matches at every codepoint boundary, so `new` lands between every codepoint and at both ends (`"abc".replace("", "-")` → `"-a-b-c-"`, matching CPython and Go). |
| `repeat` | `(n: int) -> str` | `n <= 0` → `""`. Raises a recoverable `string repeat capacity overflow` fault if `n * len` would exceed allocatable capacity. |
| `reversed` | `() -> str` | A NEW string with the codepoints in reverse order (there is no `s[::-1]` or `s.reverse()`). |
| `pad_left` | `(width: int, fill: str) -> str` | Left-pad to `width` codepoints; never shrinks (`width` ≤ len → unchanged). A multi-char `fill` is a repeating cycle truncated to fit, so the result is **exactly** `width` codepoints (`"a".pad_left(4, "xy")` → `"xyxa"`). An empty `fill` raises a recoverable `pad_left: fill must not be empty` fault. Raises a recoverable `string pad capacity overflow` fault if the pad would exceed allocatable capacity. |
| `pad_right` | `(width: int, fill: str) -> str` | Right-pad (CPython `str.ljust`): the mirror of `pad_left` with the fill after the text (`"ab".pad_right(5, ".")` → `"ab..."`); same never-shrink rule, exact-width cycle, and faults (`pad_right: fill must not be empty`, `string pad capacity overflow`). For a runtime column width either this or `"{s:<{w}}"` works. |
| `index_of` | `(sub: str) -> int` | First **codepoint** index, `-1` if absent, `0` for empty `sub`. A **sentinel, not a carrier**: see the `-1` hazard under `List.index_of`. |
| `count` | `(sub: str) -> int` | Non-overlapping occurrences; empty `sub` → codepoint length + 1 (`"abc".count("")` → `4`), matching Python/Go/`std.string.count`. |
| `strip` | `() -> str` | Trim alias (strip leading/trailing whitespace). |
| `strip_prefix` | `(p: str) -> str` | Remove `p` from the front if present, else unchanged. |
| `strip_suffix` | `(p: str) -> str` | Remove `p` from the end if present, else unchanged. |
| `split_lines` | `() -> List[str]` | Split on `"\n"`, `"\r\n"` or a lone `"\r"`; a trailing terminator yields no final empty piece (Python `str.splitlines()`). It does NOT split on `\v`, `\f`, U+0085, U+2028 or U+2029. |
| `to_int` | `() -> int?` | Safe parse (trims first, accepts PEP-515 underscores between digits): `?n` or `None` on bad input. ASCII digits only: `"١٢"`/`"１２"` are `None` (Go/Rust semantics; CPython's `int()` accepts Unicode decimal digits). Same for `to_float` and `math.parse_int_base`. |
| `to_float` | `() -> float?` | Safe parse (trims first, accepts PEP-515 underscores between digits): `?f` or `None` on bad input. |
| `parse_int` | `() -> int!str` | `T!E`-returning parse (trims first, accepts PEP-515 underscores between digits): `?n` or `!msg` with a human-readable parse-error message. The error-message sibling of `to_int`, and ASCII-digits-only like it. |
| `parse_float` | `() -> float!str` | `T!E`-returning parse (trims first, accepts PEP-515 underscores between digits): `?f` or `!msg`. The error-message sibling of `to_float`. |
| `encode` | `() -> bytes` | UTF-8 encode (always succeeds). |
| `message` | `() -> str` | Returns self, so a bare `str` satisfies the `Error` protocol. |

The `ends_with`/`replace`/`repeat`/`reversed`/`pad_left`/`pad_right`/`index_of`/`count`/`strip_prefix`/
`strip_suffix`/`split_lines` methods are receiver-method aliases of the same-named `std.string` free
fns. `s.replace(a, b)` and `text.replace(s, a, b)` (after `import std.string as text`) give identical
results, and the free fns delegate to the native methods, so a huge `n`/`width` raises the same
recoverable capacity fault in both spellings. The empty-`old` rule of `replace` also holds in both.

### `List[T]`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | |
| `push` | `(x: T) -> None` | *mutates*: append. |
| `pop` | `() -> T?` | *mutates*: remove & return last (`None` if empty). |
| `reverse` | `() -> None` | *mutates*: reverse in place (`List` and `bytearray`; `bytearray` also has `reversed() -> bytearray`, and `bytes.reversed() -> bytes`). |
| `contains` | `(x: T) -> bool` | |
| `index_of` | `(x: T) -> int` | First index, or `-1`. A **sentinel, not a carrier**: see the `-1` hazard below. |
| `concat` | `(other: List[T]) -> List[T]` | Returns a **new** list. Operator form: `a + b`. |
| `extend` | `(other: List[T]) -> None` | *mutates*: append all of `other`. |
| `sum` | `() -> T` | Numeric lists (`int`→`int`). Integer sums use checked add: overflow raises a recoverable `integer overflow in Add`, never wraps. Any-float lists accumulate to `float` with CPython 3.12+'s Neumaier compensation (ints included: `[0.1, 0.2, 0.3].sum()` is `0.6`, `[1e16, 1.0, 1.0].sum()` is `1.0000000000000002e+16`; may reach `inf`, never `nan` from the compensation). The ELEMENT TYPE decides the numeric kind, so an EMPTY list sums to `0.0` for `List[float]` and to `0` for `List[int]`. |
| `sort` | `() -> None` | *mutates*: ascending. Orderable elements (`int`/`float`/`str`, or tuples and `List`s of orderable elements, lexicographic) or `Comparable` structs or enums. A `T?` element is not orderable. Float `NaN` uses a total order (`NaN` sorts to one end), deterministic; `+0.0` and `-0.0` are Equal (input order kept, like CPython). Faults if the callback mutates the receiver (only reachable via a `Comparable` struct's or enum's `compare`). |
| `sort_by` | `(cmp: fn(T, T) -> int) -> None` | *mutates*: custom comparator (`<0`, `0`, `>0`). Faults if the callback mutates the receiver. |
| `sort_by_key` | `(key: fn(T) -> K) -> None` | *mutates*: sort by a derived orderable/`Comparable` key. A `NaN` float key sorts deterministically (total order, `NaN` to one end), consistent with `sort()`. Faults if the callback mutates the receiver. |
| `map` | `(f: fn(T) -> U) -> List[U]` | Returns a new list. |
| `filter` | `(pred: fn(T) -> bool) -> List[T]` | Returns a new list. |
| `fold` | `(init: U, f: fn(U, T) -> U) -> U` | Left fold. |
| `min` / `max` | `() -> T?` | Smallest / largest element by natural order (`int`/`float`/`str`, a tuple or `List` of orderable elements, lexicographic, or a `Comparable` struct or enum; a `T?` element is not orderable), wrapped in `?v`. Ties resolve to the first-seen element. Empty list is `None`, never a fault. Float `NaN` uses the same total order as `sort()`. |
| `min_by` / `max_by` | `(key: fn(T) -> K) -> T?` | The **element** whose derived key `K` (orderable/`Comparable`) is smallest / largest, wrapped in `?v`; first-seen ties. Empty list is `None`, never a fault. |
| `first` / `last` | `() -> T?` | The first / last element, `None` if empty. Non-mutating. |
| `reversed` | `() -> List[T]` | Returns a **new** reversed list; the receiver is untouched (contrast in-place `reverse`). |
| `insert` | `(i: int, x: T) -> None` | *mutates*: insert `x` before index `i`. Python-clamped: `i > len` appends, negatives are length-relative and clamp to `0`; never faults. |
| `remove_at` | `(i: int) -> T` | *mutates*: remove & return the element at index `i` (Python-relative negatives). A true out-of-range index **faults** (`index {i} out of bounds (len {n})`). |
| `unique` | `() -> List[T]` | Returns a **new** list with all duplicates removed, first-occurrence order kept (Python `dict.fromkeys`). Structural equality; never mutates the receiver. One pass over a hash index when every element is a flat scalar key; a container, `bytearray`, or user-`eq` element falls back to a linear scan with the same result. |
| `dedup` | `() -> List[T]` | Returns a **new** list collapsing only **consecutive** duplicate runs (Rust `Vec::dedup`); non-adjacent duplicates survive. |
| `chunk` | `(n: int) -> List[List[T]]` | Consecutive fixed-size chunks (final chunk short if `len` not divisible). `n <= 0` **faults** (`chunk size must be positive, got {n}`). |
| `windows` | `(n: int) -> List[List[T]]` | Sliding windows of size `n` (Rust `slice::windows`). `n > len` yields an **empty** list; `n <= 0` **faults** (`window size must be positive, got {n}`). |
| `take_while` | `(pred: fn(T) -> bool) -> List[T]` | Returns a **new** list of the leading prefix while `pred` holds (stops at the first false). |
| `drop_while` | `(pred: fn(T) -> bool) -> List[T]` | Returns a **new** list of the suffix after the leading prefix where `pred` holds. |
| `count` | `(pred: fn(T) -> bool) -> int` | Number of elements satisfying `pred`. |
| `position` | `(pred: fn(T) -> bool) -> int?` | Index of the **first** element satisfying `pred` (`None` if none). The **carrier** twin of `index_of`: a miss is `None`, not a usable `-1`. |
| `copy` | `() -> List[T]` | Returns a **new** list holding the same elements (**shallow**, Python `list.copy()`): mutating the copy leaves the receiver alone, but a nested `List`/`Map`/struct element is shared. |

> **The `-1` miss hazard (`List.index_of`, `str.index_of`).** These two return a **sentinel**, and
> indexing is Python-negative, so `xs[xs.index_of(v)]` on a miss yields the **last** element instead of
> faulting: `[1,2,3][[1,2,3].index_of(9)]` → `3`; `"hello"["hello".index_of("zz")]` → `'o'`. Python has
> the same trap (`'hello'['hello'.find('zz')]` → `'o'`). When a miss must not be usable, use the
> carrier: `List.position(pred) -> int?`, `Map.get(k) -> V?`, or a `contains` guard. (Python's
> `list.index(x)` raises `ValueError`; Chezzi gives `List` and `str` one shape.)

The predicate/callback methods (`map`/`filter`/`fold`/`take_while`/`drop_while`/`count`/`position`)
iterate over a **snapshot** of the receiver taken at call time. A callback that mutates the receiver
(`xs.pop()`/`xs.push(..)`) does not change the iteration sequence and never faults, as with
comprehensions and Python `map`/`filter`. The mutation survives.

The three `sort` variants also sort a snapshot, but they **write the result back**, so a callback's
mutation would vanish. `sort`/`sort_by`/`sort_by_key` therefore **fault** if the callback mutates the
receiver, matching CPython's `ValueError: list modified during sort`:

```chezzi
w := [3, 1, 2]
fn bump(x: int) -> int:
    w.push(100)
    return x
r := recover: w.sort_by_key(bump)
match r:
    !e: print(e.message())   # list modified during 'sort_by_key' -- a sort callback must not
                                  # mutate the list being sorted
    ?_: assert false
```

Keep callbacks pure; if you need both, sort a copy and merge after.

### `Map[K, V]`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | |
| `has` | `(key: K) -> bool` | |
| `get` | `(key: K) -> V?` | |
| `keys` | `() -> List[K]` | Insertion order. |
| `values` | `() -> List[V]` | Insertion order. |
| `items` | `() -> List[(K, V)]` | Insertion order, the same order as `keys()`, so `Map(m.items()) == m`. Returns a fresh list (a snapshot, like `keys()`; CPython returns a live view). |
| `remove` | `(key: K) -> V?` | *mutates*: returns the removed value, or `None`. |
| `merge` | `(other: Map[K, V]) -> Map[K, V]` | Returns a **new** map (`other` wins on key clash). |
| `update` | `(other: Map[K, V]) -> None` | *mutates*: merge `other` into self. |
| `copy` | `() -> Map[K, V]` | Returns a **new** map, shallow in the values (Python `dict.copy()`); struct/enum keys are snapshotted, as `merge` does. |

Index a map with `m[k]` (read/write); iterate with `for k, v in m:`.

### `Set[T]`
| Method | Signature | Notes |
|--------|-----------|-------|
| `len` | `() -> int` | |
| `has` | `(x: T) -> bool` | |
| `add` | `(x: T) -> None` | *mutates*: idempotent insert. |
| `remove` | `(x: T) -> bool` | *mutates*: returns whether it was present. |
| `union` / `intersection` / `difference` | `(other: Set[T]) -> Set[T]` | Return a **new** set. Operator forms: `a \| b` / `a & b` / `a - b`. |
| `copy` | `() -> Set[T]` | Returns a **new** set (shallow, Python `set.copy()`); struct/enum elements are snapshotted, as `add` does. |

> **Set operators.** `\| & - ^` on two `Set[T]` are union / intersection / difference /
> symmetric-difference, identical to the methods above (`^` has no method form). Lists support `+`
> (concat) and `*` (repeat); see [`syntax.md` §4](../syntax/03-operators.md).

> **Cyclic keys fault (recoverably).** Map/Set membership and key-equality (`has`, `get`, `remove`,
> `add`, `m[k]`, `in`, set algebra, and `List.contains`/`index_of`/`unique`/`dedup`) are *defined by*
> `==`, so a key holding a reference cycle raises the same recoverable *"maximum structural depth
> (10000) exceeded"* fault that `a == b` does, never a silent `false`. This matches Python's
> `RecursionError`. Catch it with `recover:`. The 10 000 is **one budget shared across nested
> re-entries**: a user `eq`/`str` that compares or stringifies inside its own body continues on the
> depth the enclosing walk already used, as CPython's single recursion budget does. This keeps a deep
> hook nesting from overflowing the host stack.

### `bytes` (immutable) and `bytearray` (mutable)
| Type | Method | Signature | Notes |
|------|--------|-----------|-------|
| both | `decode` | `() -> str` | UTF-8 decode (recoverable fault on invalid UTF-8). |
| `bytes` | `decode_lossy` | `() -> str` | UTF-8 decode with each **maximal invalid subsequence** replaced by `U+FFFD` (Python's `b.decode(errors="replace")`, Rust's `String::from_utf8_lossy`). **Never faults**, so it is the DISPLAY twin of `decode` (`path.Path.str()` uses it). Not injective: use `decode` when the exact bytes matter. |
| both | `len` | `() -> int` | Byte count. |
| `bytearray` | `push` | `(byte: int) -> None` | *mutates*: append a byte (0–255). |
| `bytearray` | `pop` | `() -> int?` | *mutates*: remove & return last byte. |
| `bytearray` | `extend` | `(other: bytes \| bytearray \| List[int]) -> None` | *mutates*: append all of `other`. |
| `bytearray` | `copy` | `() -> bytearray` | Returns a **new** bytearray (Python `bytearray.copy()`). |

Index either with `b[i]` (byte as `int`, 0–255); `bytearray` also supports `b[i] = byte`. **Both types
slice** (`b[1..3]`, byte offsets), and a slice keeps the receiver's type: `bytes[1..3]` is `bytes`,
`bytearray[1..3]` is a fresh `bytearray`. `for x in b` yields `int`.

- `==`/`!=` are structural, also across the two types (`bytes == bytearray` compares content, as in
  Python).
- `bytes` is `Hashable` (a valid map/set key). `bytearray` is **not** (mutable, like `list`).
- `str(b)` / `print(b)` / interpolation use the Python repr: `b'...'` and `bytearray(b'...')`.
- A `bytearray` crosses the task airlock by value (a deep copy, like `list`).
- UTF-8 only: there is no encoding-name argument, no `byte`/`u8` scalar and no non-UTF-8 codec
  (latin1/utf16).

A `bytearray` is **not** assignable to a `bytes` slot (it is mutable, so an alias under an immutable
`bytes` type would change under you). Convert with **`bytes(ba)`** (an explicit copy, like CPython's
`bytes(ba)`); `bytearray(b)` copies the other way. That is how a built-up buffer reaches the binary
APIs (`io.write_bytes`, `crypto.sha256_bytes`, `encoding.base64_encode_bytes`, `Socket.write_bytes`).
