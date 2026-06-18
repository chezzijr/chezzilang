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
"""say "hi""""  # str — triple-quoted; unescaped quotes allowed inside (same escapes & interpolation)
'''it's "ok"'''  # str — triple single-quote, equivalent
"hi {name}"   # str with interpolation — see §10
"emoji \u{1F600}, A=\u{41}"   # str — \u{HEX} unicode escape (1-6 hex digits)
b"\x01\x02AB"  # bytes — byte-string literal; \xHH hex byte + \n \t \r \\ \" \' \0; no \u, no interp
b'\xff'       # bytes — single quotes / uppercase B'…' both work; raw byte >=0x80 must use \xHH
bytearray([1, 2, 3])  # bytearray — MUTABLE byte buffer; constructor-only (no literal), see below
[1, 2, 3]     # list[int]
{"a": 1}      # map[str, int]
```

**`bytes` — immutable byte sequence (Python `bytes` model).** A `b"..."` / `b'...'` literal holds raw
bytes (the lexer applies escapes + strips the `b` prefix). Accepted escapes: `\xHH` (exactly two hex
digits → one byte `0x00`–`0xFF`, the only way to write a byte ≥ 0x80) plus `\n \t \r \\ \" \' \0`.
`\u{…}` is **rejected** (a byte literal is byte-exact, not UTF-8), as is a raw non-ASCII source char.
No interpolation. Operations: `b[i]` → `int` 0–255 (Index protocol; out-of-range is a recoverable
panic), `b[a:b:c]` → `bytes` (Slice protocol over byte offsets — open bounds / step / reverse /
negative), `for x in b` yields `int`, `len(b)` is the byte count, `==`/`!=` are structural, and `bytes`
is `Hashable` (valid `map`/`set` key). `str(b)` / `print(b)` / interpolation use the Python `b'...'`
repr (printable ASCII literal, others `\xHH`). `bytes` is immutable — `b[i] = x` is a type error.

**`bytearray` — the MUTABLE sibling (Python `bytearray` model).** Constructor-only — there is **no**
`ba"..."` literal (the `b"..."` literal already makes a `bytes`). Four forms: `bytearray()` (empty),
`bytearray(N)` (N zero bytes, Python semantics), `bytearray(b)` (a mutable copy of a `bytes`),
`bytearray([ints])` (each element 0–255). Operations: `ba[i]` → `int` 0–255, **`ba[i] = x`** mutates
in place (`IndexSet`; the value must be 0–255 and the index in range, else a recoverable panic — the
new capability `bytes` lacks), `ba[a:b:c]` → a NEW `bytearray` (mutable copy, byte offsets),
`for x in ba` yields `int`, `len(ba)`, `.push(int)` (append one byte 0–255), `.pop() -> Option[int]`,
`.extend(bytes | bytearray | list[int])` (append in place). `==`/`!=` are structural; cross-type
`b"a" == bytearray([97])` is **content-equal** (Python parity). `bytearray` is **NOT `Hashable`**
(mutable ⇒ not a `map`/`set` key, like `list`/`set`/`map`). `str(ba)` / `print(ba)` / interpolation
use the Python `bytearray(b'...')` repr (the wrapper distinguishes it from `bytes`' bare `b'...'`).
**Conversion bridge:** `bytes(ba)` → an immutable snapshot, `bytearray(b)` → a mutable copy. Crosses
the `--parallel` airlock by value (deep copy — a fresh independent buffer, like `list`). Not yet: a
`byte`/`u8` scalar, or non-UTF-8 codecs (latin1/utf16) / base64/hex.

**Built-in conversions.** Two conversion surfaces bridge the core types — see the table below.

| Conversion | Form | Result | Notes |
| --- | --- | --- | --- |
| str → bytes (UTF-8) | `s.encode()` | `bytes` | method on `str`; always succeeds (str is UTF-8 internally) |
| bytes → str (UTF-8) | `b.decode()` | `str` | method on `bytes`; **recoverable** fault on invalid UTF-8 |
| bytearray → str (UTF-8) | `ba.decode()` | `str` | identical to `bytes.decode()` (decodes the current buffer) |
| any iterable → list | `list(it)` | `list[T]` | `it` is any **for-iterable**; `T` is the element type |
| any iterable → set | `set(it)` | `set[T]` | dedup; `T` must be `Hashable`; `set()` (0 args) is the empty set |
| iterable of 2-tuples → map | `map(it)` | `map[K, V]` | `it` yields `(K, V)` pairs; last-wins on dup keys; `K` `Hashable` |

`.encode()`/`.decode()` are **UTF-8 only** — there is no encoding-name argument (latin1/utf16 are an
explicit future non-goal). `"héllo".encode().decode() == "héllo"` round-trips through a multi-byte
char; `b"\xff\xfe".decode()` faults **recoverably** (catchable by `recover:`), never a panic.

`list(it)` / `set(it)` / `map(it)` accept **any for-iterable** — exactly what `for x in it` accepts:
`list`, `set`, `str` (per-char `str`), `bytes`/`bytearray` (per-byte `int`), `map` (its keys),
`range`, and a user struct with `next(self) -> Option[T]`. They do **not** require a formal
`Iterable[T]` bound — they reuse the same internal iterable union as the `for` loop. The argument is
**required** (no zero-arg form): an empty `list`/`map` is the `[]`/`{}` literal, so `list()` / `map()`
are checker errors directing you there (`set()` keeps its empty-set 0-arg form). `map(it)`'s element
must be **exactly a 2-tuple** `(K, V)` — a non-2-tuple is a **static** type error (caught by the
checker, not at runtime).

> **`map(it)` vs `xs.map(f)` — these do NOT clash.** `map(pairs)` is the free-function **constructor**
> (a bare-name call). `xs.map(f)` is the `list` higher-order **method** (a field/method call on a
> receiver). They live in separate namespaces — the parser routes a bare `map(...)` as a builtin call
> and a `obj.map(...)` as a method dispatch — so `map([(1, "a")])` builds a `map[int, str]` while
> `[1, 2].map(double)` transforms a list.

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
count += 1             # compound assignment — see below
a, b = b, a            # tuple swap — multi-target assignment, see below
```

- **Local inference:** inside function bodies you rarely write types — `:=` infers.
- **Explicit annotation** (`name: T = ...`) is allowed anywhere and **required on function signatures** (§5).

**Compound assignment.** `x OP= v` is exactly `x = x OP v`, on variables, list elements, struct
fields, and map values. The full set is `+= -= *= /= %=` (numeric; `+=` also concatenates `str`)
and `&= |= ^= <<= >>=` (int-only, mirroring the bitwise operators). No implicit widening — `int /=
float` is a type error (the result would be a float, which can't flow back into an `int` slot).
(`//=` and `**=` are not provided — there is no `//`/`**` base operator yet.)

```chezzi
x *= 3        # x = x * 3
xs[0] <<= 1   # xs[0] = xs[0] << 1
p.n |= 4      # p.n = p.n | 4
```

**Multi-target (tuple) assignment.** `a, b = b, a` assigns several targets at once. The **whole**
right-hand side is evaluated **first** (Python semantics), then stored into each target left-to-
right — so a swap is correct even when the same place appears on both sides. Targets may be
variables, list elements, or struct fields; the RHS is a same-arity value list or a single tuple-
valued expression (`a, b = f()` where `f` returns a 2-tuple). Only plain `=` is allowed (a compound
op with multiple targets is a parse error).

```chezzi
a, b = b, a                              # swap two variables
data[0], data[1] = data[1], data[0]      # swap two list elements
p, q, r = r, p, q                        # three-way rotation (RHS evaluated first)
a, b = compute()                         # compute() returns (int, int)
```

### `ref T` — transparent by-reference bindings

`ref T` is a **binding modifier** (on **locals and params only**) that makes a binding carry
*reference* semantics while still being spelled and used as a plain `T`. It is pure sugar over the
`std.ref` `Ref[T]` box (`import std.ref` to use it). Roughly C++'s `int&`, where the explicit
`Ref[T]` (`r.get()/.set()/.update()`) is closer to Rust's `Rc`.

```chezzi
import std.ref

r: ref int = 0     # a fresh box holding 0
r = 5              # WRITE mutates the pointee (never rebinds the box)
r += 1             # compound works too → 6
print(r)           # 6   — a READ auto-derefs (no `.get()`, no `^` operator)
print(r + 100)     # 106 — usable anywhere its value is
```

- **Read / write lowering (automatic).** A read of `r` lowers to `r.get()`; `r = v` to `r.set(v)`;
  `r += 1` to `r.set(r.get() + 1)`. There is **no deref operator** and **no `ref` marker at the call
  site** — it is all inferred from the binding/param.
- **Create vs alias (driven by the RHS).** `r: ref int = 0` creates a **fresh** box. `r2: ref int = r`
  (RHS already a `ref`) **aliases** the same box — a write through either is visible through both.
  A plain `y := r` (no annotation) **auto-derefs to a copy** (`y: int`), which does *not* share.
- **Pass by reference.** A `ref T` argument into a `ref T` param **aliases** the caller's box, so the
  callee's writes persist:

  ```chezzi
  fn bump(x: ref int):
      x += 1
  bump(r)            # mutates the caller's binding
  ```

  A `ref T` argument into a plain `T` param **auto-derefs to a copy** (a ref is usable as its value).
  The reverse — a by-value local or a literal into a `ref T` param — is a **type error** (you can't
  take a reference to a by-value local or a temporary; declare the local `ref` to pass by reference).
  The alias-vs-deref-vs-error decision is **type-directed**: it follows the *resolved* callee's
  parameter, so it works uniformly through a local fn-value (`g := bump; g(r)`), a **closure** `ref`
  param (`fn(x: ref int)` — a `ref` arg aliases, a by-value arg is the same error as a named fn), and
  a method name shared by structs that disagree on ref-ness (the receiver's type picks the method).
- **Capture.** An inner fn / closure that closes over a `ref` local shares the box, so mutations
  through it persist (a plain non-`ref` local is still captured by value).
- **One transparency gap — string interpolation.** Inside a `"{ ... }"` interpolation, a bare `ref`
  binding is **not** auto-dereferenced (interpolation fragments are parsed out-of-band, after the
  desugar pass), so `"{r}"` prints the underlying box (`Ref(value=…)`), exactly as an explicit
  `Ref[T]` would. Write `"{r + 0}"` or bind a copy (`v := r`) first if you need the value in a string.
  Everywhere else (`print(r)`, arithmetic, args, indexing) `r` reads as its value.
- **Where it's allowed.** Locals + params **only**. `ref` is a **parse error** as a return type, a
  generic argument, a collection element, a tuple element, a struct field, or on a destructuring let
  — use a first-class `Ref[T]` there.
- **Concurrency (important).** `ref`/`Ref` are **same-task** aliasing only. A `ref T` is a `Ref[T]`
  box, which is **non-sendable**: capturing or passing the box across the `spawn` / `parallel:` /
  `Channel` airlock is **rejected** by the checker. To move a value across, deref the ref into a plain
  copy first; for genuine cross-task shared mutation use `Shared[T]`, never `ref`.

### Built-in types

| Type | Example | Notes |
|------|---------|-------|
| `int` | `42` | 64-bit signed |
| `float` | `3.14` | 64-bit |
| `bool` | `true` | |
| `str` | `"hi"` | UTF-8 |
| `bytes` | `b"\x01AB"` | immutable byte sequence; `b[i]`→int, `b[a:b:c]`→bytes, iterates int; `Hashable` |
| `bytearray` | `bytearray([1,2])` | MUTABLE byte buffer (constructor-only); `ba[i]`→int, `ba[i]=x`, slice→bytearray, `push`/`pop`/`extend`; NOT `Hashable` |
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
| 12 | `==` `!=` `in` | `in` = membership, yields `bool` (see below) |
| 13 | `and` | |
| 14 | `or` | |
| 15 | `\|>` | pipe (§11), left-assoc |

> This table is the contract for the Pratt parser. Bitwise ops are **int-only** (a float operand is
> a type error); the relative order follows Python (comparison looser than `\|` < `^` < `&` < shifts).
> A shift amount outside `0..64` is a runtime error.

**Membership `in`.** `x in xs` is a `bool`: element-of for a `list`/`set`, **key**-of for a `map`
(Python-style — `k in m` tests keys, not values), and substring-of for a `str`. The container type
directs the check; the element/key/`str` types must match the left operand.

```chezzi
3 in [1, 2, 3]      # true   — list element
20 in {10, 20}      # true   — set element
"a" in {"a": 1}     # true   — map KEY
"ell" in "hello"    # true   — substring
```

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

**Inline-expr body implicitly returns (Option A, inline-only).** A named function written in the
**inline** form (`fn a(): <stmt>` — the body on the *same line* after `:`) whose single statement is a
**bare expression** implicitly **returns that expression's value** — exactly like a closure
`fn(x): expr`. This is the only place a function body returns implicitly:

```chezzi
fn ten(): 10               # inferred '-> int'; ten() == 10
fn dbl(x: int): x * 2      # usable as a value / .map argument: [1,2,3].map(dbl) == [2, 4, 6]
fn answer() -> int: 42     # annotated inline-expr body is valid (the expr is the implicit return)
```

Only a *bare expression* inline body returns implicitly. An inline **non-expression** statement does
not: `fn a(): x = 5` (an assignment) returns `nil`, and `fn a(): return 10` is an explicit return as
written. An inline **call** returns the call's value (it is an expression-statement): `fn a(): foo()`
returns `foo()`'s value — which is `nil` if `foo` is void (that just makes `a` a void fn). An
annotated inline-expr body is checked against its return type exactly like `return <expr>` would be:
`fn a() -> int: "x"` is a type error, and a **non-nil** expr against an explicit `-> nil`
(`fn a() -> nil: 10`) is rejected with *"function returns nothing, cannot return a value"* (a nil-typed
inline expr against `-> nil`, e.g. a bare void call, stays legal).

**Multiline bodies are statement sequences (no implicit return).** A multiline body — even a
1-statement one — does **not** implicitly return: `fn a():\n    10` evaluates `10` and falls through to
`nil`. Multiline functions return via an explicit `return`.

**Return type inference.** Omitting `-> T` infers the return type: for an inline-expr body it is the
expression's type (`fn ten(): 10` infers `-> int`); otherwise it is inferred from the body's `return`
statements — the first concrete return wins, conflicting returns are a type error, and a body with no
value-returning `return` infers `nil`. Param types stay required.

**Returns on every path (enforced).** A **multiline** function with a **declared non-void return
type** (`-> int`, `-> str`, …) must return a value on *every* control-flow path. The checker rejects a
body that can fall off the end:

```chezzi
fn a() -> int:             # ERROR: 'a' can fall off the end without returning a value
    10                     #   (a multiline 1-stmt body does NOT implicitly return)
fn a() -> int:
    return 10              # fix: an explicit `return`
fn a() -> int: 10          # OK: an inline-expr body implicitly returns its expression
```

The check is path-aware and conservative: an `if`/`else` where every branch returns, an exhaustive
`match` where every arm returns, a `while true:` with no `break`, and a tail call to `exit` all count
as terminating. An inline-expr body is exempt (it implicitly returns). A bare `fn a(): 10` with **no**
return annotation infers `int` from the inline expr and is unaffected — the enforcement only fires on a
multiline body whose *declared* non-void return can be reached by falling off the end.

**`nil` is not a value.** `nil` is a return-only / void type: a void function's result (e.g.
`print(...)`, `list.push(...)`, `list.sort()`) may **not** be used in value position. Binding it
(`x := print("hi")`), passing it as an argument (`print(print("hi"))`), putting it in a collection
(`[print("hi")]`), or using it as an operand (`1 + print("hi")`) is a type error: *"expression returns
no value (nil) and cannot be used as a value"*. A bare void call **as a statement** (`print("hi")` on
its own line) is the normal use and stays legal. Returning `nil` from a function (making it void) is
*not* "using nil as a value" — that is how you write a void fn.

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
# Lazy sequences are normally built as adapter structs over this protocol (Rust-style; see
# examples/iter_adapters.chz for Take/Mapped) — the parity-clean, recommended form.

# `Iterable[T]` + `.iter()` — "can produce a cursor". Every collection now has `.iter()`, returning a
# COMPOSABLE cursor typed as the existing `Iterator[T]` existential, with `.next() -> Option[T]`
# (Some… then idempotent None). This lets a PLAIN collection flow into the same adapter pipeline as a
# hand-written struct iterator (you can't call `.next()` on a `list` directly — `.iter()` bridges it):
pipe := Mapped(Take([10, 20, 30, 40].iter(), 2), fn(x): x * 2)   # a list → Take → Mapped
for v in pipe:             # 20, 40
    print(v)
it := {1: "a", 2: "b"}.iter()   # map iterates KEYS; str → 1-char str; bytes/bytearray → int 0..=255
print(it.next())           # Some(1)
# `Iterable` is the LOOSER sibling of `Iterator`: an `Iterable` only promises `iter()`; an `Iterator`
# also has `next`. So every `Iterator` IS `Iterable` — `iter()` on a cursor / generator / `next`-struct
# returns SELF (idempotent), and all three flow into an `[S: Iterable[T]]` bound. A user struct with
# ONLY `iter(self) -> Iterator[E]` (no `next`) is for-iterable too (driven by a one-time `.iter()`):
fn total[S: Iterable[int]](src: S) -> int:
    sum := 0
    for x in src.iter():
        sum = sum + x
    return sum
total([1, 2, 3])           # 6   (a list)  — also accepts a generator or a `next`-struct
list([5, 6, 7].iter())     # [5, 6, 7]   (a cursor IS an Iterator[T], so list()/set() drain it)
# A cursor SNAPSHOTS the collection at `.iter()` (later mutation doesn't change the sequence). NOTE
# (no compile-time multi-pass safety, unfixable without ownership): each `.iter()` is a fresh cursor,
# but reusing one exhausted cursor yields nothing on a second pass. A cursor IS sendable across
# `spawn` — it crosses the airlock as a deep copy, like a `list`. `Iterable` / `Iterator` are reserved type names.

# `yield` / generators (VM-only — the frozen interpreter rejects `yield`, so parity is waived). A fn that declares
# `-> Iterator[T]` and uses `yield` is a generator: calling it returns a suspendable iterator, not a
# value. It runs lazily, suspending at each `yield` and resuming on the next `.next()`.
fn count_up(n: int) -> Iterator[int]:
    i := 0
    while i < n:
        yield i            # produce a value, suspend until the next .next()
        i = i + 1
for x in count_up(3):      # drives the generator: prints 0, 1, 2
    print(x)
# A generator can also be a struct method (`fn m(self) -> Iterator[T]`), and a generator value is a
# real `Iterator[T]`: drive it by `for`, pass it to an `[S: Iterator[T], T]` bound, or call `.next()`
# explicitly — it returns `Some(v)` per yield, then `None` once exhausted:
g := count_up(2)
match g.next():            # Some(0)
    Some(v): print(v)
    None: print(-1)
# `return` (bare only) stops a generator early; `defer`/`spawn`/`parallel:`/`wait:` are not allowed
# inside a generator. `Iterator` is a reserved type name. See examples/generators.chz (full showcase)
# and examples/generators_basic.chz.

while cond:
    cond = step()

# `break` exits the innermost loop; `continue` skips to the next iteration.
```

## 6b. Indexing & slicing  (M15)

```chezzi
xs := [10, 20, 30, 40]
print(xs[1])           # 20    — index
xs[1] = 99             # mutate in place
print(xs[-1])          # 40    — negative index counts from the end
xs[-1] = 0             # negative index works as an assignment target too
sub := xs[1:3]         # slice: half-open `start:end` → [99, 30]
print("hello"[0:2])    # he    — strings slice too (→ a new str)
print(xs[1:99])        # [99, 30, 0]   — slice bounds are clamped (no panic)
print(xs[2:])          # [30, 0]       — open end (defaults to len)
print(xs[:2])          # [10, 99]      — open start (defaults to 0)
print(xs[:])           # full copy
print(xs[0:4:2])       # [10, 30]      — `start:end:step`
print(xs[::-1])        # [0, 30, 99, 10]  — negative step reverses
print(xs[-2:])         # [30, 0]       — negative bounds count from the end
```

Slicing is **Python-style** `obj[start:end:step]`. Each component is optional — an omitted bound
defaults per direction (forward: start `0`, end `len`; reverse: start `len-1`, end "before 0"), and an
omitted step defaults to `1`. A `step` of `0` faults (`slice step cannot be zero`). The `..` operator
is unchanged — it stays the **range** used by `for i in 0..10` and match range-patterns (`0..10 =>`);
only the subscript-slice form moved from `[a..b]` to `[a:b]`.

**Negative indexing** counts from the end (`xs[-1]` is the last element) for plain indexing *and*
slice bounds, on `list`/`str`, including as an assignment target (`xs[-1] = v`). The out-of-range
rule follows Python's asymmetry: a plain `xs[-100]` on a short list **faults** (`index -100 out of
bounds (len N)`), while a slice bound `xs[-100:]` **clamps** to the start (never faults). Both engines
emit byte-identical messages.

`list[T]` slices to `list[T]`, `str` to `str`. Indexing and slicing are **protocols**, so custom
types opt in — see `Index`/`IndexSet`/`Slice` in §7b. A user `Slice` impl gets the full Python
surface via default parameters: `slice(self, start: int? = None, end: int? = None, step: int? = None)
-> R` (each component arrives as `None` when omitted, `Some(n)` otherwise).

## 6c. Comprehensions  (M16)

```chezzi
[x * 2 for x in xs]              # list: map each element
[x for x in xs if x > 0]         # list: with an `if` guard
[i for i in 0..10]               # over a range (any iterable works)
{x % 3 for x in xs}              # set: duplicates collapse
{k: v * 10 for k, v in scores}   # map: `for k, v` binds a map's entries
[x + y for x in xs for y in ys]  # nested: cartesian product (ys inner-most, like nested for-loops)
[y for xs in xss for y in xs]    # nested: a later clause references an earlier clause's variable
[x for x in xs if x > 0 for y in ys]   # a guard may follow ANY clause (filters that clause)
```

One or more `for` clauses, each binding one name (or two — `for k, v in m` — over a map's entries)
and optionally followed by one or more `if` guards. With multiple clauses the iteration nests like
nested `for` loops — the first clause is outermost, the last innermost — so a later clause may
reference an earlier clause's binding (`for xs in xss for y in xs`), and a guard after a non-final
clause filters at that level (Python semantics). The loop variables are scoped to the comprehension.
The iterable is anything a `for` loop accepts (list/map/set/str/range and struct iterators); set
elements and map keys must be `Hashable`.

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
val: V)`), and **`Slice[R]`** (`obj[a:b:c]` via `slice(self, start: int? = None, end: int? = None,
step: int? = None) -> R` — each component is `None` when omitted) protocols.
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
    fn slice(self, start: int? = None, end: int? = None, step: int? = None) -> list[int]:
        s := start ?? 0
        e := end ?? self.data.len()
        return self.data[s:e:step ?? 1]

r := Ring([10, 20, 30])
print(r[3])            # 10   — wraps; `index` dispatched
r[1] = 99              # `set_index` dispatched
print(r[0:2])          # [10, 99]   — `slice` dispatched

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

The prebuilt **`Iterable[T]`** is the looser sibling: it promises only `.iter() -> Iterator[T]` (a
fresh cursor), where `Iterator[T]` additionally promises `.next()`. Every `Iterator` IS `Iterable`
(its `iter()` returns self), so a generator and a user `next`-struct both satisfy `[S: Iterable[T]]`;
a struct with only `iter(self) -> Iterator[E]` (no `next`) satisfies it too and is for-iterable via a
one-time `.iter()`. Like `Iterator[T]`, `T` is recovered from the iterand's element. The cursor's type
is the existing `Iterator[T]` existential — there is no new value type.

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

Variant names are **bare** by default (`Circle`, `Leaf`) — they're program-global, so two enums may
not share a variant name. As an **optional spelling**, a variant may also be written **qualified**
with its enum, `Enum.Variant`, anywhere the bare form works: as a value (`Shape.Point`), a
constructor (`Shape.Circle(2)`), and in a `match` arm (`case Shape.Circle(r):`). Bare and qualified
are exactly equivalent — `Shape.Circle(7) == Circle(7)` — so the qualifier is purely a readability
aid; it does **not** create a per-enum namespace (the cross-enum collision rule above is unchanged).
A real binding named like the enum wins, so qualified access only resolves when the name on the left
isn't a local/parameter.

```chezzi
p: Shape = Shape.Point          # qualified value; same as bare `Point`
c: Shape = Shape.Circle(2)      # qualified constructor; same as bare `Circle(2)`
match s:
    Shape.Circle(r): r * r      # qualified arm; mixes freely with bare arms
    Square(n):       n * n
    Point:           0
```

`match` also works on `Result`/`Option` (they're enums under the hood):

```chezzi
match safe_div(10, 2):
    Ok(v):  print("got {v}")
    Err(e): print("failed: {e}")
```

A scrutinee can also be an **int/str/bool** (literal arms + a required `_` wildcard) or a **tuple**.
Patterns **nest**: a variant payload or tuple element may itself be a binding, a literal, a wildcard,
a tuple, or another variant — including a **nested nullary variant** like the `None` in `Some(None)`
(a refutable variant match, not a binding).

```chezzi
match point:                  # tuple scrutinee
    (0, 0):  "origin"
    (0, y):  "on the y axis"
    (x, y):  "at {x},{y}"     # an all-binding tuple arm is irrefutable (exhaustive)

match maybe_pair:             # nested: a tuple inside Some(...)
    None:         print("none")
    Some((a, b)): print(a + b)

match nested:                 # nested nullary variant — the bare `None` MATCHES (not binds)
    Some(None):    "inner none"
    Some(Some(v)): "value {v}"  # (one arm per outer variant; refine the rest with `_`)
    _:             "outer none"
```

**Or-patterns** (`p1 | p2 | ...`) match when **any** alternative matches (first match wins). They
work at the top of an arm and in sub-positions (`(1 | 2, x)`, `Some(1 | 2)`). Every alternative must
bind the **same** variables with unifiable types, so the body sees them regardless of which hit:

```chezzi
match c:
    Red | Green | Blue: "primary"   # a full enum or-pattern is exhaustive WITHOUT a `_`
match shape:
    Circle(a) | Square(a): a        # both alternatives bind `a` (same type)
```

A `bool` or-pattern `true | false` still does **not** close the bool domain — keep a `_` (one rule:
the int/str/bool literal domains are always open).

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
`if c:` with indented blocks and `return`/assignments inside — are unchanged; loops and
**multiline** function bodies are statement sequences (they return via explicit `return`). The one
exception is the **inline-expr function body** (`fn a(): <expr>`, §5), whose single bare expression is
an implicit return — exactly like a closure.

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

## 9c. Testing — `assert`, `test fn`, `chezzi test`  (M20)

Chezzi has a built-in test facility. Three pieces:

**`assert`** — a statement that **faults with its source line** when its condition is false:

```chezzi
assert x == 1                       # bare form
assert even_sum(10) == 20, "0..10"  # with a custom message
```

`cond` must be `bool`; the optional `msg` must be `str` (both checker-enforced). A passing assert is
a silent no-op; a failing one faults like any runtime error — the message is the custom `msg`, or
`"assertion failed"` — carrying the line you can see in `chezzi run` and in the test report. `assert`
works anywhere, not just in tests.

**`test fn`** — a `test` modifier before `fn` marks a test. A free `test fn` is an **independent**
test (no parameters, returns nothing); a `test fn name(self)` **method** turns its struct into a
**suite**:

```chezzi
# math_test.chz — independent tests
test fn parses_int():
    assert to_int("42") == 42

# a suite — a struct with test methods, optional lifecycle hooks, and a shared fixture
struct DbTests:
    db: Db = connect()              # built ONCE for the suite (a default field expr)
    fn before_each(self): self.db.begin()
    fn after_each(self): self.db.rollback()
    fn after_all(self): self.db.close()

    test fn empty(self):
        assert self.db.count() == 0
    test fn insert(self):
        self.db.insert("x")
        assert self.db.count() == 1
```

The four lifecycle hooks — `before_all`, `after_all`, `before_each`, `after_each` — are recognized by
name and optional (omit any you don't need). Each, if present, must be `fn name(self)` returning
nothing. The shared fixture is just a default-initialized field, mutated through `self`.

**`chezzi test [path]`** — discovers and runs every `test fn` in `*_test.chz` files. `path` defaults to
the current directory; a single `*_test.chz` file runs that file, a directory is walked recursively.
Each test runs in isolation (one failure doesn't abort the rest); the report is
`PASS/FAIL name (file:line) msg` plus a summary, and the exit code is non-zero if anything failed. A
suite runs `before_all? → [before_each? → test → after_each? (always, even on failure)]* → after_all?`,
constructing the suite instance once. (The runner is VM-only; only the `assert` primitive runs on both
engines.) Known limit: an assert that faults inside *imported* (non-test) code reports the test file's
path, not the library file's.

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

**Triple-quoted strings.** `"""…"""` and `'''…'''` produce an ordinary `str` with the **same**
escapes and interpolation as a regular string — the one difference is that a single (or double)
unescaped quote inside is a literal char, so you can embed quotes without backslashes. (Regular
strings already span literal newlines, so triple quotes are about quotes, not multi-line per se.)

```chezzi
print("""She said "hello" to {name}""")   # unescaped quotes + interpolation
print('''it's a "quoted" word''')          # apostrophes and double-quotes, literal
```

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

**Format specifiers.** An interpolation may carry a Python-style format spec after a `:` —
`{expr:spec}`. The mini-language is a coherent subset:

```
{expr:[[fill]align][sign][0][width][.precision][type]}
```

```chezzi
print("|{name:<10}|")     # left,  width 10            → "|thuan     |"
print("|{name:>10}|")     # right                      → "|     thuan|"
print("|{name:^10}|")     # center                     → "|  thuan   |"
print("|{name:*^10}|")    # center, '*' fill           → "|**thuan***|"
print("{42:06}")          # zero-pad to width 6        → "000042"
print("{-7:06}")          # sign kept before zeros     → "-00007"
print("{3.14159:.2f}")    # 2 decimals                 → "3.14"
print("{0.1357:.1%}")     # percent: ×100, 1 decimal   → "13.6%"
print("{255:x} {255:X}")  # hex (lower/upper)          → "ff FF"
print("{255:b} {255:o}")  # binary / octal             → "11111111 377"
print("{12345.678:.2e}")  # scientific                 → "1.23e4"
print("{5:+d}")           # force a leading '+'         → "+5"
print("{greeting:.5}")    # string precision truncates → "hello"
```

- **align**: `<` left, `>` right, `^` center; an optional **fill** char may precede it (default space).
  Numbers default to right-align, everything else to left.
- **sign**: `+` forces a leading `+` on non-negative numbers (numeric only).
- **`0`**: zero-pad numerics to *width* (the sign stays ahead of the zeros).
- **width**: minimum field width. **Capped at 4096** — a larger width (e.g. `{x:>9999999999}`) is a
  parse error, *not* a multi-gigabyte allocation.
- **precision** `.N`: float decimals; on a **string** it **truncates** to N chars (Python parity);
  also capped at 4096.
- **type**: `d` int · `f` fixed float · `x`/`X` hex · `b` binary · `o` octal · `e` scientific ·
  `%` percent (×100 then `%`). A float type char (`f`/`e`/`%`) promotes an int.

A **bare** `{expr}` (or `{expr:}` with an empty spec) renders exactly as before — e.g. a whole float
prints `5.0`. An **unknown type char** or trailing junk in the spec, and a **type/value mismatch**
(e.g. `{name:d}` on a string, `{x:.2f}` on a string, zero-pad on a non-number), are both reported as
**errors before any output** — surfaced with a runtime-error prefix and *not* caught by `chezzi check`
(interpolation specs are validated when the program starts running, after the type-check phase). The
spec is parsed once, shared by both engines (`src/fmtspec.rs`), so the VM and interpreter produce
byte-identical output. The `:` split is bracket/quote-aware — a `:` inside an index, string key, or
slice (`{m["a:b"]}`, `{xs[1:2]}`) is *not* the spec separator. **Ternaries:** a bare interpolated
ternary `{if b: a else: b}` works (its colons are part of the expression, not a spec); to attach a
spec to a ternary, **parenthesize** it — `{(if b: 1 else: 2):>5}`. (Edge case: on a *malformed* spec
over a side-effecting expression, the interpreter evaluates the expression before erroring while the
VM errors at compile time, so observable stdout-before-error can differ; well-formed programs are
always byte-identical.)

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

> **Implemented — shipped through Tier-D.** `chezzi run` defaults to the real OS-thread M:N scheduler
> (`Channel`/`Shared`/`Executor`, netpoller-backed `std.net`); size its worker pool with
> `--threads=N` / `CHEZZI_THREADS` (`0` = all cores). `--serial` selects the cooperative
> engine (the byte-identical parity oracle). Full
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
- **`Ref[T]`** (`import std.ref`) — the **in-task** mutable box: `Ref(v)` then `r.get() -> T`,
  `r.set(v)`, `r.update(fn(x): ...)`. Backed by `Rc<RefCell>`, so it is a true *shared reference*
  within one task: a closure that closes over a `Ref[T]` and any other holder see each other's writes
  — the answer to "I need a mutable value to close over or pass by reference" without hand-rolling a
  one-field struct. It is **not sendable**: copying a `Ref` across a `spawn`/`submit` would silently
  duplicate the box, so the checker rejects it (`non-sendable value of type Ref[T]`). Cross a task
  boundary with `Shared[T]` instead.
- **`Shared[T]`** — the cross-task mutable box, same `s.get()` / `s.set(v)` / `s.update(fn(x): ...)`
  API as `Ref` but synchronized and **sendable**. The mutation ladder is `value` (copied) →
  `Ref[T]` (in-task, unsynchronized) → `Shared[T]` (cross-task, synchronized).
- **`Atomic[T]`** — the cross-task **atomic** box (sibling of `Shared`, sendable handle, value-first
  `Atomic(v)`): `a.load()`, `a.store(v)`, `a.exchange(v) -> T` (returns old), `a.cas(expected, new) ->
  bool`, and on numeric `T` `a.add(x) -> T` / `a.sub(x) -> T` (return the new value; checked-overflow
  like `+`/`-`). Each op is atomic across threads. Use it for counters/flags/CAS-loops; `Shared` for
  arbitrary-transform updates.
- **`timer(ms) -> Channel[bool]`** — a one-shot timeout channel: `timer(500).recv()` blocks ~500ms then
  yields `true` (level-triggered — ready on any recv at/after the deadline). The composable timeout
  primitive; it races against real channels inside a `wait:` — there is **no separate `recv_timeout`**
  (a `wait` over a channel and a `timer` subsumes it).
- **`wait:` (select)** — race several channel `recv`s; the first ready arm wins (source-order priority).
  `wait:` then arms `v := ch.recv():` (or `result = ch.recv():` / `_ := ch.recv():`), an optional
  non-blocking `else:` (must be last), and `timer` arms for timeouts. Recv-only (sends never block on
  unbounded channels); a closed+empty arm is skipped; all-closed + no `else` faults. **Shipped on all
  engines** — the cooperative default, the interpreter, and `--parallel` (the M:N multi-channel blocking
  park, including `timer` arms, has landed). See
  [`concurrency.md §6d`](concurrency.md) and `examples/wait_select.chz`.
- **`std.cancel` (cancellation token)** — `import std.cancel`; `cancel.manual()` / `cancel.timeout(ms)`
  build a `Token` you thread down a call tree (sendable). Poll `tok.cancelled() -> bool` in CPU loops,
  race `tok.done() -> Channel[bool]` in a `wait:` for IO loops, `tok.cancel()` from anywhere; also
  `reason() -> str?` (`"cancelled"`/`"timeout"`/`None`), `deadline_at()`. **Tree-structured:**
  `tok.derive() -> Token` (or `cancel.derive(parent)`) builds a child — cancelling/timing-out a parent
  cancels every transitively-derived child (live link, tightest-deadline inheritance), while cancelling
  a child never touches the parent. Cooperative (signals, doesn't forcibly interrupt — poll in CPU
  loops). See [`concurrency.md §6e`](concurrency.md) and `examples/cancel_*.chz`.
- **`Channel.trip()`** — flip a permanent level-trigger latch: the channel is then ready (`recv`/
  `try_recv`/`wait` → `true`) for every receiver (the manual fan-out behind `std.cancel`'s `done()`).
- **Sendability:** captures are copies, **read-only** inside a task (reassign = error); only sendable
  types (scalars/str/containers+structs of sendable/`Channel`/`Atomic`/`Shared`/a `std.cancel` `Token`)
  cross — not closures, native handles, or `Ref`.

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

## 12b. Dynamic C-ABI FFI — `extern "lib":`

Call C functions in a shared library directly, with full static type-checking. An `extern "lib":`
block (indentation, not braces — `{` is a map literal) lists body-less C signatures; each becomes a
module-global callable, bound at module init by `dlopen` + `dlsym` and dispatched at runtime via
`libffi`. A missing library or symbol fails at startup.

```chezzi
extern "libm.so.6":
    fn cos(x: float) -> float
    fn sqrt(x: float) -> float

extern "libc.so.6":
    fn strlen(s: str) -> int

print(cos(0.0))        # 1.0
print(sqrt(4.0))       # 2.0
print(strlen("hello")) # 5
```

**Marshalling (v1 — scalars + opaque `ptr`):** `int` ↔ C `long` (so a 32-bit-`int` C API is called at
the wrong ABI width — declare against `long`-based APIs, or expect truncation), `float` ↔ C `double`,
`bool` ↔ C `int`, `str` → null-terminated `const char*` (a `char*` return is copied into a Chezzi
`str`), and `ptr` ↔ C `void*` (an **opaque handle** — see below). No implicit `int`→`float` (`cos(2)`
is a type error — pass `2.0`). A no-return signature (`fn srand(seed: int)`) — or an explicit
`-> nil` — maps to C `void`; `nil` is a **return-only** type (it is rejected as a parameter). The
checker rejects any other non-scalar param/return (list/map/set/tuple/struct/enum/…) with a *not
C-marshallable* error. Calls run inline (a slow C call pins its worker under `--parallel`) and produce
identical output on all three engines (VM / `--interp` / `--parallel`).

**Opaque handles (`ptr`).** A C library built around a handle (`FILE*`, `sqlite3*`, a
`create`/`use`/`destroy` context) returns a `void*` you hold and pass back. Declare it as `ptr` — a
builtin opaque type (a peer of `int`/`str`; no import needed in a signature). A `ptr` is **untyped**
(one `ptr` for every handle — Chezzi never distinguishes a `FILE*` from a `sqlite3*`, exactly like
`ctypes`), holds no data Chezzi can read (zero-copy — the data stays behind the address), and is
**never auto-freed**: call the library's own destroy yourself (forgetting **leaks**, like the `char*`
limit). A `ptr` supports only `==`/`!=` against another `ptr` and being passed/returned — no methods,
no fields, no arithmetic. NULL is allowed (a returned NULL is **not** a fault, unlike a `str` return —
it is a legitimate "creation failed" signal). The NULL sentinel and a null test live in **`std.ffi`**:

```chezzi
import null, is_null from std.ffi

extern "libc.so.6":
    fn fopen(path: str, mode: str) -> ptr
    fn fclose(f: ptr) -> int

f := fopen("/dev/null", "r")
if is_null(f):              # or: f == null()
    print("open failed")
else:
    print(fclose(f))       # 0 — the FILE* handed straight back; zero-copy
```

A `ptr` prints as `<ptr null>` / `<ptr>` — never the raw address (it is non-deterministic across
runs/engines, so printing it would break two-engine parity). A `ptr` is **sendable** (a plain
address) — it crosses a `spawn`/channel airlock by value.

**Caveats:** a `str`-declared return that comes back `NULL` (e.g. `getenv` of an unset var) is **not**
silently turned into `nil` — that would break the static non-null `str` guarantee — it raises a
recoverable runtime fault (catch with `recover:`). A returned `char*` is copied immediately and never
`free`d, so a `malloc`'d return **leaks**, and a non-NUL-terminated return over-reads (the signature
is a user assertion across the C trust boundary).

An `extern "lib":` block is a **top-level declaration only** — it is bound at module init, so nesting
it inside `if`/`for`/`fn` is a parse error. An extern fn also may **not** be named after a builtin
(`len`/`range`/`int`/`float`/`str`/`ord`/`chr`/`set`), `print`, a constructor
(`Channel`/`Shared`/`Atomic`/`timer`/`Executor`), or any of your `struct`/enum-variant names — those
resolve to a special op before a plain call, so the extern would be silently shadowed; the checker
rejects the collision (*'…' is a builtin/reserved name*).

**Known v1 limits (see `docs/spec.md` for detail):**
- **C `int` width:** Chezzi `int` (i64) marshals as C **`long`** — 64-bit on supported **LP64 unix**
  targets. 32-bit/`unsigned` C ints are out of scope, and there is **no `int32` type** (feature-frozen).
  Non-unix (LLP64, where C `long` is 32-bit) is **unsupported**: the checker rejects `extern` there.
- **`char*` return leaks:** a `str` return is copied into a Chezzi string but the C pointer is never
  `free`d (no ownership transfer) — a `malloc`'d return **leaks** on every call.
- **No `--parallel` serialization:** extern calls are **not** serialized; calling a **non-reentrant** C
  function (`strtok`, `gmtime`/`localtime`, `setlocale`, static-buffer APIs) from multiple workers
  **races at the C level**. Use thread-safe/reentrant C only under `--parallel`.

- **Untyped + un-freed handles:** a `ptr` is one opaque type for every C handle (no `FILE*` vs
  `sqlite3*` checking — passing the wrong handle is C-level UB, the author's assertion) and is **never
  auto-freed** (call the library's own destroy; forgetting **leaks**).

**Deferred (v1 limits):** structs-by-value, callbacks / function pointers, varargs, the rich Rust
`Box<dyn Any>` userdata handle (for compiled-in Rust libraries), nullable `str?` returns, and `char*`
ownership transfer / `free`. (Opaque C `void*` handles — `ptr` — **shipped**, see above.)

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

**`std.ffi`** (the C-ABI handle vocabulary, pairs with the opaque `ptr` type — see §12b):
`null() -> ptr` (the NULL sentinel) and `is_null(p: ptr) -> bool`. The `ptr` *type* itself is builtin
(usable in `extern` signatures without import); only these value helpers are imported.

Importable: `std.io`, `std.math`, `std.str`, `std.cmp`, `std.os`, `std.json`, `std.process`,
`std.fs`, `std.time`, `std.regex`, `std.request`, `std.net`, `std.ffi`, `std.iter`, `std.ref`.

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
