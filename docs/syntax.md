# Chezzi — Syntax Reference

A scannable cheat-sheet for the whole language. Revise from this; feed it to an LLM as context.
For the *why* behind each choice see [`spec.md`](spec.md); for token names see [`../src/lexer/mod.rs`](../src/lexer/mod.rs).

> **Implementation status:** the language is fully designed but built incrementally.
> Tags like `(M3)` mark which milestone first makes a feature *run*. Syntax is stable regardless.

---

## 1. Lexical basics

```chezzi
# comments start with '#' and run to end of line
```

- **Blocks = indentation** (spaces only; tabs are a lex error). A block opens after a `:` line.
- **Logical lines** end at a newline. Blank / comment-only lines are ignored.
- **Identifiers:** letter or `_`, then letters/digits/`_`. Case-sensitive.

## 2. Literals

```chezzi
42            # int   (i64)
1_000_000     # int   — '_' is a digit-group separator (only between digits)
0xFF 0b1010 0o17   # int — hex / binary / octal literals ('_' ok between digits)
3.14          # float (f64)
1_234.567_8   # float — '_' works in both parts
6.022e23  1e3  1.5e-9  2E10  1e+5   # float — scientific notation (any exponent ⇒ float, so 1e3 = 1000.0)
true  false   # bool
"hello"       # str
'hello'       # str — single quotes are equivalent to double (same escapes & interpolation)
"hi {name}"   # str with interpolation — see §10
"emoji \u{1F600}, A=\u{41}"   # str — \u{HEX} unicode escape (1-6 hex digits)
[1, 2, 3]     # list[int]
{"a": 1}      # map[str, int]
```

**Multi-line literals & trailing commas.** Inside `[]`, `{}`, and `()` the layout (newlines /
indentation) is suppressed, so a collection literal, a call's arguments, or a function's parameter
list may span lines. A single **optional trailing comma** is allowed before the closing delimiter
— `[1, 2,]` is identical to `[1, 2]` (a lone `[,]` / `(,)` / `f(,)` is still an error):

```chezzi
nums := [
    1, 2, 3,
    4, 5, 6,
]
m := {
    "a": 1,
    "b": 2,
}
t := (
    1,
    2,
)
```

A parenthesised expression is grouping; a trailing comma makes a tuple, so **`(x)` is just `x`**
while **`(x,)` is a one-element tuple**.

## 3. Variables & types

```chezzi
x := 5                 # declare + infer  (type = int)
name: str = "thuan"    # declare with explicit type
count := 0
count += 1             # reassignment (+= -= also)
```

- **Local inference:** inside function bodies you rarely write types — `:=` infers.
- **Explicit annotation** (`name: T = ...`) is allowed anywhere and **required on function signatures** (§5).

### Built-in types

| Type | Example | Notes |
|------|---------|-------|
| `int` | `42` | 64-bit signed |
| `float` | `3.14` | 64-bit |
| `bool` | `true` | |
| `str` | `"hi"` | UTF-8 |
| `list[T]` | `[1, 2]` | growable |
| `map[K, V]` | `{"a": 1}` | insertion-ordered hash map; `K` is any `Hashable` type |
| `set[T]` | `{1, 2, 3}` | deduped, insertion-ordered hash set; `T` any `Hashable` type; empty is `set()` |
| `tuple` | `(1, "a")` | fixed-arity, immutable |
| `Result[T, E]` | `Ok(x)` / `Err(e)` | §9; shorthand `T!E`, or `T!` (E = `Error`) |
| `Option[T]` | `Some(x)` / `None` | §9; shorthand `T?` |

**Type shorthand.** In any type position, `T?` is sugar for `Option[T]`; `T!E` for `Result[T, E]`;
and `T!` for `Result[T, Error]` (E defaults to the built-in `Error` protocol). Examples: `int?`,
`list[int]?`, `int!` (= `Result[int, Error]`), `int!DbErr` (= `Result[int, DbErr]`). Pure spelling —
`Some`/`None`/`Ok`/`Err`, `match`, and `?` behave exactly as on the long forms.

## 4. Operators & precedence

Highest → lowest. Same row = same precedence, left-associative unless noted.

| Level | Operators | Notes |
|-------|-----------|-------|
| 1 | `f(x)` `a.b` `a[i]` | call, field access, index |
| 2 | `?` | error propagation (postfix, §9) |
| 3 | `not` `-` (unary) | |
| 4 | `*` `/` `%` | |
| 5 | `+` `-` | |
| 6 | `..` | range (end-exclusive) |
| 7 | `<<` `>>` | bitwise shift (int-only) |
| 8 | `&` | bitwise and (int-only) |
| 9 | `^` | bitwise xor (int-only) |
| 10 | `\|` | bitwise or (int-only) |
| 11 | `<` `<=` `>` `>=` | |
| 12 | `==` `!=` | |
| 13 | `and` | |
| 14 | `or` | |
| 15 | `\|>` | pipe (§11), left-assoc |

> This table is the contract for the Pratt parser. Bitwise ops are **int-only** (a float operand is
> a type error); the relative order follows Python (comparison looser than `\|` < `^` < `&` < shifts).
> A shift amount outside `0..64` is a runtime error.

## 5. Functions  (M3)

```chezzi
fn add(a: int, b: int) -> int:     # param types REQUIRED; '-> T' optional
    return a + b

fn double(x: int):                 # no '-> T' → return type inferred from the body (here: int)
    return x * 2

fn log(msg: str):                  # body returns no value → inferred 'nil' (returns nothing)
    print(msg)

# closures / anonymous functions — body after ':'
twice := fn(x: int) -> int: x * 2
nums.map(fn(x): x * 2)             # param/return types inferred in closures
```

**Return type inference.** Omitting `-> T` infers the return type from the function's
`return` statements: the first concrete return wins, conflicting returns are a type error,
and a body with no value-returning `return` infers `nil`. Param types stay required.

**Default + named arguments.** A free function (or a struct constructor) may give trailing
parameters a **default** — any expression that does **not** reference another parameter (`= 10`,
`= 1 + 2`, `= GLOBAL * 2`, `= compute()`; a call runs once per omitting call). Defaults are evaluated
at the call site, so a param-referencing default (`y: int = x + 1`) is rejected. Callers may also
pass arguments **by name**:

```chezzi
fn greet(name: str, greeting: str = "Hello", punct: str = "!") -> str:
    return greeting + ", " + name + punct

greet("Ada")                       # "Hello, Ada!"  — both defaults
greet("Ada", "Hi")                 # "Hi, Ada!"     — override one positionally
greet("Ada", punct="?")            # "Hello, Ada?"  — name an arg, skip a middle default
greet(punct=".", name="Bo", greeting="Hey")        # all named, any order

struct Server:
    host: str
    port: int = 8080               # default field
    tls: bool = false

Server("localhost")                # port=8080, tls=false
Server("db", port=9000)            # named field; tls defaults
```

Rules: a parameter with a default may not be followed by a required one; at a call, positional
arguments must precede named ones (`f(y=2, 1)` is an error); each parameter may be supplied at most
once. Named arguments are **reordered into parameter-declaration order**, so a side-effecting named
argument evaluates in parameter order, not source-text order (`f(y=g(), x=h())` runs `h()` before
`g()`). Defaults — being constant literals — have no observable order. Scope: free functions (own
module, `from`-imported, or module-qualified `mod.f(...)`), struct constructors, **and struct
methods** (`p.greet(punct="?")`, `p.scale()` filling a default). Because a method's receiver type is
unknown to the desugar pass, methods are resolved by name: if two structs define a same-named method
with **different** parameters, a named call to it is rejected as ambiguous and — since the binding
can't be chosen safely — its **defaults aren't filled** either (the call then fails the arity check),
so give same-named methods the same parameter shape or unique names. A method that reuses a built-in
method name (`map`, `push`, `len`, …) does not get default/named support. Defaults are **not yet**
supported on closures or enum variants. (Per §above, a default may be any expression that doesn't
reference another parameter — a literal, a global, arithmetic, or a call; only param-referencing
defaults are rejected.)

**`?` inside a closure.** A closure body may use `?` (§9) — but only when the closure carries an
**explicit `-> Result[…]`/`-> Option[…]`** return type. The `?` propagates to *that closure's*
return, not the enclosing function. A closure with an inferred or non-`Result`/`Option` return type
that uses `?` is a type error.

```chezzi
fn parse(s: str) -> int!: ...
rs := ["2"].map(fn(s: str) -> int!: Ok(parse(s)? * 2))   # ? lands in the closure's own Result
```

## 6. Control flow  (M3)

```chezzi
if x > 0:
    print("pos")
else if x == 0:        # 'else if', chainable
    print("zero")
else:
    print("neg")

for i in 0..10:        # range: 0..10 is 0 through 9 (end-exclusive)
    print(i)

for item in items:     # iterate a list
    print(item)

for k in counts:       # iterate a map → its keys (insertion order)
    print(k)

for k, v in counts:    # iterate a map's entries → key + value
    print("{k}={v}")

for a, b in pairs:     # destructure a list[(A, B)] — N names over a list[tupleN]
    print("{a}:{b}")   # (one name binds the whole tuple). enumerate/zip live in std.iter.

# A user struct is iterable too: give it `next(self) -> Option[T]` and `for` drives it lazily,
# calling next() each step until it returns None (so an infinite iterator + `break` terminates).
struct Counter:
    n: int
    limit: int
    fn next(self) -> Option[int]:
        if self.n >= self.limit:
            return None
        v := self.n
        self.n = self.n + 1
        return Some(v)

for x in Counter(0, 5):    # x binds the element type (int); single loop variable only
    print(x)

# `Iterator[T]` is a real protocol bound: a generic fn can take ANY iterable — built-in
# list/set/str (intrinsically) or a user struct with `next` — and recover its element type `T`.
fn first_or[S: Iterator[T], T](xs: S, default: T) -> T:
    for x in xs:           # x is typed `T`
        return x
    return default
first_or([10, 20], 0)      # 10   (T = int, recovered from the list element)
first_or("hi", "?")        # "h"  (T = str)
# There is NO `yield`/generator keyword — lazy sequences are built as adapter structs over this
# protocol (Rust-style; see examples/iter_adapters.chz for Take/Mapped).

while cond:
    cond = step()

# `break` exits the innermost loop; `continue` skips to the next iteration.
```

## 6b. Indexing & slicing  (M15)

```chezzi
xs := [10, 20, 30, 40]
print(xs[1])           # 20    — index
xs[1] = 99             # mutate in place
sub := xs[1..3]        # slice: half-open, reuses the `..` range → [99, 30]
print("hello"[0..2])   # he    — strings slice too (→ a new str)
print(xs[1..99])       # [99, 30, 40]   — bounds are clamped (no panic)
```

Slicing reuses the existing `..` range (no `[a:b]` colon syntax, no step): `obj[start..end]` is
half-open and bounds-clamped. `list[T]` slices to `list[T]`, `str` to `str`. Indexing and slicing
are **protocols**, so custom types opt in — see `Index`/`IndexSet`/`Slice` in §7b. (Deferred:
omitted bounds `xs[..n]`/`xs[1..]`, inclusive `..=`, negative indices.)

## 6c. Comprehensions  (M16)

```chezzi
[x * 2 for x in xs]              # list: map each element
[x for x in xs if x > 0]         # list: with an `if` guard
[i for i in 0..10]               # over a range (any iterable works)
{x % 3 for x in xs}              # set: duplicates collapse
{k: v * 10 for k, v in scores}   # map: `for k, v` binds a map's entries
```

One `for` clause (binds one name, or two — `for k, v in m` — over a map's entries) and an optional
`if` guard. The loop variable is scoped to the comprehension. The iterable is anything a `for` loop
accepts (list/map/set/str/range and struct iterators); set elements and map keys must be `Hashable`.
(Deferred: nested clauses, e.g. `[x for x in xs for y in ys]`.)

## 7. Structs  (M3)

```chezzi
struct Point:
    x: int
    y: int

    fn dist(self) -> float:           # method: first param is 'self'
        return math.sqrt(float(self.x*self.x + self.y*self.y))   # needs: import std.math

p := Point(3, 4)        # construct positionally
print(p.x)              # field access
print(p.dist())         # method call
```

No inheritance (by design). Composition only.

## 7b. Generics & protocols  (M7)

**Generic functions** take type parameters in `[…]` after the name. A parameter may carry a
**bound** — a protocol the instantiating type must satisfy. Type arguments are normally **inferred**
from the call, but may be **given explicitly** at the call site: `id[int](42)`, the struct form
`Pair[int, str](1, "one")`, and the enum-variant form `Full[int](9)` (see §8). Explicit args pin the
type; inference fills any that are left off.

```chezzi
fn first[T](a: T, b: T) -> T:        # unbounded: works for any type
    return a

fn max[T: Comparable](a: T, b: T) -> T:   # bounded: T must be Comparable
    if a < b:
        return b
    return a

print(max(3, 7))                     # 7   (int is Comparable)
print(max("apple", "banana"))        # banana
```

**Protocols** are Go-style structural interfaces: a block of body-less method signatures. A type
satisfies a protocol by *having* the methods — there is no `implements` declaration. `Self` inside
a signature refers to the conforming type.

```chezzi
protocol Comparable:                 # this one is PREBUILT — shown for illustration
    fn compare(self, other: Self) -> int

struct Point:
    x: int
    y: int
    fn compare(self, other: Point) -> int:   # ⇒ Point satisfies Comparable, structurally
        return (self.x + self.y) - (other.x + other.y)

print(max(Point(1, 2), Point(3, 0)).x)   # works: Point is Comparable
```

The prebuilt **`Comparable`** protocol (`compare(self, other: Self) -> int`) is special: it is the
one protocol wired to operators. For any `Comparable` value — including a bare `T: Comparable` —
the ordering operators `< <= > >=` dispatch to `compare` (a negative/zero/positive result means
less/equal/greater). `int`, `float`, and `str` satisfy `Comparable` intrinsically.

```chezzi
print(Point(1, 1) < Point(5, 5))     # true  — `<` calls Point.compare
```

Equality (`==` / `!=`) is **not** affected — it stays structural (field-by-field) for every type.
Ordering is overloaded through `Comparable`; arithmetic `+`/`-`/`*` is overloaded through the
per-operator protocols **`Add`/`Sub`/`Mul`** (methods `add`/`sub`/`mul(self, other: Self) -> Self`).
A struct defining the matching method gets that operator on its values; `int`/`float` satisfy them
intrinsically. `/` and `%` are never overloaded.

```chezzi
struct Vec2:
    x: int
    y: int
    fn add(self, o: Vec2) -> Vec2:
        return Vec2(self.x + o.x, self.y + o.y)

print((Vec2(1, 2) + Vec2(3, 4)).x)   # 4   — `+` calls Vec2.add
```

Indexing and slicing are overloaded through the prebuilt **`Index[K, V]`** (read `obj[k]` via
`index(self, key: K) -> V`), **`IndexSet[K, V]`** (mutable `obj[k] = v`, adds `set_index(self, key: K,
val: V)`), and **`Slice[R]`** (`obj[a..b]` via `slice(self, start: int, end: int) -> R`) protocols.
Built-in `list`/`map`/`str` satisfy them intrinsically (`str` is read-only — `Index`/`Slice` but not
`IndexSet`); a struct defining the matching methods becomes indexable/sliceable. Because they are real
protocols, a generic can be bounded by them — `K`/`V`/`R` are recovered at the call site like
`Iterator[T]`'s element:

```chezzi
struct Ring:
    data: list[int]
    fn index(self, key: int) -> int:
        return self.data[key % self.data.len()]
    fn set_index(self, key: int, val: int):
        self.data[key % self.data.len()] = val
    fn slice(self, start: int, end: int) -> list[int]:
        return self.data[start..end]

r := Ring([10, 20, 30])
print(r[3])            # 10   — wraps; `index` dispatched
r[1] = 99              # `set_index` dispatched
print(r[0..2])         # [10, 99]   — `slice` dispatched

fn first[C: Index[int, V], V](c: C) -> V:   # works over a list OR a Ring
    return c[0]
```

A type parameter may carry **multiple bounds** with `+`: `fn fma[T: Add + Mul](a: T, b: T, c: T)`
requires `T` to satisfy both.

A protocol may itself take **type parameters** — `protocol Container[T]:`. A bound then supplies
concrete arguments, and a type satisfies it structurally with the parameters substituted:

```chezzi
protocol Container[T]:
    fn get(self, i: int) -> T

fn first[X: Container[int]](c: X) -> int:   # T pinned to int; c.get(0) is int
    return c.get(0)
```

The number of args must match the protocol's arity (a bare protocol takes none). A parameterized
protocol is usable **only as a bound**, not as an existential value type (`c: Container[int]` is an
error — its type args have nowhere to live in a value).

The prebuilt **`Iterator[T]`** is a parameterized bound with extra magic: `[S: Iterator[T], T]`
accepts any iterable `S` and **recovers** `T` from the iterand's element (by unifying it), rather
than requiring it written out. It is satisfied **intrinsically** by `list`/`set`/`str`/`map`
(str → str, map → its keys) and **structurally** by any struct with `next(self) -> Option[T]`. `T`
then flows into the body's loop variable and the return type. (User protocols take their args
explicitly; only `Iterator` recovers them.)

```chezzi
fn to_list[S: Iterator[T], T](xs: S) -> list[T]:
    out := []
    for x in xs:            # x : T
        out.push(x)
    return out
to_list("ab")              # ["a", "b"]   (T = str)
```

**Type aliases** name an existing type transparently — `type Name =
<type>` makes `Name` interchangeable with the aliased type everywhere (structural, not a distinct
nominal type); aliases may name scalars, collections, structs, or other aliases (cycles are
rejected).

```chezzi
type UserId = int
type Scores = map[str, int]
uid: UserId = 7        # UserId and int are the same type
```

The prebuilt **`Stringable`** protocol (`str(self) -> str`) customises how a value is rendered. A
struct that defines `str(self) -> str` overrides its default `Name(field=value, …)` repr everywhere
it is printed: by `print`, by the `str()` builtin, and inside `{…}` string interpolation — including
when nested in a list / tuple / map / set / enum payload. Structs without a `str` method keep the
default repr; enums always use the built-in `Variant(payload)` repr (enums have no methods). Like
`Comparable`, `Stringable` is prebuilt and works as a generic bound (`fn show[T: Stringable](v: T)`).

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

The prebuilt **`Hashable`** protocol (`hash(self) -> int`) governs `map` keys and `set` elements:
`int`/`str`/`bool` satisfy it intrinsically, and a struct satisfies it by defining `hash(self) ->
int`. `map`/`set` are real insertion-ordered hash tables, so **any `Hashable` type can be a key or
element** — a struct key is hashed via its `hash()` and the probe confirmed by structural `==`.
`float` is rejected (NaN footgun). Contract: two structurally-equal structs must return the same
`hash()` (the implementor owns this, like Rust's `Hash`/`Eq`).

```chezzi
struct Point:
    x: int
    y: int
    fn hash(self) -> int:
        return self.x * 31 + self.y

label: map[Point, str] = {}
label[Point(1, 2)] = "here"      # struct key — hashed via Point.hash
print(label[Point(1, 2)])        # here
```

**Generic structs** carry type parameters after the name; their fields and methods may use them.
Type arguments are inferred at construction, or written explicitly in a type annotation.

```chezzi
struct Pair[A, B]:
    first: A
    second: B
    fn left(self) -> A:
        return self.first

struct Stack[T]:
    items: list[T]
    fn push(self, x: T):
        self.items.push(x)

p := Pair(42, "hi")              # inferred Pair[int, str]
print(p.left())                  # 42  — left() returns A = int
q: Pair[str, int] = Pair("k", 9) # explicit type arguments
```

A **method may introduce its own type parameters** in `[…]`, fresh and beyond the struct's own —
inferred from the call arguments just like a free generic function (it may not reuse a struct
parameter's name):

```chezzi
struct Box[T]:
    v: T
    fn map_to[U](self, f: fn(T) -> U) -> U:   # U is fresh; inferred from the closure
        return f(self.v)

b := Box(5)
print(b.map_to(fn(x: int) -> str: "n{x}"))    # U = str  → "n5"
```

Generics are **type-erased**: the parameters exist only for the checker. At runtime a
`Pair[int, str]` is an ordinary struct value — there is no monomorphization and no per-type code.

## 8. Enums & pattern matching  (M3)

```chezzi
enum Shape:
    Circle(int)         # variants may carry data
    Square(int)
    Point               # ...or not

fn area(s: Shape) -> float:
    match s:                          # match is exhaustive — compiler checks all variants
        Circle(r): return 3.14 * float(r * r)
        Square(n): return float(n * n)
        Point:     return 0.0
```

Enums may be **generic**, carrying type parameters after the name exactly like generic structs; a
variant's payload may reference them (including the enum's own type, for recursive shapes). Type
arguments are inferred from the constructor's arguments, or written explicitly in a type annotation,
and — like all generics — are **type-erased** (a `Tree[int]` and a `Tree[str]` share one runtime
shape). Bounds (`[T: Comparable]`) and multiple type parameters (`Either[A, B]`) work too.

```chezzi
enum Tree[T]:
    Leaf
    Node(T, Tree[T], Tree[T])

fn sum(t: Tree[int]) -> int:
    match t:
        Leaf:          return 0
        Node(v, l, r): return sum(l) + v + sum(r)   # v is int — T substituted in the match
```

A **payload-carrying** variant's type args are inferred from the payload, but may be pinned
explicitly — `Node[int](1, Leaf, Leaf)` — the same `name[Type, …](…)` form as generic fns and
structs (§7b).

`match` also works on `Result`/`Option` (they're enums under the hood):

```chezzi
match safe_div(10, 2):
    Ok(v):  print("got {v}")
    Err(e): print("failed: {e}")
```

A scrutinee can also be an **int/str/bool** (literal arms + a required `_` wildcard) or a **tuple**.
Patterns **nest**: a variant payload or tuple element may itself be a binding, a literal, a wildcard,
a tuple, or another variant.

```chezzi
match point:                  # tuple scrutinee
    (0, 0):  "origin"
    (0, y):  "on the y axis"
    (x, y):  "at {x},{y}"     # an all-binding tuple arm is irrefutable (exhaustive)

match maybe_pair:             # nested: a tuple inside Some(...)
    None:         print("none")
    Some((a, b)): print(a + b)
```

(Nested **nullary** variants like `Cons(h, None)` aren't supported yet — nest a `match`.)

**Guards** (`pattern if cond:`) add a boolean test to an arm — it matches only when the pattern
binds *and* the guard (which sees the pattern's bindings) is true; otherwise the next arm is tried.
A guarded arm is never irrefutable, so it can't satisfy exhaustiveness on its own — keep a `_`.

```chezzi
match n:
    x if x < 0: "neg"
    0:          "zero"
    _:          "pos"         # required: guarded arms don't close the match
```

**Range patterns** (`start..end:`) match an **int** in the half-open range `start <= v < end`
(int-only, refutable — still need a `_`):

```chezzi
match score:
    0..60:  "F"
    60..90: "B"
    _:      "A"
```

### `match` and `if` as expressions

Both branch forms can also be used as **expressions** that produce a value — handy for
initializing a variable without a pre-declared mutable:

```chezzi
# match-expression: multiline, exhaustive, each arm body is a single value-expression
label := match shape:
    Circle(r): "round"
    Square(n): "boxy"
    Point:     "dot"

# if-expression: inline, ternary-style — `else` is REQUIRED
sign := if n > 0: "pos" else: "neg"
```

All arms (and both `if` branches) must agree on a type. The statement forms — `match s:` /
`if c:` with indented blocks and `return`/assignments inside — are unchanged; only loops and
function bodies stay statement-only (functions still return via explicit `return`).

## 9. Errors — Result / Option + `?`  (M3)

Errors are **values**, not exceptions. No hidden control flow.

```chezzi
fn safe_div(a: int, b: int) -> int!:        # int! == Result[int, Error]
    if b == 0:
        return Err("divide by zero")        # a str IS an Error (see below)
    return Ok(a / b)

fn calc() -> Result[int]:
    x := safe_div(10, 2)?     # '?' unwraps Ok, or returns the Err from THIS function
    y := safe_div(x, 0)?      # if Err, calc() returns that Err immediately
    return Ok(x + y)
```

**The `Error` type (Go-style).** `E` defaults to the built-in `Error` protocol — one method,
`message(self) -> str`. `str` conforms to it intrinsically (its message is itself), so `Err("…")`
works everywhere with no wrapper. For a *structured* error, define a struct with `message` and
name it explicitly with `T!E`:

```chezzi
protocol Error:                 # built-in; shown for reference
    fn message(self) -> str

struct DbErr:
    code: int
    fn message(self) -> str:
        return "db error {self.code}"

fn query() -> Row!DbErr:        # Result[Row, DbErr]
    return Err(DbErr(503))

match query():
    Ok(row): use(row)
    Err(e):  print(e.message())   # on a default `Error`, only message() is available
```

`Option[T]` (shorthand `T?`) is the same shape for "maybe absent": `Some(v)` / `None`, also usable with `?`.

**Optional chaining `?.` and null-coalescing `??`** (on `Option`) cut the `Some`/`None` boilerplate:

```chezzi
name := user?.profile?.name ?? "anon"   # None anywhere short-circuits to None, then ?? defaults
len  := s?.trim()?.len() ?? 0           # ?. also chains method calls
```

`x?.field` / `x?.method(args)` on an `Option[T]`: `None` short-circuits to `None`, `Some(v)` applies
the access to `v` and re-wraps — so the result is always an `Option` (a field that is itself `Option`
is **not** flattened: `Option[Option[U]]`). `a ?? b` returns `a`'s inner value if `Some`, else `b`;
it is **right-associative** (`a ?? b ?? c` = `a ?? (b ?? c)`). Both require the chars **adjacent**
(`x?.f`, not `x? .f` — the spaced form is the try `?` then `.field`). Sugar only: both desugar to a
`match` on the `Option`.

**Unhandled errors at the top level exit the program.** An `Err`/`None` that reaches the top level —
a bare top-level expression statement that evaluates to one (e.g. `compute()` whose result is `Err`),
or a top-level `?` that hits one — terminates the program with `unhandled error: <detail>` and a
non-zero exit code. *Binding* the value handles it (`r := compute()` keeps running; inspect `r`).

### `recover:` — the panic-recovery boundary

`Result`/`?` handle *expected* errors. A **runtime fault** — index-out-of-bounds, divide-by-zero,
integer overflow, a missing map key — is a *panic*: by default it terminates the program. A
`recover:` block is a boundary that **catches any panic occurring transitively beneath it** (no need
to pre-mark risky code) and yields a `Result[T, Error]`:

```chezzi
r := recover:
    rows := parse(file)       # may panic deep inside
    rows[0] / rows[1]         # OOB / divide-by-zero is caught here
match r:
    Ok(v):  print(v)          # Ok wraps the block's trailing-expression value
    Err(e): print("recovered: {e.message()}")   # a fault becomes Err(message)
```

It behaves like a **try-block**: a `?` inside the block short-circuits to the boundary (the `Err`
lands in `r`), so one `recover:` handles *both* panics and propagated `Result` errors. Because `?`
targets the boundary rather than the function, it is allowed even when the enclosing function does
not return a `Result`.

```chezzi
fn run() -> str:                       # not a Result-returning function
    r := recover:
        n := parse_int(input)?         # an Err here lands in `r`, not propagated out of run()
        n * 2
    match r:
        Ok(v):  return "got {v}"
        Err(e): return "failed: {e.message()}"
```

Rules: `recover:` is a value (not a control-flow target) — `return`/`break`/`continue` that would
escape it are rejected; a `?` on an `Option` inside it is rejected (its result is `Result`-typed —
use `match`). Reach for `recover:` at boundaries (a request, a REPL line, a plugin, a test), not as
everyday error handling — `Result`/`?` remain the tool for expected failures.

### `defer` — block-scoped cleanup  (M16)

`defer <call>` schedules a call to run when the **enclosing lexical block** exits — on **every**
path: fall-through, `break`/`continue`, normal return, a `?` short-circuit, or a panic. Deferred
calls run **LIFO** (last registered, first run); an unwind crossing several blocks runs each block's
defers inner-block-first. The receiver and arguments are evaluated **at the `defer` statement** (Go
semantics); only the call itself is delayed.

Every indented block is a defer scope: the function body, a loop body, an `if`/`elif`/`else` branch,
a `recover:` block, a statement-form `match` arm, and the module top level.

```chezzi
fn process(path: str) -> int!:
    f := open(path)
    defer f.close()           # runs however `process` exits
    n := f.read_int()?        # if this short-circuits, f.close() still runs
    return Ok(n * 2)

for path in paths:
    f := open(path)
    defer f.close()           # runs at the END of each iteration — no leak across the loop
    use(f)
```

`defer` targets a **method call** or a call to a **first-class callable value** (a function or
closure, or a name bound to one). Built-ins (`print`, `len`, …) and constructors aren't first-class
values — wrap them: `fn log(m: str): print(m)` then `defer log("done")`. `defer` composes with
`recover:` — a defer inside a `recover:` block runs as that block unwinds, before the boundary binds
its value. Top-level defers run LIFO when the program ends (or while unwinding an unhandled
top-level error). `std.os.exit` is a hard halt and does **not** run deferred calls (matching Go's
`os.Exit`).

**Block form `defer:`** — to group several cleanup actions, give `defer` an indented block instead
of a single call (mirrors `spawn`'s dual form). The body runs **top-to-bottom** at scope exit, but
is **LIFO as a unit** relative to other `defer`s. Unlike the call form (which has no call-only
restriction inside the block) the body is ordinary statements — built-ins are fine. Free variables
are **snapshotted by value at the `defer` point** (consistent with the call form's eager argument
evaluation), and the block runs in the **same task** — so reads of enclosing locals (even
non-sendable ones, unlike a `spawn:` block) are allowed. Two rules follow from the by-value snapshot:
**reassigning** an enclosing local inside the block is an error (it can't be written back through the
snapshot — declare a fresh binding with `:=`, or use a `Shared[T]`); and a `?` short-circuit inside
the block is **discarded** (a cleanup body has no error-return contract, like a deferred call whose
`Err` result is dropped).

```chezzi
fn handle(conn: Conn):
    x := 1
    defer:                        # both lines run at scope exit, top-to-bottom
        log("closing")
        conn.close()
    defer:
        log("x = {x}")            # prints "x = 1" — snapshotted here, not at exit
    x = 2
```

## 9b. Program entry — there is no automatic `main`

Chezzi is a scripting language: a program runs **top-to-bottom**. There is **no automatic entry
point** — `main` is an ordinary function. Define it and call it yourself if you want one:

```chezzi
fn main():
    print("hello")

main()        # nothing runs main for you
```

(A future `chezzi.toml` may declare a project `entrypoint` for tooling-driven builds; the language
core does not special-case `main`.)

## 10. Strings & interpolation

```chezzi
name := "thuan"
age := 30
print("hi {name}, age {age}")     # {expr} interpolates
print("sum: {a + b}")             # any expression
print("brace: {{not interpolated}}")   # '{{' / '}}' = literal braces
```

**Quote styles.** A string may be delimited by `"…"` or `'…'`; the two are fully interchangeable —
same `str` type, same escapes, same interpolation. Inside a double-quoted string `'` is a literal
char (and `\"` escapes the quote); inside a single-quoted string `"` is a literal char (and `\'`
escapes the quote). `\'` and `\"` are both accepted in either style.

**Escapes.** Backslash escapes resolve at lex time: `\n` `\t` `\r` `\\` `\"` `\'` `\0` and
`\u{HEX}` (1-6 hex digits naming a Unicode scalar value, e.g. `\u{41}` → `A`, `\u{1F600}` → 😀).
A surrogate (`D800`-`DFFF`), a value above `10FFFF`, an empty `\u{}`, a missing brace, a non-hex
digit, or any other unknown escape is an error. Two independent layers, like Python f-strings: `\`
escapes a *character* (`\"` → a quote), while `{{` / `}}` escape *interpolation* (→ a literal brace).

```chezzi
print("tab\tgap, quote \"x\", path C:\\tmp")
print('single quotes work too: {name}, with \'apostrophe\' and \u{2728}')
print("literal {{x}} vs value {x}")
```

Core-type string methods (built in — no import needed):

```chezzi
s.len()          s.upper()        s.lower()
s.trim()         s.split(",")     s.starts_with("ab")
s.contains("b")  ",".join(parts)  # join: separator.join(list[str])
s.chars()        # → list[str] of 1-char strings; also `for c in s:` iterates them
"a" + "b"        # concatenation
```

A character is just a 1-char `str` (Python-style — there is no `char` type): index with `s[i]`,
iterate with `for c in s:` or `s.chars()`, and bridge to codepoints with `ord`/`chr`.

List methods (built in): `xs.push(x)` `xs.pop()` `xs.len()` `xs.reverse()` `xs.contains(v)`
`xs.index_of(v)` `xs.sum()` `xs.sort()` (ascending, in place); `xs.concat(ys)→list` (new list) and
`xs.extend(ys)` (append in place, → nil); higher-order `xs.map(f)` `xs.filter(p)` `xs.fold(init, f)`;
`xs.sort_by(fn(a, b) -> int)` — a custom comparator (negative = `a` before `b`), stable, in place;
and `xs.sort_by_key(fn(x) -> K)` — sort by a derived key (`K` Comparable: int/float/str or a struct
with `compare`), stable, in place.

Map methods: `m.get(k)→V?` `m.has(k)` `m.keys()` `m.values()` `m.remove(k)` `m.len()`;
`m.merge(n)→map` (new map, `n` wins on a key clash) and `m.update(n)` (write `n` into `m` in place,
→ nil); `m[k]` reads (errors on a missing key), `m[k] = v` inserts/updates. Iterate with `for k in m`
/ `for k, v in m`.

Sets: `{a, b, c}` is a set literal (deduped, insertion-ordered; `{}` is the empty *map*, the empty
set is `set()`; `set(list)` builds one from a list). Elements are any `Hashable` type (int/str/bool,
or a struct with `hash(self) -> int`).
Methods: `s.add(x)` `s.remove(x)→bool` `s.has(x)` `s.len()` `s.union(t)` `s.intersection(t)`
`s.difference(t)`; iterate with `for x in s`. `==` is order-independent.

## 11. Pipe operator `|>`

Threads the left value as the first argument of the right call. Reads top-to-bottom for data flow.
The right side must be a call; `a |> f(x)` desugars to `f(a, x)`.

```chezzi
result := [1, 2, 3, 4]
    |> filter(fn(x): x % 2 == 0)   # → filter([1,2,3,4], ...)
    |> map(fn(x): x * 10)
    |> sum()
# result == 60

# equivalent without pipe:
# sum(map(filter([1,2,3,4], fn(x): x % 2 == 0), fn(x): x * 10))
```

## 11b. Concurrency — `spawn` / `parallel:`  (see [`concurrency.md`](concurrency.md))

> **Implemented — shipped through Tier-D.** The cooperative engine is the default; `--parallel` is a
> real OS-thread M:N scheduler (`Channel`/`Shared`/`Executor`, netpoller-backed `std.net`). Full
> design in [`concurrency.md`](concurrency.md); phase history in
> [`concurrency-tier-d.md`](concurrency-tier-d.md).

```chezzi
fn worker(id: int, prefix: str, out: Channel[str]):
    out.send("{prefix}-{id}")

fn main():
    ch := Channel[str]()
    label := "task"
    parallel:                          # nursery — joins all children at the dedent
        spawn worker(1, label, ch)     # form 1: a named call (Go's `go f(x)`); args COPIED in
        spawn worker(2, label, ch)
        spawn:                         # form 2: an anonymous indented block
            out := heavy()
            ch.send(out)
    # reaching here ⇒ all children finished. No WaitGroup, no leaks.
    for _ in 0..3:
        print(ch.recv())               # results moved into main's heap

main()
```

- **`parallel:` is a nursery** — all tasks spawned inside join at the dedent, then the parent
  proceeds. `spawn` returns immediately (the parent continues); tasks run at the barrier.
- **Every function body (and the module top level) is an implicit nursery** (M-C) — a bare `spawn`
  is legal anywhere and joins at the body's `return`/end (the module top level joins at program exit).
  `return`, fall-through, and `?` are all join points (tasks run, *then* control leaves); `defer`s run
  after the join. An explicit `parallel:` is an *inner* sub-nursery for an earlier join. A `spawn`
  always binds to a nursery **in its own function** — a task can't outlive the function that spawned
  it (the function-boundary rule).

```chezzi
fn fetch_all(urls: list[str]):
    for u in urls:
        spawn fetch(u)        # no `parallel:` needed — joins when fetch_all returns
    print("dispatched")       # runs before the tasks; they join at end-of-function
```
- **`Channel[T]`** — a mailbox (buffered FIFO): `ch.send(v)`, `ch.recv() -> T`,
  `ch.try_recv() -> T?` (non-blocking poll — `Some(v)`/`None`, never blocks or faults), `ch.len()`,
  `ch.close()`, `ch.try_send(v) -> bool` (safe `send` — `false` if closed, never faults). After
  `close()`: `send` faults, `recv` drains then faults, `try_send` returns `false`. Drain a channel to
  completion with **`for v in ch:`** — it blocks per value and ends cleanly once closed-and-drained
  (Go's `for v := range ch`). Values **move/copy** across the boundary; the sender can't reuse a sent
  value.
- **`Shared[T]`** — the cross-task mutable box: `s.get()`, `s.set(v)`, `s.update(fn(x): ...)`. The
  ladder is `value` (copied) → `Ref[T]` (in-task) → `Shared[T]` (cross-task). `Ref` is **not**
  sendable; `Shared` is.
- **`Atomic[T]`** — the cross-task **atomic** box (sibling of `Shared`, sendable handle, value-first
  `Atomic(v)`): `a.load()`, `a.store(v)`, `a.exchange(v) -> T` (returns old), `a.cas(expected, new) ->
  bool`, and on numeric `T` `a.add(x) -> T` / `a.sub(x) -> T` (return the new value; checked-overflow
  like `+`/`-`). Each op is atomic across threads. Use it for counters/flags/CAS-loops; `Shared` for
  arbitrary-transform updates.
- **`timer(ms) -> Channel[bool]`** — a one-shot timeout channel: `timer(500).recv()` blocks ~500ms then
  yields `true` (level-triggered — ready on any recv at/after the deadline). The composable timeout
  primitive; once `wait` lands it races against real channels (`recv_timeout` is just `wait` over a
  channel and a `timer`).
- **`wait:` (select)** — *designed, not yet implemented.* Blocks until the first of several channel
  `recv`s is ready: `wait:` then arms `v := ch.recv():` (or `=`/`_`), an optional non-blocking `else:`,
  and timer arms for timeouts. Recv-only (sends never block on unbounded channels). See
  [`concurrency.md §6d`](concurrency.md) for the locked design.
- **Sendability:** captures are copies, **read-only** inside a task (reassign = error); only sendable
  types (scalars/str/containers+structs of sendable/`Channel`/`Shared`) cross — not closures, native
  handles, or `Ref`.

## 12. Imports & modules  (M4.5)

```chezzi
import std.io                    # use as  io.read(...)
import std.io as fs              # module alias → fs.read(...)
import read, write from std.io   # pull names in (no braces)
import read as r from std.io     # named + alias
import core.db.pool              # local module → <root>/core/db/pool.chz
```

**Resolution:** walk up from the file for `chezzi.toml`; found → that's the project root, else the
script's own dir is root. `std.*` is reserved (stdlib). `a.b.c` → `<root>/a/b/c.chz`. No `./` relative imports.

## 13. Standard library (v1)

Always available (no import): `print`, `len`, `range`, `int()`, `str()`, `float()`,
`ord(s)→int` (first codepoint), `chr(n)→str` (codepoint → 1-char string), `set()`/`set(list)`,
plus methods on core types.

`std.math.abs` is int+float polymorphic (int → int, float → float). `min`/`max`/`clamp` live in
**`std.cmp`** as generic `[T: Comparable]` functions — they work on int, float, str, **and any
struct that implements `compare`** (the old numeric-only `std.math.min`/`max` were replaced by these
in M7). `list.sort()` is likewise Comparable: it sorts lists of int/float/str or of any struct with
a `compare` method.

**`std.json`** (M8): `json.parse(s) -> Result[Json]` and `json.stringify(j) -> str` over a dynamic
`Json` enum (`Null`/`Bool`/`Num`/`Str`/`Arr`/`Obj`), with accessors `as_int`/`as_float`/`as_str`/
`as_bool`/`get`/`at`/`is_null`/`as_object`/`as_array`. For known shapes, `json.decode[T](s) ->
Result[T]` deserializes straight into a struct / `map[str, V]` / `list[T]` / scalar (Option fields
accept null-or-absent; extra keys ignored; recursive/generic struct targets are rejected). Note: a
JSON *literal in Chezzi source* must double its braces (`{{ }}`) — bare `{…}` is interpolation.

**`std.os`**: `args() -> list[str]`, `env(key) -> str?`, `getcwd() -> Result[str]`, and
`exit(code)` — a **hard, uncatchable** exit: the program halts immediately with `code` as its process
exit status (clamped `0..=255`), unwinding past any `recover:`. Output written before the call is
preserved; statements after it never run.
**`std.process`** (M8): `cmd(s) -> Result[str]` runs `s` via the shell — `Ok(stdout)` on success,
`Err(stderr)` otherwise. **`std.fs`**: `list_dir`, `exists`, `is_file`, `is_dir`, `size`, `glob`
(`*`/`?` in the last path component). **`std.time`**: `now()` (epoch secs), `monotonic()` (secs,
steady), `sleep_ms(n)`, `format(epoch)` (UTC `"YYYY-MM-DD HH:MM:SS"`).

**`std.regex`** (M9, backed by the `regex` crate — stateless, with an internal compile cache):
`is_match(pat, s) -> Result[bool]`, `find(pat, s) -> Result[Option[Match]]`,
`find_all(pat, s) -> Result[list[Match]]`, `replace_all(pat, s, repl) -> Result[str]`,
`split(pat, s) -> Result[list[str]]`. A `Match` is `{text: str, start: int, end: int,
groups: list[str]}` (`start`/`end` are **byte** offsets; `groups` is capture groups 1..n, a
non-participating optional group is `""`). A bad pattern → `Err`. Patterns are ordinary strings, so a
literal backslash is written `\\` (e.g. `"\\d+"`, `"\\."`).

**`std.request`** (M9, blocking HTTP/HTTPS via `ureq` + rustls): `get(url) -> Result[Response]`,
`post(url, body) -> Result[Response]`. A `Response` is `{status: int, body: str,
headers: map[str, str]}` (header names lowercased). A ≥400 status is a normal `Response` (its
`status` carries the code); only transport/DNS/TLS failures are `Err`. Synchronous — blocks the
single thread until the response arrives. `Match` and `Response` are reserved (program-global) type
names.

Importable: `std.io`, `std.math`, `std.str`, `std.cmp`, `std.os`, `std.json`, `std.process`,
`std.fs`, `std.time`, `std.regex`, `std.request`, `std.net`, `std.iter`, `std.ref`.

---

## Full example

```chezzi
import sqrt from std.math

struct Point:
    x: int
    y: int

    fn dist(self) -> float:
        return sqrt(float(self.x*self.x + self.y*self.y))

fn safe_div(a: int, b: int) -> Result[int]:
    if b == 0:
        return Err("divide by zero")
    return Ok(a / b)

fn main():
    p := Point(3, 4)
    print("dist: {p.dist()}")

    total := 0
    for i in 0..10:
        if i % 2 == 0:
            total += i
    print("even sum: {total}")

    match safe_div(10, 2):
        Ok(v):  print("div: {v}")
        Err(e): print("err: {e}")

main()   # no automatic entry point — call it yourself
```
