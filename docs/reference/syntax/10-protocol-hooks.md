### Iteration: `Iterable[T]` and `Iterator[T]`

The `std.iter` protocols `Iterable[T]` and `Iterator[T]` are parameterized bounds that **recover** `T` from the
iterand's element (user protocols take their args explicitly). `T` then reaches the loop variable and
the return type. Recovery unifies structurally, so `Iterable[(A, B)]`, `Iterable[A?]` and
`Index[int, (A, B)]` recover every type param they mention. The two differ in what they accept (Rust's
`IntoIterator` vs `Iterator`):

- `[S: Iterable[T], T]`: **anything you can iterate.** Built-in `list`/`set`/`map`/`str`/`bytes`/
  `bytearray` (str → str, map → (key, value) pairs, a Channel → each received value), a `.iter()`
  cursor, a generator, and any struct with `next(self) -> T?`. Use this by default.
- `[S: Iterator[T], T]`: **a cursor** that holds a position, so the body may call `s.next()`. A
  `.iter()` cursor, a generator, or a struct with `next`. A raw collection does not satisfy it; pass `xs.iter()`.

```chezzi
import Iterable from std.iter

fn to_list[S: Iterable[T], T](xs: S) -> List[T]:
    out: List[T] = []
    for x in xs:            # x : T
        out.push(x)
    return out
to_list("ab")              # ["a", "b"]   (T = str)
```

`Iterable[T]` promises only `.iter() -> Iterator[T]`; `Iterator[T]` also promises `.next()`. Every
`Iterator` is `Iterable` (its `iter()` returns itself). A struct with only
`iter(self) -> Iterator[E]` satisfies `Iterable` and is for-iterable, but its element is not recovered
in bound position: bound it with a concrete arg (`[S: Iterable[int]]`) or annotate the parameter
`Iterable[int]`.

**`iter` wins over `next`.** A type with both hooks is iterated through `iter` by every consumer:
`for`, a comprehension, `List()`, `Set()`, `Map()`, a `[S: Iterable[T]]` bound and `.iter()` (Python's
`for` calls `__iter__`). A `next` that does not satisfy `Iterator`'s requirement (extra parameters, a
return that is not `E?`, no `self`) is an ordinary method: it never runs under `for` and does not stop
a conforming `iter`.

**The `Iterator[T]` bound has one element type.** In a `[S: Iterator[T], T]` body, `s.next()` runs
`next` while `for q in s` and `List(s)` run `iter`. A type whose two hooks yield different types does
not satisfy `Iterator` (it still satisfies `Iterable`). A generic `Both[T]` gets one answer for every
instantiation.

```
import Iterator from std.iter

struct Both:
    n: int
    fn next(self) -> int?: return None
    fn iter(self) -> Iterator[str]: return ["a", "b"].iter()

for q in Both(2): print(q)                 # a, b: `iter` wins
fn first[S: Iterator[T], T](s: S) -> T?: return s.next()
first(Both(2))
# error: type Both does not satisfy Iterator ('next' yields int, 'iter' yields str); ...
```

`Iterable[T]` also works as a plain parameter type (`fn f(xs: Iterable[int])`): the body may
`for v in xs`, `[v for v in xs]` or `List(xs)`. It is invariant: a `List[int]` is an `Iterable[int]`,
not an `Iterable[Any]`. A bare `Iterable` with no arg is not iterable. An `Iterable[T]` value has no
`.next()`; take a cursor with `xs.iter()`.

### One rule for every hook

A method is the hook an operator calls (`iter` / `next` for `for`, `str` for `print`, `index` /
`slice` / `set_index` for `x[k]`, `contains` for `in`, `eq` for `==`, `hash`, `compare`, the arithmetic
methods) exactly when it satisfies the builtin protocol's requirement: `self` first, the protocol's
parameter names, its parameter and return types, no type parameters of its own. Structs and enums get
hooks the same way.

- A method with a hook's name that does not satisfy the requirement is an ordinary method. The
  operator acts as if there were no hook (default display, structural `==`, the usual "cannot iterate
  / index" error), the bound `T: P` rejects the type, and a call by name (`Temp.str(21)`) works.
- A hook may use `yield`; the operator then gets the generator.
- `self` is the receiver only as the first parameter of a method in a struct, enum or protocol body.
  Any other parameter named `self` is an error.

**Near-miss warning.** A method with a receiver and a hook's name that is not the hook warns
(`'eq' on P is not the Eq hook (…); == will not call it`) where the operator would skip it silently:
`==` (structural), `print` and interpolation (default display), and `for` on a type with the other
iteration hook. A near-miss `hash` never warns: a `Set` element or `Map` key without the hook is a
check error. It is silent
where the operator rejects the type, and on a static method. The generic-operand `eq` forms
(`fn eq(self, x: T)`, `fn eq[U](self, o: U)`) warn too.

### Display: `Stringable`

The prebuilt **`Stringable`** protocol (`str(self) -> str`) sets how a value renders. A struct or enum
with `str(self) -> str` replaces its default repr (`Name(field=value, …)`, `Variant(payload)`) in
`print`, `str()` and `{…}` interpolation, also when nested in a list / tuple / map / set / enum
payload. It works as a bound (`fn show[T: Stringable](v: T)`).

`int`/`float`/`bool`/`str` satisfy `Stringable`; in an erased body `v.str()` renders as `str(v)`. This
is bound-only: `(5).str()` on a concrete scalar is a compile error (use `str(5)`).

**Every intrinsic grant is callable and equals its operator form.** Inside an erased `[T: P]` body you
may call `a.add(b)`/`a.sub(b)`/`a.mul(b)`/`a.div(b)`/`a.mod(b)`/`a.neg()` on `int`/`float`,
`a.compare(b)`, `x.hash()` on `int`/`str`/`bytes`/`bool`/a tuple, and
`c.index(k)`/`c.set_index(k, v)`/`c.slice(r)` on `list`/`map`/`str`/`bytes`/`bytearray`.

- `a.add(b)` ≡ `a + b` (same overflow / divide-by-zero fault); `c.index(k)` ≡ `c[k]` (negative
  indexing, same out-of-bounds message); `c.slice(0..2)` ≡ `c[0..2]`; `x.hash()` is the hash `x` gets
  as a key. A type that defines the method gets its own.
- Through a protocol-typed value (`x: Hashable = 5`) only grants with no `Self` argument are callable:
  `neg()`, `str()`, `hash()`, `index(k)`. `add`/`sub`/`mul`/`div`/`mod`/`compare` are rejected:
  `method 'add' is not callable through the protocol value Add — ... Bind the receiver with a generic
  parameter instead: `[T: Add]``.
- `Iterator`'s `next` on a raw collection faults (use `for`, or `.iter()`).
- `a.compare(b)` with a NaN never faults: it uses the total order of `sort()` / `.min()` / `.max()`
  (`f64::total_cmp`), while `<`/`<=`/`>`/`>=` stay IEEE (`false` for NaN). `+0.0` and `-0.0` are equal
  in that order (a stable sort keeps their order; `[0.0, -0.0].min()` is `0.0`, like CPython).

**Display-hook resolution.** `print`/`str()`/interpolation use your `str` method only when it conforms
to `Stringable`: one `self` parameter and a `str` return (written, inferred, or a `str` alias). A static
`str`, a `str` with extra parameters, or one that returns another type is not the display hook: the
value prints with the default repr, `print` never calls the method, and `obj.str(…)` / `Type.str(…)`
work as written. The checker decides this once.

```chezzi
struct Point:
    x: int
    y: int
    fn str(self) -> str:
        return "({self.x}, {self.y})"

print(Point(1, 2))            # (1, 2)        — not Point(x=1, y=2)
print("here: {Point(3, 4)}")  # here: (3, 4)
print([Point(5, 6)])          # [(5, 6)]      — dispatches when nested too
```

**A nested `str` is quoted (`str` vs `repr`).** A string on its own prints bare. Inside something
else (a list/tuple/map/set element, a struct field, an enum payload) it is quoted and escaped, as in
Python:

```chezzi
print("a, b")                 # a, b
print(["a", "b"])             # ['a', 'b']       — two elements
print(["a, b"])               # ['a, b']         — one element (was identical before)
print([""])                   # ['']             — one empty element (printed as [] before)
print({"k": "v"})             # {'k': 'v'}
print(["it's", "a\nb"])       # ["it's", 'a\nb'] — CPython quote choice + escapes
print(Point(1, 2))            # (1, 2)           — a `str` hook's output is NOT quoted
```

- The quote is `'`, or `"` when the string contains a `'` and no `"`.
- Escapes: `\\`, `\n`, `\t`, `\r`, the quote, and every non-printable character (`\xHH` / `\uXXXX` /
  `\UXXXXXXXX`, CPython's widths). Printable non-ASCII stays literal (`['é', '😀']`).
- A combining mark such as `U+0301` is escaped (CPython prints it literally).
- The rule applies inside a wrapper: `print(Shared(["a"]))` is `Shared(['a'])`. A `str(self)` hook's
  result is never quoted.

**A container that contains itself prints the back edge as `...`** (CPython's rule): `[...]`, `{...}`,
`Set(...)` or `...`. Only an object still being rendered counts, so a shared acyclic child prints in full:

```chezzi
xs: List[Any] = [1]
xs.push(xs)
print(xs)                     # [1, [...]]
m: Map[str, Any] = {}
m["a"] = m
print(m)                      # {'a': {...}}
x: List[Any] = []
print([x, x])                 # [[], []]       — shared, not cyclic
```

A `str(self)` hook is not guarded. `==` and hashing on cyclic data fault with `maximum structural
depth` (`recover:` catches it). An acyclic nest deeper than 10 000 levels faults on print.

### Hashing: `Hashable`

The prebuilt **`Hashable`** protocol (`hash(self) -> int`, and it embeds `Eq`) governs `map` keys and
`set` elements.

- `int`/`str`/`bool`/`bytes` satisfy it; a struct or enum satisfies it only by defining both
  `hash(self) -> int` and `eq(self, other: Self) -> bool` (Rust: `Hash + Eq`). No struct or enum is a
  key on its own, not even an empty struct. `float` is rejected (NaN).
- A tuple is `Hashable` when every element type is: `{(1, 2): "a"}` and `{("a", true)}` work. A
  rejection names the element (`element List[int] is not Hashable`).
- `T?` is not a key; declare an enum with `hash` and `eq`. A wrapper type takes none of `T`'s
  protocols (owner decision 2026-10-21; Rust accepts a nullable wrapper of an integer as a key).
- A struct or enum key is hashed by `hash()` and confirmed by its own `eq`.
- Contract: two keys that `==` calls equal must hash equal (see the `eq` limits in
  [Protocols](09-protocols.md#equality-eq)).
- In a `[T: Hashable]` body `x.hash()` is the hash the container uses. The number is unspecified (a
  build-dependent 64-bit value, possibly negative).

**`Hashable` embeds `Eq`, so `[T: Hashable]` is enough for a `Map`/`Set` keyed on `T`.** A
`[T: Hashable]` body may write `a == b`, `Set(xs)` or `{x: 1}`; a redundant `+ Eq` still checks and
says the same. The key check runs on every `Map[T, _]`/`Set[T]` type the signature or body spells,
even a bare mention, and a bound-free `fn h[T](xs: Set[T])` is rejected. A type whose `eq` cannot
hold for the instantiation (`Box[Tag]` with `eq ... where T: Comparable`) is not `Hashable`, so it
is no key. This is Go's `comparable` and Python's `__hash__` plus `__eq__` pairing; Rust spells it
`Eq + Hash`.

**Keys are value types (Go model).** A `struct`/`enum` key or element is deep-copied when stored, so
mutating your original after the insert cannot corrupt the stored key.

- Every insert path copies: `m[k] = v`, `{k: v}` and `{a, b}` literals, `set.insert`, `Map.insert`, `Map.update` /
  `Map.merge`, `Map(it)` / `Set(it)`.
- Map **values** are not copied. A lookup key (`m[k]`, `k in m`, `s.contains(k)`) is not copied.
  Scalar keys need no copy.
- The copy covers mutable aggregates (nested structs/enums/lists/maps/sets). A by-reference value
  inside (a closure, `Channel`, `Shared`, a live generator) stays shared; it compares by identity.
- A cyclic key, or one nested deeper than `MAX_STRUCTURAL_DEPTH` (10000), is stored by reference, so
  mutating it after insert can corrupt the collection. Use shallow acyclic keys.
- A key from `keys()` or from iterating a `map`/`set` is the stored key. Mutating it
  (`for k in m.keys(): k.x = 9`) corrupts the collection. Do not mutate keys you get from iteration.

```chezzi
struct Point:
    x: int
    y: int
    fn hash(self) -> int:
        return self.x * 31 + self.y
    fn eq(self, other: Point) -> bool:
        return self.x == other.x and self.y == other.y

label: Map[Point, str] = {}
label[Point(1, 2)] = "here"      # struct key — hashed via Point.hash
print(label[Point(1, 2)])        # here

p := Point(1, 2)
label[p] = "again"
p.x = 9                          # mutating the original can't touch the stored key
print(label[Point(1, 2)])        # still "again" — the stored key was snapshotted
```
