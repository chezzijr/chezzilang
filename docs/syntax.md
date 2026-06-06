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
3.14          # float (f64)
1_234.567_8   # float — '_' works in both parts
true  false   # bool
"hello"       # str
"hi {name}"   # str with interpolation — see §10
[1, 2, 3]     # list[int]
{"a": 1}      # map[str, int]
```

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
| `map[K, V]` | `{"a": 1}` | hash map |
| `Result[T]` | `Ok(x)` / `Err(msg)` | §9; shorthand `T!` |
| `Option[T]` | `Some(x)` / `None` | §9; shorthand `T?` |

**Type shorthand.** In any type position, `T?` is sugar for `Option[T]` and `T!` for `Result[T]`
(e.g. `int?`, `list[int]?`, `int!`). Pure spelling — `Some`/`None`/`Ok`/`Err`, `match`, and `?`
behave exactly as on the long forms.

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

while cond:
    cond = step()

# `break` exits the innermost loop; `continue` skips to the next iteration.
```

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
**bound** — a protocol the instantiating type must satisfy. Type arguments are always inferred
from the call (no `max[int](…)` form).

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
Only ordering is overloaded, and only through `Comparable`; there is no operator overloading for
`+ - * /`.

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

(Nested **nullary** variants like `Cons(h, None)` aren't supported yet — nest a `match`. Match
guards and range patterns are future additions.)

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
fn safe_div(a: int, b: int) -> int!:        # int! == Result[int]
    if b == 0:
        return Err("divide by zero")
    return Ok(a / b)

fn calc() -> Result[int]:
    x := safe_div(10, 2)?     # '?' unwraps Ok, or returns the Err from THIS function
    y := safe_div(x, 0)?      # if Err, calc() returns that Err immediately
    return Ok(x + y)
```

`Option[T]` (shorthand `T?`) is the same shape for "maybe absent": `Some(v)` / `None`, also usable with `?`.

**Unhandled errors at the top level exit the program.** An `Err`/`None` that reaches the top level —
a bare top-level expression statement that evaluates to one (e.g. `compute()` whose result is `Err`),
or a top-level `?` that hits one — terminates the program with `unhandled error: <detail>` and a
non-zero exit code. *Binding* the value handles it (`r := compute()` keeps running; inspect `r`).

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

**Escapes.** Backslash escapes resolve at lex time: `\n` `\t` `\r` `\\` `\"` `\0`. An unknown
escape is an error. Two independent layers, like Python f-strings: `\` escapes a *character*
(`\"` → a quote), while `{{` / `}}` escape *interpolation* (→ a literal brace).

```chezzi
print("tab\tgap, quote \"x\", path C:\\tmp")
print("literal {{x}} vs value {x}")
```

Core-type string methods (built in — no import needed):

```chezzi
s.len()          s.upper()        s.lower()
s.trim()         s.split(",")     s.starts_with("ab")
s.contains("b")  ",".join(parts)  # join: separator.join(list[str])
"a" + "b"        # concatenation
```

List methods (built in): `xs.push(x)` `xs.pop()` `xs.len()` `xs.reverse()` `xs.contains(v)`
`xs.index_of(v)` `xs.sum()` `xs.sort()` (ascending, in place); higher-order `xs.map(f)`
`xs.filter(p)` `xs.fold(init, f)`; and `xs.sort_by(fn(a, b) -> int)` — a custom comparator
(negative = `a` before `b`), stable, in place.

Map methods: `m.get(k)→V?` `m.has(k)` `m.keys()` `m.values()` `m.remove(k)` `m.len()`; `m[k]`
reads (errors on a missing key), `m[k] = v` inserts/updates. Iterate with `for k in m` / `for k, v in m`.

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
`ord(s)→int` (first codepoint), `chr(n)→str` (codepoint → 1-char string), plus methods on core types.

`std.math.abs` is int+float polymorphic (int → int, float → float). `min`/`max`/`clamp` live in
**`std.cmp`** as generic `[T: Comparable]` functions — they work on int, float, str, **and any
struct that implements `compare`** (the old numeric-only `std.math.min`/`max` were replaced by these
in M7). `list.sort()` is likewise Comparable: it sorts lists of int/float/str or of any struct with
a `compare` method.

Importable: `std.io`, `std.math`, `std.str`, `std.cmp`, `std.os`. (Later: `std.list`, `std.map`, `std.json`, `std.time`.)

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
