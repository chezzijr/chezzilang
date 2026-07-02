# Chezzi — Syntax Reference

A scannable cheat-sheet for the whole language. Revise from this; feed it to an LLM as context.
For the *why* behind each choice see [`spec.md`](spec.md); for token names see [`../src/lexer/mod.rs`](../src/lexer/mod.rs).

> **Implementation status:** the language is fully designed but built incrementally.
> Tags like `(M3)` mark which milestone first makes a feature *run*. Syntax is stable regardless.

---

## 1. Lexical basics

```chezzi
# comments start with '#' and run to end of line

# A '#' comment block touching a declaration (no blank line between) is its DOC-COMMENT,
# shown on editor hover (LSP) above the type. Stacked '#' lines join into one doc.
fn greet(name: str):           # ^ this two-line block documents `greet`
    print("hi {name}")

# detached — a BLANK line below breaks the run, so this line is NOT part of the doc

# this line IS the doc for Point (adjacent, no gap)
struct Point:
    x: int
```

- **Blocks = indentation** (spaces only; tabs are a lex error). A block opens after a `:` line.
- **Logical lines** end at a newline. Blank / comment-only lines are ignored.
- **Identifiers:** letter or `_`, then letters/digits/`_`. Case-sensitive.
- **Doc-comments:** any plain `#` line(s) *immediately above* a declaration (`fn`/method, `struct`,
  `enum`, `protocol`, `newtype`, `type` alias, top-level binding) become its doc. The doc surfaces on
  LSP hover above the `chezzi` type fence **today for free functions, methods, struct constructors, and
  top-level bindings**; for `enum`, `protocol`, `newtype`, and `type` aliases the doc is parsed and
  attached to the declaration but does **not** yet surface on hover (their names/constructors record no
  hover info). Enum-variant **constructor names** are the exception: they hover their ctor signature at
  both the declaration and the use site (e.g. `Val(int)` → `fn(int) -> Col`, generic `Full(T)` →
  `fn(T) -> Box[T]`), but a variant carries no doc-comment of its own (there is no per-variant doc
  field), so only the signature surfaces. The doc is the *contiguous* run of comment lines with NO blank line
  between the last one and the declaration — a blank line detaches earlier comments. Stacked lines join
  with newlines (one leading `# ` stripped per line). An inline trailing comment on the decl line is
  not a doc. Purely informational: doc-comments never affect type-checking or execution.
  *(Reinstall the LSP after upgrading — `cargo install --path . --features lsp --bin chezzi-lsp` — the
  installed binary is a snapshot.)*

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
r"\d+\s"      # str — raw string: VERBATIM; no escapes (\ is literal), no interp ({ } are literal)
r'C:\tmp'     # str — raw, single-quote / uppercase R"…" both work; cannot contain its own quote
r"""{"k": [1,2]}"""  # str — triple raw: embeds quotes + braces verbatim (best for JSON / brace-heavy)
b"\x01\x02AB"  # bytes — byte-string literal; \xHH hex byte + \n \t \r \\ \" \' \0; no \u, no interp
b'\xff'       # bytes — single quotes / uppercase B'…' both work; raw byte >=0x80 must use \xHH
bytearray([1, 2, 3])  # bytearray — MUTABLE byte buffer; constructor-only (no literal), see below
[1, 2, 3]     # List[int]
{"a": 1}      # Map[str, int]
```

**`bytes` — immutable byte sequence (Python `bytes` model).** A `b"..."` / `b'...'` literal holds raw
bytes (the lexer applies escapes + strips the `b` prefix). Accepted escapes: `\xHH` (exactly two hex
digits → one byte `0x00`–`0xFF`, the only way to write a byte ≥ 0x80) plus `\n \t \r \\ \" \' \0`.
`\u{…}` is **rejected** (a byte literal is byte-exact, not UTF-8), as is a raw non-ASCII source char.
No interpolation. Operations: `b[i]` → `int` 0–255 (Index protocol; out-of-range is a recoverable
panic), `b[a:b:c]` → `bytes` (Slice protocol over byte offsets — open bounds / step / reverse /
negative), `for x in b` yields `int`, `b.len()` is the byte count, `==`/`!=` are structural, and `bytes`
is `Hashable` (valid `map`/`set` key). `str(b)` / `print(b)` / interpolation use the Python `b'...'`
repr (printable ASCII literal, others `\xHH`). `bytes` is immutable — `b[i] = x` is a type error.

**Raw strings — `r"..."` / `r'...'` and triple `r"""..."""` / `r'''...'''`.** A raw string is an
ordinary `str` (`Ty::Str`, identical downstream) but lexed **verbatim**: there is **no escape
processing** (`r"\d+"` is a backslash, `d`, `+` — *not* an escape; `r"C:\tmp"` is a literal Windows
path) and **no interpolation** (the always-on `{expr}` of normal strings is OFF — `r"{}"` prints a
literal `{}`, and `r"{x}"` stays `{x}` even with `x` in scope). It is the escape hatch for the
always-on interpolation: instead of doubling braces (`"{{}}"`) you write `r"{}"`. The `r`/`R` prefix
fires only when immediately followed by a quote (so a variable named `r` is unaffected, exactly like
`b`). The **short** form `r"..."` cannot contain its own closing quote (no escaping in raw — switch
quote style or use the triple form); the **triple** form `r"""..."""` embeds lone single/double
quotes verbatim, which is how you write brace-and-quote-heavy data like JSON: `r"""{"k": [1,2]}"""`.
Uppercase `R"..."` is accepted (mirrors `B"..."`). (Out of scope for now: a combined raw-bytes
prefix `rb"..."`, and Rust-style `r#"..."#` hash delimiters — the triple form already embeds quotes.)

**`bytearray` — the MUTABLE sibling (Python `bytearray` model).** Constructor-only — there is **no**
`ba"..."` literal (the `b"..."` literal already makes a `bytes`). Four forms: `bytearray()` (empty),
`bytearray(N)` (N zero bytes, Python semantics), `bytearray(b)` (a mutable copy of a `bytes`),
`bytearray([ints])` (each element 0–255). Operations: `ba[i]` → `int` 0–255, **`ba[i] = x`** mutates
in place (`IndexSet`; the value must be 0–255 and the index in range, else a recoverable panic — the
new capability `bytes` lacks), `ba[a:b:c]` → a NEW `bytearray` (mutable copy, byte offsets),
`for x in ba` yields `int`, `ba.len()`, `.push(int)` (append one byte 0–255), `.pop() -> Option[int]`,
`.extend(bytes | bytearray | List[int])` (append in place). `==`/`!=` are structural; cross-type
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
| any iterable → list | `List(it)` | `List[T]` | `it` is any **for-iterable**; `T` is the element type. `List[T]()` / `List()` are the empty forms. |
| any iterable → set | `Set(it)` | `Set[T]` | dedup; `T` must be `Hashable`; `Set[T]()` / `Set()` (0 args) is the empty set |
| iterable of 2-tuples → map | `Map(it)` | `Map[K, V]` | `it` yields `(K, V)` pairs; last-wins on dup keys; `K` `Hashable`. `Map[K, V]()` / `Map()` are the empty forms. |

`.encode()`/`.decode()` are **UTF-8 only** — there is no encoding-name argument (latin1/utf16 are an
explicit future non-goal). `"héllo".encode().decode() == "héllo"` round-trips through a multi-byte
char; `b"\xff\xfe".decode()` faults **recoverably** (catchable by `recover:`), never a panic.

`List(it)` / `Set(it)` / `Map(it)` accept **any for-iterable** — exactly what `for x in it` accepts:
`list`, `set`, `str` (per-char `str`), `bytes`/`bytearray` (per-byte `int`), `map` (its keys),
`range`, and a user struct with `next(self) -> Option[T]`. They do **not** require a formal
`Iterable[T]` bound — they reuse the same internal iterable union as the `for` loop. The empty
**container constructors** are first-class: `List[T]()` / `Map[K, V]()` / `Set[T]()` take a
**turbofish** that pins the element/key/value type, and the bare `List()` / `Map()` / `Set()`
zero-arg forms produce an empty container whose type is refined from the expected type or first use
(e.g. `xs: List[int] = List()`, then `xs.push(1)`) — exactly the inference that already served `Set()`
and the `[]` / `{}` literals. (As with those literals, a bare `List()`/`Map()`/`Set()` that is *never*
pinned — neither annotated nor constrained by a later op — is a static error requiring an annotation;
see "Empty-collection element typing" below.) (The `[]` / `{}` literals remain the idiomatic empty forms; the
constructors are there for when a literal is awkward, e.g. binding a type parameter.) A turbofish with
an iterable argument checks the elements against the type arg: `List[int]([1, 2])` is fine,
`List[int](["a"])` is a static error. `Map(it)`'s element must be **exactly a 2-tuple** `(K, V)` — a
non-2-tuple is a **static** type error (caught by the checker, not at runtime).

> **`Map(it)` vs `xs.map(f)` — these do NOT clash.** `Map(pairs)` is the free-function **constructor**
> (a bare-name call). `xs.map(f)` is the `List` higher-order **method** (a field/method call on a
> receiver). They live in separate namespaces — the parser routes a bare `Map(...)` as a builtin call
> and a `obj.map(...)` as a method dispatch — so `Map([(1, "a")])` builds a `Map[int, str]` while
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
name: str = "chezzi"   # declare with explicit type
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
`Ref[T]` box. `Ref` is a **reserved global** (like `Result`/`Option`/`Iterator`) that backs the
`ref` keyword, so **no import is needed** — both `ref T` and the explicit `Ref[T]` work standalone.
(`import std.ref` still works as a harmless no-op for older code.) Roughly C++'s `int&`, where the
explicit `Ref[T]` (`r.get()/.set()/.update()`) is closer to Rust's `Rc`.

```chezzi
r: ref int = 0     # a fresh box holding 0 — no import needed
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
  generic argument, a collection element, a tuple element, a struct field, or on a destructuring binding
  — use a first-class `Ref[T]` there.
- **Concurrency (important).** `ref`/`Ref` are **same-task** aliasing only. A `ref T` is a `Ref[T]`
  box, which is **non-sendable**: capturing or passing the box across the `spawn` / `parallel:` /
  `Channel` airlock is **rejected** by the checker. To move a value across, deref the ref into a plain
  copy first; for genuine cross-task shared mutation use `Shared[T]`, never `ref`.

### Closure capture — by binding kind

A closure's capture semantics depend on **what kind of binding** the captured name is. The rule is
consistency-by-rule, not by uniformity: a *local* dies with its frame, so it is **copied** (snapshot
at closure creation); a *global* never dies, so it is **referenced live**. Opt a local into
by-reference sharing with `ref T` (above).

| binding | captured as | `x := 10; f := fn() -> int: x; x = 20; f()` |
|---|---|---|
| plain local | snapshot at creation | `10` |
| global | live reference | `20` |
| `ref` local | shared box | `20` |

So a closure over a plain local sees the value *as of* the closure's creation (later writes to the
local are invisible); a closure over a global reads the module binding's *current* value each call
(later writes are visible); a closure over a `ref` local shares the box, so writes are visible both
ways. Across tasks use `Shared[T]` (a `ref`/`Ref` box is non-sendable). Runnable contrast:
[`examples/closure_capture_scopes.chz`](../examples/closure_capture_scopes.chz).

### Built-in types

| Type | Example | Notes |
|------|---------|-------|
| `int` | `42` | 64-bit signed |
| `float` | `3.14` | 64-bit |
| `bool` | `true` | |
| `str` | `"hi"` | UTF-8 |
| `bytes` | `b"\x01AB"` | immutable byte sequence; `b[i]`→int, `b[a:b:c]`→bytes, iterates int; `Hashable` |
| `bytearray` | `bytearray([1,2])` | MUTABLE byte buffer (constructor-only); `ba[i]`→int, `ba[i]=x`, slice→bytearray, `push`/`pop`/`extend`; NOT `Hashable` |
| `List[T]` | `[1, 2]` | growable |
| `Map[K, V]` | `{"a": 1}` | insertion-ordered hash map; `K` is any `Hashable` type |
| `Set[T]` | `{1, 2, 3}` | deduped, insertion-ordered hash set; `T` any `Hashable` type; empty is `Set()` |
| `tuple` | `(1, "a")` | fixed-arity, immutable |
| `Result[T, E]` | `Ok(x)` / `Err(e)` | §9; shorthand `T!E`, or `T!` (E = `Error`) |
| `Option[T]` | `Some(x)` / `None` | §9; shorthand `T?` |

> **Naming.** The three builtin containers spell their type **and** constructor in PascalCase —
> `List`/`Map`/`Set` (e.g. `List[int]`, `Set(xs)`). The lowercase `list`/`map`/`set` are no longer
> type/ctor names. (Literal syntax is unchanged: `[…]`, `{k: v}`, `{a, b}`.) `tuple` is deliberately
> left lowercase for now — a possible later follow-up.

**Type shorthand.** In any type position, `T?` is sugar for `Option[T]`; `T!E` for `Result[T, E]`;
and `T!` for `Result[T, Error]` (E defaults to the built-in `Error` protocol). Examples: `int?`,
`List[int]?`, `int!` (= `Result[int, Error]`), `int!DbErr` (= `Result[int, DbErr]`). Pure spelling —
`Some`/`None`/`Ok`/`Err`, `match`, and `?` behave exactly as on the long forms.

**One-way `int`→`float` widening (C-like).** An `int` value flows into a `float` SLOT automatically and
is converted to a real `f64`; the reverse (`float`→`int`) is always a type error (lossy). Widening
fires at every value-DEFINITION boundary: a typed binding (`x: float = 3` → `3.0`), a `float` function /
method / closure parameter (incl. when you pass an `int` *variable*, not just a literal — it is coerced
at the callee), a `float` parameter DEFAULT value (`fn g(a: float = 3)`), a `-> float` return, a `float`
struct field (`P(3)` for `v: float`), and a
**mixed-numeric-literal** collection — a list/map literal with ≥1 float literal infers `List[float]` /
`Map[_, float]` and coerces its int literals (`xs: List[float] = [1, 2.3]`, a `Map[_, float]` value, or
a bare `[1, 2.3]`). Because the conversion is real, the stored value behaves as a float everywhere — e.g.
`x: float = 3` makes `x / 2 == 1.5` (float division), not `1`. The mixed-type arithmetic / comparison
operators (`1 + 2.0`, `1 < 2.3`, `1 == 2.3`) follow the same one-way rule. Anti-lossy cases stay type
errors: `y: int = 2.3`, `fn f() -> int: return 2.3`, a `float` into a `List[int]`, and an `int`→`float`
into a **newtype** (a newtype is nominal — no widening across its boundary). Widening is
**scalar-at-the-sink** — a compound/nested float annotation is NOT widened: `List[List[float]] = [[1]]`,
`float? = Some(3)`, `float! = Ok(3)`, an all-int literal `List[float] = [1, 2]`, and a non-literal RHS
(`List[float] = f()`) all stay type errors; write explicit floats (`[[1.0]]`, `Some(3.0)`, `[1.0, 2.0]`)
or a mixed literal. Two further scoped carve-outs: a plain reassignment `x = 3` to a `float`-declared
local is rejected (a reassignment target is type-blind, like `p.x = 3`), and an UN-annotated NON-literal
mixed collection (`xs := [a, b]` with `a:int`, `b:float`) is inferred `List[float]` but its non-literal
`int` element is not widened at runtime (rare; annotate `xs: List[float] = …` for the conversion).

## 4. Operators & precedence

Highest → lowest. Same row = same precedence, left-associative unless noted.

| Level | Operators | Notes |
|-------|-----------|-------|
| 1 | `f(x)` `a.b` `a[i]` | call, field access, index |
| 2 | `?` | error propagation (postfix, §9) |
| 3 | `not` `-` (unary) | |
| 4 | `*` `/` `%` | `*` also list repeat: `[0] * 3` (and `3 * [0]`, commutative) |
| 5 | `+` `-` | `+` also list concat: `[1,2] + [3,4]`; `-` also set difference: `a - b` |
| 6 | `..` | range (end-exclusive) |
| 7 | `<<` `>>` | bitwise shift (int-only) |
| 8 | `&` | bitwise and (int) / set intersection (`Set[T]`) |
| 9 | `^` | bitwise xor (int) / set symmetric-difference (`Set[T]`) |
| 10 | `\|` | bitwise or (int) / set union (`Set[T]`) |
| 11 | `<` `<=` `>` `>=` | |
| 12 | `==` `!=` `in` | `in` = membership, yields `bool` (see below) |
| 13 | `and` | |
| 14 | `or` | |
| 15 | `\|>` | pipe (§11), left-assoc |

> This table is the contract for the Pratt parser. The relative order follows Python (comparison
> looser than `\|` < `^` < `&` < shifts). A shift amount outside `0..64` is a runtime error. A left
> shift (`<<`) that drops a significant bit overflows like `+ - * / %` — a recoverable
> `integer overflow in Shl` (e.g. `1 << 63`), not a silent wrap; round-trip-safe shifts incl.
> `-1 << 63 == INT_MIN` still succeed. `>>` never overflows.
>
> **Collection operators.** `+ *` and `& ^ |` also operate on collections, with behaviour identical
> to the equivalent methods (so a mismatched element type is a type error, same as the method form):
> - `List[T] + List[T]` → concat (= `.concat`); element types must match. `[] + [1]` infers `List[int]`.
> - `List[T] * int` / `int * List[T]` → repeat (commutative, Python-style); `n <= 0` → `[]`. A giant
>   `n` raises a recoverable `list repeat capacity overflow`, never a process abort.
> - `Set[T] | Set[T]` → union (= `.union`), `& ` → intersection (= `.intersection`), `-` → difference
>   (= `.difference`), `^` → symmetric-difference (no method form). Result preserves insertion order.
>
> The compound-assign forms work too: `xs += ys` / `xs *= n` (list), `s |= t` / `s &= t` / `s ^= t` /
> `s -= t` (set) — identical to the binary form.
>
> Plain bitwise (`& ^ | << >>`) on **int** operands is unchanged; `<< >>` are int-only (no set form),
> and a float operand is still a type error.

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

**Closure-parameter inference.** An unannotated closure parameter is resolved to a concrete type from
the context the closure appears in (it is never left dynamic). In priority order: **(1)** the
*expected type* of the slot the closure sits in — a call argument matching a `fn(...)` parameter
(`s.update(fn(x): x + 1)` → `x: T` of `Shared[T]`; `[1,2,3].map(fn(x): x + 1)` → `x: int`;
`Mapped(it, fn(x): x * 2)` → `x` from the field type), an assignment/`:=` to a `fn`-typed target, a
struct `fn`-field, or a `fn`-typed return position; **(2)** a `match` whose scrutinee is the bare
param (`fn(x): match x: E.A: …; E.B: …` → `x: E`); **(3)** a member access **uniquely owned by one
type** — a method only `str`/`bytes` has (`fn(x): x.upper()` → `x: str`) or a field/method exactly
one struct declares (`fn(x): x.f`). Arithmetic/comparison/indexing and any member shared by >1 type
(`x.len()` — on `str`/`list`/`map`/`set`, so it never pins) do **not** pin — they are *checked*
against a type a higher source resolved. Once resolved, the closure's `fn` signature is filled in and
**call sites are type-checked** like a named function. A parameter that *nothing* resolves
(`g := fn(x): x + 1` with no slot or match) is an error:
`cannot infer type of parameter 'x'; add a type annotation` — annotate it (`fn(x: int): …`). A
closure passed to a **generic** slot whose type parameter only *it* would pin (`store(fn(a): a + 1)`
for `fn store[T](x: T) -> T`) is likewise un-inferable → annotate (`fn(a: int): …`): the param is
never silently left dynamic, so a later call can never trap.

A type *annotation* also counts as expected-type context for source **(1)** when it surrounds a
generic constructor or generic function call: a `let`-binding's declared type, a function's declared
return type, and a call argument's declared parameter type each pin the called generic's type
parameters, which in turn fix any closure params that depend on them. So
`h: Heap[int] = Heap([], fn(x, y): x < y)` type-checks — the `Heap[int]` annotation pins `T=int`,
which gives the comparator `x, y: int` — and likewise for `fn mk() -> Heap[int]: return Heap([], fn(x,
y): x < y)` and `take(Heap([], fn(x, y): x < y))` where `take(h: Heap[int])`. The annotation only
fills type params the *arguments* leave free, so an explicit turbofish or a concrete argument still
wins over it (an argument that pins `T` differently from the annotation is the usual mismatch error).
When the bound value is an `if`/`match` *expression*, the annotation reaches **every** branch
(`h: Heap[int] = if rev: Heap([], fn(x, y): x > y) else: Heap([], fn(x, y): x < y)`), independent of
branch order. The one remaining gap: an annotation does **not** yet reach a generic ctor nested inside a *container
literal* (`a: List[Heap[int]] = [Heap([], fn(x, y): x < y)]`) — annotate the closure params or use a
turbofish there.

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
value-returning `return` infers `nil`. Param types stay required. Inference is **order-independent**: a
recursive call contributes no type (it is skipped, the non-recursive returns decide), and forward
references / mutual recursion resolve via a fixpoint — so a callee defined *after* the caller still
yields the caller's precise inferred type. A function that is genuinely un-inferable (pure self- or
mutual recursion with **no concrete base case anywhere**) keeps a permissive inferred type; annotate
it with an explicit `-> T` for a precise type. This applies uniformly to **struct/enum methods** as
well as free functions: an inferred method return flows to call sites (`P(3).val()` is typed by the
inferred return, not `Unknown`) and to **protocol satisfaction** (an inferred `compare(self, o)`
yielding `bool` fails `Comparable`, which requires `-> int`, exactly as an explicit `-> bool` would).

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
so give same-named methods the same parameter shape or unique names. A method that **reuses a built-in
method name** (`map`, `push`, `len`, `add`, …) does still get default/named support, but **only when
the receiver's struct/enum type is statically known** at this pre-type pass — a typed local
(`c := Counter(0)` or `t: Tag = …`), an inline constructor call (`Counter(0).add(amount=5)`), or a
struct-returning function call (`mk().add(...)`). A genuine builtin receiver (a `List`/`Set`/`Map`/`str`
value) keeps routing to the builtin method untouched; a named call to a builtin-colliding method whose
receiver type is *not* statically known (e.g. an unannotated parameter, or an inferred `m := E.Variant`)
is rejected with an accurate "reuses a built-in method name — bind it to a typed local or pass
positionally" error. Defaults are **not**
supported on **closures** or on **enum variant constructors** — note this is the variant
*constructor*; an enum's *methods* take defaults just like struct methods. (Per §above, a default may
be any expression that doesn't
reference another parameter — a literal, a global, arithmetic, or a call; only param-referencing
defaults are rejected.)

Built-ins take no named arguments, with **one** exception: **`print`** accepts `sep=` (default `" "`,
joins the positional args) and `end=` (default `"\n"`, appended after) — both `str` (see `docs/stdlib.md`).
So `print("a", end="")` writes `a` with no trailing newline, and `print("a","b", sep="-", end="!")`
writes `a-b!`. Any other named argument on a built-in is an error.

**Keyword arguments through a function VALUE (Swift-style labels).** Named arguments also work through a
first-class **function value**, not just a direct call — a `fn(...)` type carries its parameters'
**labels**, so a value bound to a user function (or closure), or reached through a `fn(...)`-typed
parameter, accepts keyword args:

```chezzi
fn greet(name: str, greeting: str):
    print(greeting, name)

g := greet
g(name="Bob", greeting="Hi")           # "Hi Bob" — by label, through a value
g(greeting="Hi", name="Bob")           # same — labels may be reordered
g("Bob", "Hi")                         # positional through the value still works

fn apply(f: fn(name: str) -> nil):     # labels ride on the fn TYPE
    f(name="X")                        # keyword through a HOF parameter
```

Labels are **surface-only** (Swift SE-0111): `fn(str) -> nil` and `fn(name: str) -> nil` are the **same
type** — mutually assignable, so an unlabelled callback flows into a labelled parameter and vice-versa
(no impact on existing HOF/callback/protocol code). Two limits, both by design: **(1)** a value call
must supply **every** parameter — declaration-site **defaults do not fill through a value** (`h :=
hasdefault; h()` is an error, while a **direct** `hasdefault()` still fills the default); **(2)**
first-class **built-in** function values (`p := ord`) take **no** keyword arguments (labels are a
user-function surface). Named arguments through a value evaluate in **parameter-declaration order**, the
same as a direct named call, and work in `defer`/`spawn` position too (`defer d(name="Zoe")`). Resolution
is fully static (the checker rewrites the keyword call to a positional one; the runtime `Op::Call` /
`DeferCall` / `SpawnCall` stay positional), so all engines produce identical output.

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

for i in 0..10:        # range: 0..10 is 0 through 9 (end-exclusive, ascending only)
    print(i)

# `a..b` is always ascending: `for i in 10..0` yields nothing (no auto-reverse). To count down or
# by N, use the 3-arg `range(start, end, step)` builtin — `step` is a non-zero int, negative counts
# down (end-exclusive). It returns a materialized `List[int]` (capped at 10M; `step == 0` faults).
for i in range(10, 0, -1):   # 10, 9, 8, … , 1
    print(i)
print(range(0, 10, 2))       # [0, 2, 4, 6, 8]   — by-N
print((0..10)[::2])          # [0, 2, 4, 6, 8]   — a range literal is sliceable like a list

for item in items:     # iterate a list
    print(item)

for k in counts:       # iterate a map → its keys (insertion order)
    print(k)

for k, v in counts:    # iterate a map's entries → key + value
    print("{k}={v}")

for a, b in pairs:     # destructure a List[(A, B)] — N names over a List[tupleN]
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
List([5, 6, 7].iter())     # [5, 6, 7]   (a cursor IS an Iterator[T], so List()/Set() drain it)
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
only the subscript-slice form moved from `[a..b]` to `[a:b]`. A **range literal is sliceable** like a
list: `(0..10)[::2]` materializes the (ascending) range then slices it with the same `start:end:step`
machinery (`(0..5)[::-1]` → `[4, 3, 2, 1, 0]`).

**Negative indexing** counts from the end (`xs[-1]` is the last element) for plain indexing *and*
slice bounds, on `list`/`str`, including as an assignment target (`xs[-1] = v`). The out-of-range
rule follows Python's asymmetry: a plain `xs[-100]` on a short list **faults** (`index -100 out of
bounds (len N)`), while a slice bound `xs[-100:]` **clamps** to the start (never faults). Both engines
emit byte-identical messages.

`List[T]` slices to `List[T]`, `str` to `str`. Indexing and slicing are **protocols**, so custom
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

### 7a. Static (associated) methods — the "no self ⇒ static" rule

A method's **first parameter** decides its call shape. If it is named `self`, the method is an
**instance method**, called `value.method(args)` (unchanged). If the first parameter is **not** `self`
— or the method takes **no parameters** — it is a **static (associated) method**, called
`Type.method(args)` on the type name itself (the Rust `fn new` ergonomic). The two are different call
shapes: an instance method is **not** callable as `Type.method`, and a static method is **not**
callable as `value.method` (each errors clearly, pointing at the other form).

Static methods are **additive** — the positional all-fields constructor `Name(...)` still works. They
unlock **named / alternative** constructors and **validating** constructors (returning `Result` /
`Option`) that the positional ctor cannot express:

```chezzi
struct Rect:
    w: int
    h: int
    fn square(s: int) -> Rect:        # static: first param is not `self`
        return Rect(s, s)
    fn area(self) -> int:             # ordinary instance method coexists
        return self.w * self.h

struct Email:
    addr: str
    fn parse(s: str) -> Result[Email, str]:   # validating ctor
        if "@" in s:
            return Ok(Email(s))
        return Err("missing @")

r := Rect.square(5)        # Type.method(args) — static call
print(r.area())            # 25
match Email.parse("a@b"):
    Ok(e): print(e.addr)
    Err(m): print(m)
```

**Enums** get static methods too (e.g. a `from_str(s) -> Option[Color]`). For an enum, a **variant**
name **always wins** over a static-method name on `Enum.x` — so a variant and a static method may
**not** share a name (a collision is a declaration-time error). This keeps `Color.Red` always the
variant.

**Generic static methods** are reached with a **type-level turbofish** — the enclosing type's args sit
on the **type**:

```chezzi
struct Box[T]:
    items: List[T]
    fn empty() -> Box[T]:
        return Box([])

b := Box[int].empty()      # turbofish on the TYPE: Box[int].empty()
```

The type-level turbofish takes **one or more** type args — multi-param types use the comma form
(`Pair[K, V].empty()`, `Result[int, str].Ok(5)`).

A static method may **also declare its OWN `[U]`** type parameters — they sit on the **member**
(`make[U]`), the **declaration-site rule**: a type argument is written where its parameter is
declared. Member-level args are **inferred** from the call by default; a **member-level turbofish**
pins them only when they can't be (`Box.make[str]("hi")`). The two compose — `Box[int].make[str](x)`
supplies the enclosing `T = int` *and* the method `U = str`:

```chezzi
struct Box[T]:
    val: T
    fn make[U](x: U) -> Box[U]:       # member declares its own [U]
        return Box(x)

a := Box[int].make(5)                 # U inferred from the arg ⇒ Box[int]
b := Box[int].make[str]("hi")         # combined turbofish: T=int + U=str ⇒ Box[str]
```

A method param name may **not shadow** an enclosing type param (`fn make[T]` inside `Box[T]` is an
error). A method-level turbofish on a member that declares **no** type params (or a builtin like
`xs.len[int]()` / `xs.iter[int]()`) is an arity error. **Instance** methods take the same member-level
turbofish, multi-arg: `pair.first[int, str](1, "x")`. Static methods do **not** participate in
**protocol** satisfaction — protocols stay instance-only. Static methods on `newtype` are not
supported yet (struct + enum only).

**Uniform parse rule (any receiver).** `recv.name[X](args)` parses as a method turbofish on **any**
receiver — not just a bare ident, but a call result, a field, or an index:

```chezzi
W(1).cast[str]("a")          # call-result receiver
mk().cast[str]("a")          # factory-result receiver
h.w.cast[str]("a")           # field receiver
xs[0].cast[str]("a")         # index receiver
W(1).cast[Map[str, int]](m)  # nested-generic type arg, non-bare receiver
```

The trade-off: index-then-call of a **fn-valued** field needs **parens** on any receiver —
`(recv.name[k])(args)` — because `recv.name[k](args)` reads `k` as a type and parses as a turbofish.
This is uniform with the bare-ident receiver, which already required parens. A **numeric** index
(`arr[0].handlers[0](20)`) still parses as index-then-call (an int is not a type), and a plain
subscript with no following call (`obj.items[0]`, `m.data[k]`) is always an ordinary index.

## 7b. Generics & protocols  (M7)

**Generic functions** take type parameters in `[…]` after the name. A parameter may carry a
**bound** — a protocol the instantiating type must satisfy. Type arguments are normally **inferred**
from the call, but may be **given explicitly** at the call site: `id[int](42)` and the struct form
`Pair[int, str](1, "one")`. For a generic **enum** the args go on the **type** (the declaration-site
rule), not the variant — `Box[int].Full(9)`, `Result[int, str].Ok(5)` (see §8). Explicit args pin the
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
Ordering is overloaded through `Comparable`; arithmetic is overloaded through the per-operator
protocols **`Add`/`Sub`/`Mul`/`Div`/`Mod`** (binary, methods `add`/`sub`/`mul`/`div`/`mod(self,
other: Self) -> Self`, powering `+`/`-`/`*`/`/`/`%`) and **`Neg`** (unary, method `neg(self) -> Self`,
powering unary `-`). A struct/enum defining the matching method gets that operator on its values;
`int`/`float` satisfy all six intrinsically. (C-style: `/` truncates and `%` is the int remainder, so
`Div`/`Mod` are `Self -> Self` with no float-return surprise.)

```chezzi
struct Vec2:
    x: int
    y: int
    fn add(self, o: Vec2) -> Vec2:
        return Vec2(self.x + o.x, self.y + o.y)
    fn div(self, o: Vec2) -> Vec2:
        return Vec2(self.x / o.x, self.y / o.y)
    fn neg(self) -> Vec2:
        return Vec2(-self.x, -self.y)

print((Vec2(1, 2) + Vec2(3, 4)).x)   # 4    — `+` calls Vec2.add
print((Vec2(6, 8) / Vec2(2, 4)).y)   # 2    — `/` calls Vec2.div
print((-Vec2(1, 2)).x)               # -1   — unary `-` calls Vec2.neg
```

**Protocol embedding (super-protocols).** A protocol body may, in addition to `fn` signatures, list
**embed lines** — one-or-more protocol refs joined by `+` — to pull in those protocols' requirements.
Embeds and `fn` sigs interleave in any order; a body of only embed lines is a *bundle*. A type
satisfies the protocol iff it satisfies every embed (transitively) **and** has every own method, so a
bound flattens at use sites (`[T: Arithmetic]` requires add/sub/mul/div). The builtin **`Arithmetic`**
bundle is `Add + Sub + Mul + Div`. Collision rules: an own `fn` whose name matches an embedded-required
method is an error; two embeds requiring the same method with the *same* signature dedup silently (a
legal diamond — so `Arithmetic + Add` is fine), with *differing* signatures it is an error; a cyclic
embed is an error.

```chezzi
protocol Arithmetic:        # builtin — shown for reference
    Add + Sub + Mul + Div

protocol VectorSpace:       # embeds two protocols and adds its own requirement
    Arithmetic + Neg
    fn dot(self, o: Self) -> int

fn combine[T: Arithmetic](a: T, b: T) -> T:   # +, -, *, / all available on T
    return (a + b) * (a - b) / b
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
    data: List[int]
    fn index(self, key: int) -> int:
        return self.data[key % self.data.len()]
    fn set_index(self, key: int, val: int):
        self.data[key % self.data.len()] = val
    fn slice(self, start: int? = None, end: int? = None, step: int? = None) -> List[int]:
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
fn to_list[S: Iterator[T], T](xs: S) -> List[T]:
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
type Scores = Map[str, int]
uid: UserId = 7        # UserId and int are the same type
```

**Newtypes** (`newtype Name = <type>`, M21) are the *distinct-type* counterpart to a transparent
`type` alias: `Name` wraps the underlying type but is a **separate, nominal** type that does NOT
silently mix with the raw underlying (Go's "defined type" model). The point is to catch accidental
mixing at compile time — a bare `int` is **not** assignable to a `UserId` parameter/binding/field,
and a `UserId` is **not** accepted where a raw `int` is expected.

```chezzi
newtype UserId = int
newtype Meters = float

fn needs_int(x: int): ...

uid := UserId(10)      # construct (a call with one arg of the underlying type)
n: int = int(uid)      # unwrap via the cast builtin → 10
# x: UserId = 10       # ERROR: an int literal is not a UserId
# needs_int(uid)       # ERROR: a UserId is not an int
```

Crossing the boundary is always **explicit** — either **construct** (`UserId(10)`) or **cast-unwrap**
via the matching cast builtin: `int(uid)` / `float(m)` return the inner value (and for a
`newtype N = str`, `str(n)` unwraps the inner string; for an aggregate underlying the matching
aggregate builtin unwraps too — see *Aggregate underlyings* below). There is no `.value` field and no
auto-deref.
From another module the constructor takes the **qualified path** — `geo.UserId(10)`, exactly like a
qualified enum variant.

For a **numeric** underlying (`int`/`float`), arithmetic and ordering **auto-flow same-type only**:
`a OP b` where both operands are the *same* newtype applies the underlying's **native** op and
re-wraps — `Meters + Meters -> Meters` (also `- * / %`), `Meters < Meters -> bool`. Equality
(`==`/`!=`) works between two values of the same newtype for **any** underlying
(`UserId == UserId -> bool`). A `str`/`bool` newtype does **not** auto-inherit `+`/`<` in v1 — define
a method or unwrap to operate (operator auto-flow for non-numeric underlyings is a follow-up).
Mixing a newtype with its raw underlying (`Meters + 1.0`) or with a *different* newtype
(`Meters + Seconds`) is a type error — that rejection is the whole point.

A newtype may carry its own **methods** (a trailing-colon block, like a struct/enum), and satisfies
the **non-operator** prebuilt protocols by defining the relevant method — `str(self)` (Stringable
display override) and `hash(self)` (so it can be a `map`/`set` key — opt-in, *not* inherited from the
underlying) — so it passes into those protocol-bound generics (`fn show[T: Stringable](x: T)`). The
**operator** protocols (`Add`/`Sub`/`Mul`/`Div`/`Mod`/`Neg`/`Comparable`) are **not** satisfiable by a
newtype method: a newtype's own `add`/`div`/`compare`/… is never dispatched as an operator (the
same-type arm always auto-flows to the underlying's native op/ordering), so only a **numeric**
underlying supplies them — a numeric newtype satisfies `Add`/`Sub`/`Mul`/`Div`/`Mod`/`Comparable`
intrinsically (native same-type ops above), while a `newtype Name = str` with an `add` (or `compare`)
method does **not** pass `fn twice[T: Add](x: T)` (or `fn sorted[T: Comparable](xs: T)`) — its `<`
would silently use the underlying's native ordering, never the method, so the checker rejects it.

```chezzi
newtype Meters = float:
    fn str(self) -> str:
        return "{float(self)}m"

print(Meters(1.5))                 # 1.5m       (str(self) override)
print(float(Meters(1.0) + Meters(2.0)))  # 3.0  (same-type +)
```

**Aggregate underlyings.** A `newtype` may wrap an aggregate (`newtype Names = List[str]`), but it
gets **identity + construct + unwrap + its own methods only** — it does NOT auto-inherit the
underlying's operations: `names.push(..)`, `names[i]`, and `for x in names` do not resolve. Reach the
underlying through an explicit method or the **unwrap cast** — the matching aggregate builtin, exactly
as `int(uid)` unwraps a scalar newtype: `List(names)` returns a copy of the inner `List[str]`
(likewise `Set(..)` / `Map(..)` for a set/map underlying). (Operation-forwarding for aggregates and
`derive` remain out of scope.)

**Generic newtypes.** A `newtype` may carry generic type parameters (`newtype Stack[T] = List[T]`),
the Go defined-type model extended to generics — the underlying and the method signatures may
reference `T`, and the type args ride on the value's type so a cast-unwrap recovers the
instantiation. A type-parameterized newtype is **methods-only**: it gets **no native operator
auto-flow** — even `newtype Box[T] = T` over a numeric `T` does not get `+`/`<` for free. Operators
come strictly from the newtype's own methods + protocol satisfaction (the scalar `UserId = int` /
`Meters = float` numeric auto-flow above is unchanged). Construction infers the type args from the
argument (`Stack([1, 2])` ⇒ `Stack[int]`); when an argument can't bind them (an empty `[]` can't
pin `T`), an enclosing **annotation** (a `let`/return/parameter type) now pins them
(`e: Stack[str] = Stack([])`), or supply them with a **turbofish**: `Stack[int]([])` (still needed
where there is no annotation, e.g. a nested `ConcurrentMap(RwShared({}))` — expected, not a bug). A cast-unwrap propagates the instantiation:
for `s: Stack[int]`, `List(s)` is `List[int]` (not bare `list`), and `int(b)` for `b: Box[int]`
unwraps to `int`.

```chezzi
newtype Stack[T] = List[T]:
    fn size(self) -> int:
        return List(self).len()
    fn top(self) -> Option[T]:
        xs := List(self)
        return if xs.len() == 0: None else: Some(xs[xs.len() - 1])

s := Stack([1, 2, 3])      # inferred Stack[int]
print(s.size())            # 3
t: Option[int] = s.top()   # method dispatch substitutes T -> int
xs: List[int] = List(s)    # cast-unwrap propagates: List[int]
e: Stack[str] = Stack([])        # annotation pins T=str (the empty list can't); turbofish also works
```

(Static / associated methods like `Type.method()` and typeclass-style associated requirements
`T.zero()` remain out of scope — a separate follow-up.)

The prebuilt **`Stringable`** protocol (`str(self) -> str`) customises how a value is rendered. A
struct *or enum* that defines `str(self) -> str` overrides its default repr (`Name(field=value, …)`
for a struct, `Variant(payload)` for an enum) everywhere it is printed: by `print`, by the `str()`
builtin, and inside `{…}` string interpolation — including when nested in a list / tuple / map / set
/ enum payload. Types without a `str` method keep the default repr. Like `Comparable`, `Stringable`
is prebuilt and works as a generic bound (`fn show[T: Stringable](v: T)`).

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

label: Map[Point, str] = {}
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
    items: List[T]
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
        Shape.Circle(r): return 3.14 * float(r * r)
        Shape.Square(n): return float(n * n)
        Shape.Point:     return 0.0
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
        Tree.Leaf:          return 0
        Tree.Node(v, l, r): return sum(l) + v + sum(r)   # v is int — T substituted in the match
```

A **payload-carrying** variant's type args are inferred from the payload, but may be pinned
explicitly at the **declaration site** — the type args go **on the TYPE**, not on the variant:
`Tree[int].Node(1, Tree.Leaf, Tree.Leaf)`. This is the declaration-site rule (§7b): a generic
declared on the type (`enum/struct/newtype [T]`) is pinned on the type (`Tree[int].Node`), and a
generic declared on the member is pinned on the member. Multi-param enums use the comma form —
`Result[int, str].Ok(5)`, `Result[int, str].Err("e")`. The same type-level turbofish supplies args
the payload can't bind (a nullary variant: `Box[int].Empty`) and drives a generic **static** method
(`Box[int].empty()`). (The old gliding form `Tree.Node[int](…)` — type args on the variant — is no
longer accepted; the checker redirects you to `Tree[int].Node(…)`.)

Enums may carry **methods** — `fn name(self, …)` blocks written **after all variants**, exactly like
struct methods. The receiver `self` is the whole enum value (a method body typically `match self`).
A generic enum's methods may use its type parameters (`fn get(self) -> T`). Methods are name-resolved
on the value (`shape.area()`), satisfy structural protocols (so an enum can define `str(self)` for
`Stringable`, `hash(self)` for `Hashable`, `add`/`sub`/`mul`/`div`/`mod`/`neg`/`compare` for
`Add`/`Sub`/`Mul`/`Div`/`Mod`/`Neg`/`Comparable`, and pass into protocol-bound generics like
`fn twice[T: Add](x: T)`), and overload the matching operators (`a + b`, `a / b`, `-a`, `a < b`) just
as struct methods do. (No `derive` — write the method.)
A `test fn` is **not** allowed inside an enum body (enum test suites aren't wired — it would silently
never run); test suites are a struct-only feature.

```chezzi
enum Light:
    Red
    Yellow
    Green
    fn cost(self) -> int:           # method matching on self
        match self:
            Light.Red:    return 0
            Light.Yellow: return 1
            Light.Green:  return 2
    fn next(self) -> Light:         # a method may return a new variant
        match self:
            Light.Red:    return Light.Green
            Light.Yellow: return Light.Red
            Light.Green:  return Light.Yellow

print(Light.Green.cost())           # 2
print(Light.Red.next().cost())      # 2
```

Variants are **scoped under their enum** and must be written **qualified** as `Enum.Variant`
everywhere they're used: as a value (`Shape.Point`), a constructor (`Shape.Circle(2)`), and in a
`match` arm (`case Shape.Circle(r):`). A bare user-variant name is a compile error (the error names
the enum so you can fix it). Because variants are per-enum, **two enums may share a variant name**
(`Color.Red` and `Light.Red` are distinct values). A real binding named like the enum wins, so
qualified access only resolves when the name on the left isn't a local/parameter. (The built-in
`Ok`/`Err`/`Some`/`None` for Result/Option stay **bare** — see below.)

```chezzi
p: Shape = Shape.Point          # qualified value
c: Shape = Shape.Circle(2)      # qualified constructor
match s:
    Shape.Circle(r): r * r
    Shape.Square(n): n * n
    Shape.Point:     0
```

A `match` arm may also use the **module-qualified** spelling `module.Enum.Variant` — symmetric with
module-qualified construction (`geo.Color.Red`). For an enum from a whole-module `import geo` (so its
bare `Color` isn't in scope), you can match it directly without a named import:

```chezzi
import geo
import geo as g                  # an `as` alias works as the binder too
c := geo.Color.Red
match c:
    geo.Color.Red:      print("red")
    geo.Color.Green:    print("green")
match c:
    g.Color.Red:        print("R")    # aliased binder
    g.Color.Green:      print("G")
match shp:
    geo.Shape.Circle(r): r * r        # payload bindings work as usual
```

The module binder (`geo`/`g`) is the bound module name (last path segment or `as` alias). The checker
validates that the module is bound and owns the named enum, then it's dropped — matching keys on the
same `(enum, variant)` identity as the bare/named-import form, so output is byte-identical. (A
plain `module.Variant` — dropping the enum name — is **not** accepted; the enum name is mandatory.)

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
    Color.Red | Color.Green | Color.Blue: "primary"   # a full enum or-pattern is exhaustive WITHOUT a `_`
match shape:
    Shape.Circle(a) | Shape.Square(a): a        # both alternatives bind `a` (same type)
```

A `bool` or-pattern `true | false` still does **not** close the bool domain — keep a `_` (one rule:
the int/str/bool literal domains are always open).

**Guards** (`pattern if cond:`) add a boolean test to an arm — it matches only when the pattern
binds *and* the guard (which sees the pattern's bindings) is true; otherwise the next arm is tried.
A guarded arm is never irrefutable, so it can't satisfy exhaustiveness on its own — keep a `_`.
Likewise a variant arm whose payload contains a *refutable* sub-pattern (a literal, range, or nested
variant — e.g. `Some(0)`, `Pair(0, y)`) covers only part of that variant's domain, so it does **not**
close the variant; only an unguarded arm whose payload is all wildcards/plain bindings does. (A guarded
variant arm may therefore be followed by an unguarded fallback on the *same* variant — `E.A(n) if c`
then `E.A(n)` — without a "duplicate arm" error.)

**Matching on an unannotated closure parameter.** An unannotated closure param is now *inferred* to a
concrete type from its context (see "Closure-parameter inference" above), so `fn(x): match x: E.A: …;
E.B: …` resolves `x: E` and is checked like any typed scrutinee — the call site is enforced
(`g(5)` → error) and exhaustiveness is the ordinary enum/literal rule (cover all variants, or `_`; a
literal match needs a `_`). A **structural** pattern (an enum variant, a tuple, or a variant/`Ok`/
`Err`/`Some`/`None` payload) over a value whose type genuinely *can't* be inferred — a residual
`Unknown`, e.g. a tuple-element binding `a` in `match x: (a, b): match a: E.A: …` where `x` itself was
only shape-pinned — is **rejected** (`cannot match a <tuple|variant> pattern on a value of un-inferable
type; annotate it`): matching a shape/tag on an un-typed value would trap at runtime, and a trailing
`_` cannot rescue it (the destructure runs first). Literal/range/`_`/binding patterns over an
un-inferable value stay allowed (a value-compare or bind never traps); a heterogeneous literal match
(`1` then `"b"`) is rejected once the first arm pins the scalar type. Annotate the parameter
(`fn(x: (E, int)): …`) to make a structural match legal.

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

Int literal and range patterns may be **negative** (`-3:`, `-10..-5:`; either bound independently,
`-10..5`, `0..-5`). This is int-only — there is **no float pattern**, so a negative (or positive)
float like `-3.0:` / `3.0:` is a parse error, the same as today.

```chezzi
match temp:
    -3:       "exactly minus three"
    -10..-5:  "cold"
    _:        "warm"
```

### `match` and `if` as expressions

Both branch forms can also be used as **expressions** that produce a value — handy for
initializing a variable without a pre-declared mutable:

```chezzi
# match-expression: multiline, exhaustive, each arm body is a single value-expression
label := match shape:
    Shape.Circle(r): "round"
    Shape.Square(n): "boxy"
    Shape.Point:     "dot"

# if-expression: inline, ternary-style — `else` is REQUIRED
sign := if n > 0: "pos" else: "neg"
# chained: `else if` chains without parentheses (the final `else` stays mandatory)
grade := if s >= 90: "A" else if s >= 80: "B" else: "F"
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
`?` must match the enclosing function's return **kind**: a `Result`-`?` needs a `Result`-returning fn (and its
propagated error type must fit the function's error type), an `Option`-`?` needs an `Option`-returning fn. A
nothing-returning top level / `fn main()` accepts either (the unhandled `Err`/`None` exits the program). Mixing
kinds — e.g. a `Result`-`?` inside an `Option`-returning fn — is a compile error.

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

**`panic(msg: str)` — raise a panic yourself.** The faults above (OOB, divide-by-zero, overflow) are
raised by the runtime; `panic(msg)` raises the *same* recoverable fault from your own code. It
**unwinds** — it does not return a value (it is **not** sugar for `return Err(...)`, which already
exists for *expected* errors). The nearest enclosing `recover:` catches it as `Err(e)` with
`e.message() == msg`; uncaught, it terminates the program with that message and a non-zero exit code,
exactly like an integer overflow. `defer`s run as it unwinds, like any panic. Because `panic` never
returns, it is *bottom-typed*: it type-checks in any position — as a statement, as the diverging tail
of a branch (no explicit `return` needed), or in an expression (`x := if ok: v else: panic("no")`
takes `v`'s type).

```chezzi
r := recover:
    panic("boom")                 # raised here, caught at the boundary
match r:
    Ok(v):  print(v)
    Err(e): print("recovered: {e.message()}")   # → recovered: boom
```

### `defer` — block-scoped cleanup  (M16)

`defer <call>` schedules a call to run when the **enclosing lexical block** exits — on **every**
path: fall-through, `break`/`continue`, normal return, a `?` short-circuit, or a panic. Deferred
calls run **LIFO** (last registered, first run); an unwind crossing several blocks runs each block's
defers inner-block-first. The receiver and arguments are evaluated **at the `defer` statement** (Go
semantics); only the call itself is delayed.

Every indented block is a defer scope: the function body, a loop body, an `if`/`else if`/`else` branch,
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
closure, or a name bound to one). The four **universe builtin functions** `print`, `ord`, `chr`, and
`panic` are first-class values, so `defer print("done")` (etc.) works directly — and they can be
bound and passed like any function (`f := ord; f("a")`, a HOF arg). **Type / container / runtime
constructors** (`int`, `str`, `List`, `Map`, `Channel`, `range`, …) and user struct/enum constructors
are **not** first-class values — wrap them: `fn log(m: str): print(m)` then `defer log("done")`.
Note: the **value form of `print`** (a bound `p := print`) is a **fixed one-argument call** using the
defaults **`sep=" "`, `end="\n"`** — the variadic multi/zero-arg shapes AND the `sep=`/`end=` named
arguments stay **direct-call-only** (`print(a, b, sep=",")`), because they need the specialized print
opcode a bound value doesn't reach. The direct call keeps its full variadic surface. Because a
deferred/spawned `print` runs its value form, passing `sep=`/`end=` there is a **type error**
(`defer print(a, sep="-")` is rejected) rather than silently ignored. All four builtins
are **sendable** — a value bound to one (`f := ord`) crosses the `spawn` airlock and runs in the
spawned task, on both the serial and the OS-thread engine; likewise **`spawn print(...)` is accepted**
directly, symmetric with `defer print(...)`. A **user binding shadows** one of these
names in value position exactly like any other name: `fn f(ord: int): print(ord)` (a param),
`for chr in xs:` (a loop var), or a top-level `chr := "…"` all read the *binding*, not the builtin —
only an unbound name resolves to the first-class builtin (and a same-named module global read *before*
its definition line is a use-before-def error, just like any other global). `defer` composes with
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

`cond` must be `bool`; the optional `msg` must be `str` (both checker-enforced) and is evaluated
**only on failure** (a passing assert never runs it). A passing assert is a silent no-op; a failing
one faults like any runtime error — the fault message is `assertion failed: <msg>` when a `msg` is
given, or just `assertion failed` otherwise — carrying the line you can see in `chezzi run` and in
the test report. `assert` works anywhere, not just in tests.

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
point** in the language — `main` is an ordinary function. Run a file directly and only its top-level
runs, so you call `main` yourself:

```chezzi
fn main():
    print("hello")

main()        # running this FILE directly needs the call; nothing runs main for you
```

For a **project**, `chezzi.toml`'s `[project] entrypoint` declares what a bare `chezzi run` executes.
It is a dotted module path, optionally suffixed with **`:function`**:

```toml
[project]
entrypoint = "src.main:main"   # run src/main.chz's top-level, then call its `main`
# entrypoint = "src.main"      # (no :function) run src/main.chz's top-level only — call main yourself
```

With a `:function` suffix, a bare `chezzi run` runs the entry module's top-level and **then calls that
function** — so the source needs no trailing call, and you can swap which function runs (e.g.
`main` → `other_main`) by editing the manifest alone. A named function that doesn't exist (or isn't a
function) is a clear error, not a silent no-op. Without the suffix, the module top-level runs and
nothing is auto-called. Running an explicit file (`chezzi run src/main.chz`) is always top-level-only
(scripting model) regardless of the manifest.

**`chezzi init [dir]`** scaffolds a new project (`dir` defaults to the current directory, created if
missing): a `chezzi.toml` manifest, `src/main.chz` (a `fn main():` — **no** trailing call, since the
scaffolded `entrypoint = "src.main:main"` calls it), and an example `src/main_test.chz` with
`test fn`s. It refuses to overwrite an existing `chezzi.toml`. The manifest is **both a root marker and
a parsed manifest**: the toolchain reads its `[project]` keys (`name`/`version` metadata, and
**`entrypoint`**, scaffolded active as `"src.main:main"`). It is a tiny fixed-schema reader
(`[section]` headers, `key = "value"` string pairs, `#` comments); an empty `chezzi.toml` is a valid
root marker with no entrypoint.

## 10. Strings & interpolation

```chezzi
name := "chezzi"
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

**Raw strings — the verbatim opt-out (`r"…"`).** Interpolation is always on, so a literal brace in a
normal string must be doubled (`"{{}}"`). The escape hatch is a **raw string**: prefix any quote form
with `r`/`R` (`r"…"`, `r'…'`, triple `r"""…"""` / `r'''…'''`) and the contents are taken **verbatim** —
**no escape processing** and **no interpolation**. So `\` is a literal backslash (great for regex
`r"\d+\s"` and Windows paths `r"C:\tmp"`), and `{`/`}` are literal (`r"{}"` prints `{}`, `r"{x}"` stays
`{x}`). The result is a plain `str`, identical downstream. The short form cannot contain its own
closing quote (no escaping — use the other quote style or the triple form); the triple form embeds
quotes verbatim, which is how you write JSON: `r"""{"k": [1,2]}"""`.

```chezzi
x := 5
print(r"{}")                  # {}        ← literal braces, no doubling needed
print(r"\d+\s")               # \d+\s     ← literal backslashes, no escapes
print(r"{x}")                 # {x}       ← raw: NOT interpolated
print("{x}")                  # 5         ← normal string still interpolates (raw is opt-in)
print(r"""{"k": [1,2]}""")    # {"k": [1,2]}   ← triple raw embeds quotes + braces
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
print("|{name:<10}|")     # left,  width 10            → "|chezzi    |"
print("|{name:>10}|")     # right                      → "|    chezzi|"
print("|{name:^10}|")     # center                     → "|  chezzi  |"
print("|{name:*^10}|")    # center, '*' fill           → "|**chezzi**|"
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

**Float formatting (plain) never uses scientific notation.** A bare float — `print(x)`, `str(x)`, or
a `{x}` interpolation with no spec — always renders its **full decimal expansion**, never an `e`
exponent. So `1.0e20` prints `100000000000000000000.0` (Python's `repr` gives `1e+20`), `1.5e-9`
prints `0.0000000015` (Python `1.5e-09`), and very large / very small magnitudes print out in full
rather than collapsing to an exponent. The rendered digits are **shortest-round-trip-correct** (the
fewest digits that parse back to the same `f64`), just spelled out in full — so output stays exact
and Python-feel readable, only more verbose at the extremes. This is an **intended divergence** from
CPython, which switches to scientific notation past a magnitude threshold. When you *want* an
exponent, ask for it explicitly with the `:e` format spec (`"{1.0e20:e}"` → `"1e20"`).

Core-type string methods (built in — no import needed):

```chezzi
s.len()          s.upper()        s.lower()
s.trim()         s.strip()        s.split(",")
s.starts_with("ab")  s.ends_with("yz")  s.contains("b")
",".join(parts)  # join: separator.join(List[str])
s.chars()        # → List[str] of 1-char strings; also `for c in s:` iterates them
s.replace("a","b")  s.repeat(3)   s.reverse()      s.pad_left(4,"0")
s.index_of("x")  s.count("x")     s.strip_prefix("p")  s.strip_suffix("s")
s.split_lines()  # → List[str] split on "\n"
s.to_int()       s.to_float()     # → int? / float? (Some/None — None on bad input)
"a" + "b"        # concatenation
```

The `ends_with`/`replace`/`repeat`/`reverse`/`pad_left`/`index_of`/`count`/`strip_prefix`/
`strip_suffix`/`split_lines` methods forward to the matching `std.str` free fns (no import needed).

A character is just a 1-char `str` (Python-style — there is no `char` type): index with `s[i]`,
iterate with `for c in s:` or `s.chars()`, and bridge to codepoints with `ord`/`chr`.

List methods (built in): `xs.push(x)` `xs.pop()` `xs.len()` `xs.reverse()` `xs.contains(v)`
`xs.index_of(v)` `xs.sum()` `xs.sort()` (ascending, in place); `xs.concat(ys)→list` (new list) and
`xs.extend(ys)` (append in place, → nil); higher-order `xs.map(f)` `xs.filter(p)` `xs.fold(init, f)`;
`xs.sort_by(fn(a, b) -> int)` — a custom comparator (negative = `a` before `b`), stable, in place;
and `xs.sort_by_key(fn(x) -> K)` — sort by a derived key (`K` Comparable: int/float/str or a struct
with `compare`), stable, in place.

> **Empty-collection element typing (refine-on-first-use).** An un-annotated empty `[]` / `{}` /
> `Set()` has no element/key type yet; the **first** mutating op on the binding — `.push`/`.add`/
> `.insert`/`.extend`, or `m[k]=v` — **pins** the element/key/value type, and later ops are checked
> against that pinned type. So `out := []; out.push(1)` is `List[int]` and a later `out.push("s")` is a
> type error (it would read as `List[int]`). A **heterogeneous / protocol** collection therefore needs
> an explicit annotation — `shapes: List[Shape] = []` — which is also clearer to readers.
> A **never-constrained** empty — one that nothing ever pins or constrains (e.g. `b := []` that is only
> *read* into an untyped sink: `print(b)`, `b.len()`) — is a **static error**: `cannot infer element type
> of empty collection; add a type annotation`. Annotate it (`b: List[int] = []`, `m: Map[str,int] = {}`,
> `s: Set[int] = Set()`) or pin it with a turbofish constructor (`List[int]()`). Besides the mutating
> first-use ops above, a binding is also **constrained** (so it does *not* error) when a concrete-typed
> value flows into it: a whole-binding reassignment / compound-assign / tuple-assignment (`b = [1, 2]`,
> `b += [1]`, `a, b = [1], [2]`), or passing/returning it into a concrete collection sink — a typed
> binding, a typed function parameter (`f(b)` where the param is `List[int]`), or a typed `return`. The
> direct-literal forms (`f([])`, `return []`, `c: List[int] = []`) likewise leave no un-inferred slot, so
> those never error. A binding is *also* considered constrained — so it does not error — once it **escapes
> as a value** into another binding or structure: an alias (`c := b`), a plain or field assignment
> (`c = b`, `bx.items = b`), or nesting in a collection literal (`c := [b]`); the requirement then moves
> to the new binding (which records its own if *it* stays unrefined) rather than firing a false positive.
> (Reassigning *another* empty — `b = []` — does not constrain it; the requirement
> stands. A terminal read that does not escape — `print(b)`, `b.len()` — likewise does not constrain it.) The
> `Hashable` key/element ban applies the moment the type is concrete (and a non-Hashable key/element
> like a `float` is rejected at the insertion site even on an empty `{}`/`Set()`). The pin is
> **persistent** (scope-wide first-use pinning): the first mutating op fixes the element type for the
> binding's whole scope, even across sibling `if`/`else`/statement-`match` arms and a loop body — so
> building a heterogeneous collection split across branches/arms is a type error, exactly like the
> literal `[1, "s"]`. `xs := []; if c: xs.push(1) else: xs.push("s")` is **rejected**. This accepts a
> sound zero-trip over-approximation: `xs := []; for i in []: xs.push(1); xs.push("s")` rejects even
> though the loop body never runs. Limitations: refinement fires only on a **simple-variable** receiver
> (`obj.field.push(…)` / `xss[0].push(…)` are not refined — annotate those), and the one remaining
> uncaught sliver is a differently-typed push done as a *side effect* inside sibling **if-EXPRESSION /
> match-EXPRESSION** value-arms (value-arms refine independently so branch value inference stays
> correct; rare, since a value-arm is a single expression and the mutating ops are statements).

Map methods: `m.get(k)→V?` `m.has(k)` `m.keys()` `m.values()` `m.remove(k)` `m.len()`;
`m.merge(n)→map` (new map, `n` wins on a key clash) and `m.update(n)` (write `n` into `m` in place,
→ nil); `m[k]` reads (errors on a missing key), `m[k] = v` inserts/updates. Iterate with `for k in m`
/ `for k, v in m`.

Sets: `{a, b, c}` is a set literal (deduped, insertion-ordered; `{}` is the empty *map*, the empty
set is `Set()`; `Set(list)` builds one from a list). Elements are any `Hashable` type (int/str/bool,
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
fn fetch_all(urls: List[str]):
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
- **`Ref[T]`** (reserved global, no import needed) — the **in-task** mutable box: `Ref(v)` then `r.get() -> T`,
  `r.set(v)`, `r.update(fn(x): ...)`. Backed by `Rc<RefCell>`, so it is a true *shared reference*
  within one task: a closure that closes over a `Ref[T]` and any other holder see each other's writes
  — the answer to "I need a mutable value to close over or pass by reference" without hand-rolling a
  one-field struct. It is **not sendable**: copying a `Ref` across a `spawn`/`submit` would silently
  duplicate the box, so the checker rejects it (`non-sendable value of type Ref[T]`). Cross a task
  boundary with `Shared[T]` instead.
- **`Shared[T]`** (`import std.concurrency`) — the cross-task mutable box, same `s.get()` / `s.set(v)`
  / `s.update(fn(x): ...)` API as `Ref` but synchronized and **sendable**. The mutation ladder is
  `value` (copied) → `Ref[T]` (in-task, unsynchronized) → `Shared[T]` (cross-task, synchronized).
  `Shared`/`RwShared`/`Atomic`/`Executor` require `import std.concurrency` (whole-module licenses all
  four; `import Shared from std.concurrency` per-name) — they are NOT global builtins. They stay
  **reserved names** (no user `struct Shared`/`struct Executor`). `Channel` stays global; `timer` now
  requires `import std.time` (it stays a reserved name too — see below).
  These import-gated native types are **also reachable by the qualified / aliased module-member path**,
  exactly like a `.chz` module type (`geo.Point`) or `regex.Match`: after `import std.concurrency` you
  may write `concurrency.Shared[int]` / `concurrency.Shared(0)`, and `import std.concurrency as c` gives
  `c.Shared[int]` / `c.Shared(0)`. The qualified form works in every position — annotation, constructor
  call, `type S = concurrency.Shared[int]`, `newtype MyS[T] = concurrency.Shared[T]`, and method calls —
  and lowers to the same value as the bare name. (Paths are two-level: `concurrency.Shared`, never
  `std.concurrency.Shared`, like every Chezzi module.) The qualified path still requires the `import`
  (qualified access to a non-imported module is an `unknown module` error), so the gate is unchanged.
  `net.Socket` / `net.Listener` (`import std.net`) and the FFI width types / `ptr` (`import std.ffi`,
  usable as `ffi.int32` incl. inside an `extern` signature) resolve the same qualified way but are
  **type-only** — they have no from-nothing constructor (a value comes from `net.connect`/`net.listen`
  or an FFI call), so `net.Socket(...)` is rejected. `time.timer(ms)` works as a qualified call;
  `time.timer` in **type** position is rejected (it is a function, not a type). The bare-after-import
  spelling remains fully supported; the qualified path is **additive** (the bare-name licensing may be
  deprecated in a later milestone, but is not going away in this change).
- **`Atomic[T]`** — the cross-task **atomic** box (sibling of `Shared`, sendable handle, value-first
  `Atomic(v)`; an optional `Atomic[T](v)` turbofish pins the element type and is checked against the
  value): `a.load()`, `a.store(v)`, `a.exchange(v) -> T` (returns old), `a.cas(expected, new) ->
  bool`, and on numeric `T` `a.add(x) -> T` / `a.sub(x) -> T` (return the new value; checked-overflow
  like `+`/`-`). Each op is atomic across threads. Use it for counters/flags/CAS-loops; `Shared` for
  arbitrary-transform updates.
- **`timer(ms) -> Channel[bool]`** (`import std.time`) — a one-shot timeout channel: `timer(500).recv()`
  blocks ~500ms then yields `true` (level-triggered — ready on any recv at/after the deadline). The
  composable timeout primitive; it races against real channels inside a `wait:` — there is **no separate
  `recv_timeout`** (a `wait` over a channel and a `timer` subsumes it). `timer` requires `import std.time`
  (whole-module, or `import timer from std.time`) — it is NOT a global builtin, but it stays a **reserved
  name** (no user `struct timer`/`fn timer`).
- **`Socket` / `Listener`** (`import std.net`) — the TCP handle TYPE names (from `connect`/`listen`).
  Like `Shared`/`Executor`/`timer`, a bare annotation requires `import std.net` (whole-module, or
  `import Socket from std.net`) — they are NOT global builtins, but stay **reserved names** (no user
  `struct Socket`/`struct Listener`). The builtin SCALAR (`int`/`float`/`str`/…), CONTAINER
  (`List`/`Set`/`Map`/`Channel`/`range`), and FFI (`ptr`/`owned_str`) type names are likewise reserved
  at declaration (a `struct int` / `struct List` is rejected `type 'X' is reserved (builtin)`). The 15
  prebuilt PROTOCOL names (`Comparable`/`Stringable`/`Hashable`/`Error`/`Add`/`Sub`/`Mul`/`Div`/`Mod`/
  `Neg`/`Arithmetic`/`Iterable`/`Index`/`IndexSet`/`Slice`) are reserved the same way — usable as a
  bound (`[T: Comparable]`) but not as a `struct`/`enum`/`newtype`/`type` decl name.
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
  types (scalars/str/containers+structs of sendable/`Channel`/`Atomic`/`Shared`/`RwShared`/a `std.cancel` `Token`)
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

**Types are module-scoped** (like functions — exported by default, no `pub`; visible elsewhere only
via import, under the same bound last-segment name):

```chezzi
import core.geo
p: geo.Point = geo.Point(1, 2)   # qualified construct + annotate
c := geo.Color.Red               # qualified enum variant
xs: List[geo.Point] = []         # qualified type inside a generic
print(p)                         # Point(x=1, y=2)  — bare name, no `::`

import Point from core.geo       # named import → bare use
q := Point(3, 4)
import Point as Pt from core.geo # rename (user types only; FFI widths can't be renamed)

c0 := geo.Counter.zero()         # qualified type as a STATIC-method receiver
col := geo.Color.first()         # enum static via the qualified type (variant still wins on a clash)
```

A **qualified type** may also be the receiver of a **static (associated) method** —
`module.Type.static_method(args)` — symmetric with qualified construction (`module.Type(args)`) and the
bare `Type.static_method()` form after a named import. As on a bare type, an enum **variant** name
always wins over a static-method name on `module.Enum.x`.

**Type/value paths are TWO-LEVEL** (Go-style): `module.Symbol`, where `module` is the imported
last-segment name (or an alias). Multi-level paths like `std.concurrency.Shared` are **not supported**
(even though `import` paths *are* multi-level) — write `concurrency.Shared`, or alias with
`import std.concurrency as c` then `c.Shared`. The deeper form gets a targeted two-level-path error.

A bare type whose module was imported whole (`import geo`) but not named-imported is a **check-time
error** (`unknown type 'Point'; import it from geo`). Two modules may declare the same type name —
no collision; each is importable. Under the hood every user type has ONE canonical, always-qualified
**identity key** (`<module-key>::Name`) used as the runtime tag + every layout lookup, while its **bare
name** is what prints — so output stays byte-identical regardless of module and two colliding `Point`s
both render `Point(...)` (the module is never shown). Reserved/native types (`Result`/`Option`/`Some`/
`Ok`, `Ref`, `Iterator`, the std type surface on `import std.*`, FFI widths) stay global/bare always. An
imported `type` alias is transparent (its body resolves in the defining module's scope, carrying any
FFI-width license).

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

**Marshalling (v1 — scalars + fixed-width ints + opaque `ptr`):** `int` ↔ C `long` (for a fixed-width
C `int32_t`/`uint32_t`/… use the dedicated `int8`..`uint64` names below — they bind the exact ABI width),
`float` ↔ C `double`,
`bool` ↔ C `_Bool` (1 byte; a C function using the pre-C99 int-returning predicate idiom — e.g.
`isdigit`, which returns an *arbitrary* nonzero `int` for true — must be bound `-> int` and tested
`!= 0`, **not** `bool`), `str` → null-terminated `const char*` (a `char*` return is copied into a Chezzi
`str`; **return-only** `owned_str` also frees it, `str?` makes a `NULL` return `None` — see below), and
`ptr` ↔ C `void*` (an **opaque handle** — see below). One-way `int`→`float` widening applies at a
C `double` param too (`cos(2)` widens the int to `2.0` before marshalling — the FFI host promotes it;
a non-numeric arg like a `str`/`bool` is still rejected). A no-return signature (`fn srand(seed: int)`) — or an explicit
`-> nil` — maps to C `void`; `nil` is a **return-only** type (it is rejected as a parameter). A
**flat-scalar `struct`** marshals **by value** as a C struct (see below). The checker rejects any
other non-marshallable param/return (list/map/set/tuple/enum/generic struct/struct-with-non-scalar-
field/…) with a *not C-marshallable* error. Calls run inline (a slow C call pins its worker under
`--parallel`) and produce identical output on all three engines (VM / `--interp` / `--parallel`).

**Structs by value.** Name a Chezzi `struct` as an extern param and/or return type to pass/return a C
struct **by value** (not by pointer). The struct's **field order + types define the C layout** — libffi
computes size/alignment/offsets from the platform ABI (small-struct-in-registers vs by-hidden-pointer
is handled for you), so it works as both a by-value param and a by-value return:

```chezzi
# div_t div(int numer, int denom) — returns a small POD struct BY VALUE.
struct DivT:
    quot: int32
    rem: int32

extern "libc.so.6":
    fn div(numer: int32, denom: int32) -> DivT

r := div(17, 5)
print(r.quot)   # 3
print(r.rem)    # 2
```

**v1 limit — flat scalar fields only.** Every field must itself be an already-marshallable **scalar**
(`int`/`float`/`bool`/`ptr`/the `int8`..`uint64` widths). A struct with a **`str` field** or a **nested
struct** field is rejected with an error naming the struct *and* the offending field; **generic
structs** (`Pair[int]`) have no fixed C layout and are rejected. (A transparent `type P = Point` alias
to a flat struct works exactly like the bare struct.) The struct may be declared **before or after** the
`extern` block. A struct (or a width alias like `type Len = int32`) declared in another **module** may be
named at the extern boundary either by **named import** (`import DivT from core.cdefs`, then bare `DivT`)
or by the **module-qualified spelling** (`import core.cdefs`, then `cdefs.DivT` directly in the `extern`
fn) — both lower to the same C type. A **`bool` field** marshals as a C `_Bool` (1 byte) — it matches a C struct field
declared `_Bool` (or `char`), **not** a 4-byte `int`; for an *int*-width boolean field declared `int` in
C, use `int8`/`uint8` (or `int32`) and test `!= 0`. Nested structs-by-value and string fields are
deferred to a later version.

**Sync scalar callbacks (`fn(...)` extern params).** Pass a Chezzi closure to C as a C function
pointer C calls *back* synchronously, during the extern call. Declare the param with the **existing**
function-type spelling — `fn(scalars) -> scalar` (no new syntax) — restricted to C scalars
(`int`/`float`/`bool`/`ptr`/`int8`..`uint64`; no `str`, struct, or nested callback):

```chezzi
extern "libapply.so":
    # f is a sync scalar callback: C calls it back during apply(), on this thread.
    fn apply(x: int, f: fn(int) -> int) -> int

print(apply(10, fn(n: int) -> int: n * n))   # C runs f(10) -> 100, then returns 100 + 1 = 101
```

If the Chezzi callback faults (or panics), the error is **re-raised** as the extern call's own error
(recoverable via `recover:`) — stronger than CPython `ctypes`, which swallows it to stderr and returns
`0`. Both engines run the callback identically (two-engine parity), and it fires on the calling thread
under `--parallel` (no cross-thread hand-off).

**Deferred FFI features (with design notes + the callback feasibility ladder in
[`docs/ffi-and-packaging.md §1b`](ffi-and-packaging.md)):** the **rest of callbacks** (#4 — *stored* /
*cross-thread* callbacks a C library keeps and calls later or from its own thread; harder than in
Python because `--parallel` has no GIL to serialize the re-entry, plus they need a GC-rooting registry)
and **pointer-deref builtins** (to deref a `void*` callback arg → unlocks `qsort`/`bsearch`); and
**varargs** (#5 — rare; `printf`-family + a few syscalls, most of which you bind with a concrete
*fixed-arity* signature today, caveat: float varargs / non-x86-64 aren't ABI-portable that way). `bool`
now **is** C `_Bool` (1 byte) — no separate `bool8` type; the classic int-returning predicates
(`isdigit`, …) bind `-> int` and test `!= 0`.

**Opaque handles (`ptr`).** A C library built around a handle (`FILE*`, `sqlite3*`, a
`create`/`use`/`destroy` context) returns a `void*` you hold and pass back. Declare it as `ptr` — an
opaque type imported from `std.ffi` (using it, **including in an `extern` block**, requires
`import std.ffi` or `import ptr from std.ffi`, just like the fixed-width integer types). A `ptr` is **untyped**
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

`std.ffi` exports both **value members** — `null()` / `is_null(p)` (above) — and **eight fixed-width
integer TYPE names** — `int8`/`int16`/`int32`/`int64`/`uint8`/`uint16`/`uint32`/`uint64`. The TYPE names
are Chezzi's first **type imports**: like the value members they are brought in per-name with
`import int32, uint32 from std.ffi`, and a module that does not import a width name cannot use it (see
*Fixed-width integers* below). The opaque `ptr` type is imported the same way — whole-module
`import std.ffi` or per-name `import ptr from std.ffi` — and likewise cannot be used (in an annotation
OR an `extern` signature) without that import.

**`str` returns — owned + nullable (return-only opt-ins).** A plain `str` return is **borrowed**: the
`char*` is copied into a Chezzi string and never `free`d (a `malloc`'d return would **leak**), and a
`NULL` is a recoverable **fault** (it would break the static non-null `str` guarantee). Two return-only
forms (no `import`, no grammar change — both are recognized only inside an `extern` return slot, like
`ptr`) opt into the other behaviours:

- **`owned_str`** — the C function transfers ownership of a `malloc`'d `char*` (e.g. `strdup`). Chezzi
  copies it into a `str` **and then frees** the buffer with libc `free`, so it does **not** leak. To
  your program it is a plain `str`. A `NULL` still faults (use `owned_str?` for nullable).
- **`str?`** (sugar for `Option[str]`) — the C function legitimately returns `NULL` (e.g. `getenv` of an
  unset variable). `NULL` becomes `None`, a non-null pointer becomes `Some(str)` (still borrowed, not
  freed). This is the only way to make a `NULL` `char*` return non-fatal.
- **`owned_str?`** composes both: nullable **and** freed (`NULL` → `None` and frees nothing).

Both are **return-only** — an `owned_str`/`str?` *parameter* is rejected as *not C-marshallable*.
`owned_str` is additionally legal **only inside an `extern` signature**: a bare non-extern annotation
(`fn f(x: owned_str)`) is rejected (*'owned_str' is a return-only extern marshalling type and cannot be
used as a general type annotation*) rather than silently collapsing to `str`.

```chezzi
extern "libc.so.6":
    fn strdup(s: str) -> owned_str   # owned malloc'd char* — copied AND freed (no leak)
    fn getenv(name: str) -> str?     # nullable — NULL → None instead of a fault

print(strdup("hi"))                  # hi   (the C buffer is freed after the copy)
match getenv("HOME"):
    Some(v): print(v)
    None: print("unset")
```

`free` is resolved once (via `dlsym("free")` on the loaded library, which finds libc `free`); a custom
user-named deallocator is **not** supported. **Caveat (C trust boundary):** `owned_str` asserts the
returned buffer is genuinely `malloc`'d — declaring a **static / string-literal** return `owned_str`
frees memory you don't own and corrupts the heap, exactly like a non-NUL-terminated return over-reads.

**Fixed-width integers (`int8`..`uint64`) — bidirectional, imported from `std.ffi`.** Bare `int` marshals
as C `long`; to bind a C function taking or returning a **fixed-width** integer, declare it with one of
eight marshalling type names. These are **not global builtins** (like `ptr`, and unlike `owned_str`, which
is neither global nor importable — legal only inside an `extern` signature): a module that
names a width type must **import it per-name from `std.ffi`** — Chezzi's first **type imports**, with the
same `import <name>, … from std.ffi` form as the `null`/`is_null` value members:

```chezzi
import int32, uint32 from std.ffi   # bring the width TYPE names into this module
```

A module that uses a width name without importing it gets *unknown type 'int32' (import it from std.ffi:
`import int32 from std.ffi`)*. Importing a non-existent width name errors like any bad import (*module
'std.ffi' has no member 'int99'*). A width type **cannot be renamed on import** — `import int32 as W from
std.ffi` is rejected (the backend marshaller keys off the literal name `int32`), as is a wrong-width
trap (`import int8 as int32`). A redundant **identical** self-rename (`import int32 as int32`) is harmless
and accepted — it just imports `int32`. You also can't redefine a
width name as a user alias (`type int32 = …` is reserved). The import is per-module: a struct's int32 field
declared (and resolved) in module A is usable from module B with **no** import in B, but a bare `int32`
*written in B's own source* needs B's own import. **A licensed transparent alias is the opt-in:** a
`type Len = int32` resolves wherever the alias is used **only if the alias's defining module imported
`int32`** — that import licenses the alias once, and any later use (including after another module is
checked) resolves without re-importing. A `type Len = int32` whose defining module never imported `int32`
does **not** launder the bare width name — it is still *unknown type 'int32'* (a bare `int32` always needs
the import; only a *licensed* alias indirection bypasses the per-site requirement). Composite alias bodies
that *embed* widths (`type Pair = (int32, int32)`, `type Buf = List[uint8]`) follow the same rule, and the
licence is precise: the alias is licensed only if its defining module imported **every** width it embeds —
a `type Mixed = (int32, int64)` that imported only `int32` is **not** licensed, so `int64` can't ride in on
`int32`'s opt-in. To your program each
width name is a plain **`int`** — the width/signedness is a
runtime-only marshalling distinction. Unlike `owned_str`, these are **bidirectional** (valid as both param
and return):

| name | C type | libffi type | as a parameter | as a return |
|------|--------|-------------|----------------|-------------|
| `int8`   | `int8_t`   | `sint8`  | truncate i64 → i8  (wrap) | sign-extend i8 → i64  |
| `int16`  | `int16_t`  | `sint16` | truncate i64 → i16 (wrap) | sign-extend i16 → i64 |
| `int32`  | `int32_t`  | `sint32` | truncate i64 → i32 (wrap) | sign-extend i32 → i64 |
| `int64`  | `int64_t`  | `sint64` | i64 (no change)           | i64 (no change)       |
| `uint8`  | `uint8_t`  | `uint8`  | truncate i64 → u8  (wrap) | zero-extend u8 → i64  |
| `uint16` | `uint16_t` | `uint16` | truncate i64 → u16 (wrap) | zero-extend u16 → i64 |
| `uint32` | `uint32_t` | `uint32` | truncate i64 → u32 (wrap) | zero-extend u32 → i64 |
| `uint64` | `uint64_t` | `uint64` | truncate i64 → u64 (wrap) | reinterpret u64 → i64 |

A **param truncates** the Chezzi i64 to the C width with **C-cast (wrapping) semantics — never an
overflow trap**: `255` passed to `int8` becomes `-1`, `300` becomes `44`. A **return sign-extends**
(signed) or **zero-extends** (unsigned) the C value back to i64: `int32` returning `-1` is `-1`,
`uint32` returning `0xFFFFFFFF` is `4294967295` (stays positive). A `type Len = int32` alias used in an
`extern` sig behaves identically to bare `int32` — but the alias only resolves if its target `int32` is
imported in the **same** module as the alias declaration.

```chezzi
import int32, uint32, int8 from std.ffi
extern "libc.so.6":
    fn atoi(s: str) -> int32      # parse to a C int; -1 sign-extends back to i64 -1
    fn htonl(x: uint32) -> uint32 # unsigned in+out; a high-bit result stays positive
    fn abs(x: int8) -> int8       # signed round-trip; an out-of-range param wraps (C cast)

print(atoi("-1"))   # -1
print(htonl(128))   # 2147483648   (0x80000000, zero-extended → positive)
print(abs(255))     # 1            (255 → int8 → -1, then abs)
```

**Limits:** `uint64` values above `i64::MAX` are not representable in Chezzi's i64 `int` and wrap
negative (the other seven widths fit i64 losslessly). No C-spelling aliases (`c_int`/`c_short`/…) yet —
their width is platform-dependent (LP64 vs LLP64); deferred to a future task. See `examples/ffi_int.chz`.

An `extern "lib":` block is a **top-level declaration only** — it is bound at module init, so nesting
it inside `if`/`for`/`fn` is a parse error. An extern fn also may **not** be named after a builtin
(`range`/`int`/`float`/`str`/`ord`/`chr`/`set`/`panic`), `print`, a constructor
(`Channel`/`Shared`/`RwShared`/`Atomic`/`timer`/`Executor`), or any of your `struct`/enum-variant names — those
resolve to a special op before a plain call, so the extern would be silently shadowed; the checker
rejects the collision (*'…' is a builtin/reserved name*).

**Known v1 limits (see `docs/spec.md` for detail):**
- **C `int` width:** bare Chezzi `int` (i64) marshals as C **`long`** — 64-bit on supported **LP64 unix**
  targets. For a **fixed-width** C integer (`int32_t`/`uint32_t`/…) use the dedicated `int8`..`uint64`
  marshalling names (bidirectional, truncate-on-param / sign-or-zero-extend-on-return — see above).
  Non-unix (LLP64, where C `long` is 32-bit) is **unsupported**: the checker rejects `extern` there.
- **`char*` ownership:** a plain `str` return is **borrowed** (copied, never `free`d — a `malloc`'d
  return leaks). Declare it **`owned_str`** to transfer ownership: Chezzi copies then frees it with libc
  `free` (no leak). Only libc `free` is supported — a custom/user-named deallocator is **deferred**.
- **No `--parallel` serialization:** extern calls are **not** serialized; calling a **non-reentrant** C
  function (`strtok`, `gmtime`/`localtime`, `setlocale`, static-buffer APIs) from multiple workers
  **races at the C level**. Use thread-safe/reentrant C only under `--parallel`.

- **Untyped + un-freed handles:** a `ptr` is one opaque type for every C handle (no `FILE*` vs
  `sqlite3*` checking — passing the wrong handle is C-level UB, the author's assertion) and is **never
  auto-freed** (call the library's own destroy; forgetting **leaks**). The `ptr` is opaque *as a value*
  (cannot be forged from an int), but its **memory is readable/writable** via `std.ffi`
  `load_*`/`store_*` (every C scalar width + `load_str`, each with an `_at(p, off)` byte-offset form) —
  for struct fields, return buffers, and C output-params. You can also **make your own C-laid-out
  buffer** via `std.ffi` `alloc(nbytes)`/`alloc_zeroed(nbytes)` (libc `malloc`/`calloc` → a raw `ptr`)
  and release it with `free(p)` (**manually freed** — `defer ffi.free(p)`; never auto-freed). Unsafe
  like `ctypes`: a bad pointer segfaults; double-free / use-after-free / out-of-bounds is UB; only the
  NULL base pointer is guarded (recoverable error). See `stdlib.md §std.ffi`.

**Deferred (v1 limits):** *stored / cross-thread* callbacks (sync scalar callbacks **shipped** — see
above), varargs, a **GC-tracked auto-freed owned-buffer type** + bulk-copy helpers + `realloc` (the
manual `ffi.alloc`/`alloc_zeroed`/`free` layer **shipped** — see above), the rich Rust `Box<dyn Any>`
userdata handle (for compiled-in Rust libraries), a **custom user-named deallocator** (only libc `free`
backs `owned_str`), and — within structs-by-value — **nested structs** and **`str`/`owned_str` fields**.
(Opaque C `void*` handles — `ptr` — **shipped**, with `load_*`/`store_*` memory deref + the
`ffi.alloc`/`alloc_zeroed`/`free` C-buffer layer, so `qsort`/`bsearch` of a Chezzi list now fully works;
nullable `str?`
returns and `char*` ownership transfer via `owned_str` — **shipped**; **flat-scalar structs by value** —
**shipped**; **sync scalar callbacks** (`fn(scalars) -> scalar` extern params) — **shipped**, see above.)

## 12c. `native fn` / `native ctor` — universe-builtin signatures in Chezzi (prelude/std-only)

The **internal** analog of `extern "lib":`. Where `extern` binds a C function, a `native` decl declares
the **signature** of a built-in whose body is implemented natively (in the engine), name-keyed. This is
how the **universe builtins** are declared: their signatures live in **`std/prelude.chz`** (always linked
into every program, like `std/ref.chz`), not hidden in the compiler.

```chezzi
native fn ord(c: str) -> int      # first-class universe FUNCTION
native fn panic(msg: str)         # no `-> ret` → native-controlled/never
native ctor int(x) -> int         # non-first-class scalar CONSTRUCTOR; `x` unannotated → dynamic
native ctor bytearray(x) -> bytearray
```

- **`native fn`** declares a **first-class** function intrinsic (bindable/passable — `f := ord`).
  **`native ctor`** declares a **non-first-class** constructor (like `int`/`str`; a value-position use
  `f := int` is a type error, uniform with `f := List`).
- **Bodyless**, NEWLINE-terminated (like an `extern` sig). The engine binds the implementation by name;
  a `native` decl compiles to **no** code and is never a callable user function.
- **Dynamic-param convention** (scoped to `native` decls only — Chezzi user code stays statically typed,
  there is **no** user-facing `any`/`never`): an **unannotated** param means "accepts anything"; a decl
  with **no `-> ret`** means native-controlled/never (how `panic`'s divergent return is spelled).
- **Prelude/std-only:** a `native fn`/`native ctor` in an ordinary user `.chz` is a **checker error**
  (*native fn/ctor declarations are only allowed in standard-library modules*) — a footgun guard, so a
  user can't bind a name to a nonexistent intrinsic. Top-level only (nesting is a parse error).

The eight universe builtins `ord`, `chr`, `panic` (fns) and `int`, `float`, `str`, `bytes`, `bytearray`
(ctors) are declared this way in `std/prelude.chz`. `print` stays engine-synthetic (its `sep=`/`end=`
variadic isn't expressible), and `range` + the `List`/`Map`/`Set` container ctors remain built-in for now.

## 12d. `native struct` — native-type signatures in Chezzi (prelude/std-only)

The **type-level** analog of `native fn`/`native ctor` (phase 4a). A body-less `native struct Name:`
declares a native (Rust-backed) type's **checker signature** — its field layout — in Chezzi; the runtime
layout + method dispatch stay **native** (name-keyed). Like `native fn`, it is **prelude/std-only** and
**top-level only**.

```chezzi
native struct Match:      # regex.Match's SIGNATURE (fields-only)
    text: str
    start: int
    end: int
    groups: List[str]
```

- **Fields-only** (phase 4a): a `fn`/`test` method sig or a field `= default` inside the body is a
  parse error. Bodyless native **method** sigs are a phase-4b follow-up.
- **Prelude/std-only:** a `native struct` in an ordinary user `.chz` is a **checker error** (*native
  struct declarations are only allowed in standard-library modules*); nesting it is a parse error.
- **Companion-stub convention** (file-less native modules): `std.regex` is a *virtual* module — the
  resolver injects an empty AST, there is no `std/regex.chz`. Its `Match` type's signature is declared
  in a **parse-only companion stub `std/regex.stub.chz`** (embedded via `include_str!`), which is
  **never** added to the runnable module graph — the checker parses it solely to harvest its `native
  struct` decls into `std.regex`'s module signature. The type stays **import-gated** exactly as before
  (`import std.regex` / `import Match from std.regex` licenses the bare `Match`; `regex.Match(...)`
  qualified); the migration is a **zero observable behavior change** (identical runtime + bytecode,
  three-engine byte-identical).

## 13. Standard library (v1)

> **The complete library reference — every global builtin, type method, runtime type, and `std.*`
> module with signatures — lives in [`stdlib.md`](stdlib.md).** This section is a short orientation.

Always available (no import): `print`, `range`, `int()`/`float()`/`str()`,
`ord(s)→int` (first codepoint), `chr(n)→str` (codepoint → 1-char string), `Set()`/`Set(list)`,
`panic(msg)` (raise a recoverable fault; see `recover:`), plus methods on the core types
(`list`/`map`/`set`/`str`/`bytes`/`bytearray`).

Modules are `import std.X` then `X.func(...)`. Importable:
`std.io`, `std.math`, `std.str`, `std.cmp`, `std.os`, `std.json`, `std.process`, `std.fs`,
`std.time`, `std.regex`, `std.request`, `std.net`, `std.ffi`, `std.iter`, `std.ref`, `std.cancel`.

A few cross-cutting notes (full detail in `stdlib.md`):

- `min`/`max`/`clamp` live in **`std.cmp`** as generic `[T: Comparable]` functions (int/float/str and
  any struct with a `compare` method); `list.sort()` is likewise Comparable.
- **`std.json`** parses/stringifies a dynamic `Json` enum, and `json.decode[T](s) -> Result[T]`
  deserializes straight into a known shape. A JSON *literal in source* needs a raw string
  (`r"""{"k": 1}"""`) or doubled braces — a bare `{…}` in a normal string is interpolation.
- **`std.os.exit(code)`** is a hard, uncatchable exit (does not run `defer`s). **`std.process.cmd`**
  runs a shell line — never interpolate untrusted input.
- `Match` (`std.regex`), `Response` (`std.request`), and `ProcResult` (`std.process`) are **module-owned**
  struct types, not reserved program-global names. Reading their fields off a returned value
  (`regex.find(…).text`, `request.get(…).status`, `process.run(…).code`) needs **no import**; naming or
  constructing the type (`m: Match` / `Match(…)`) requires importing the owning module (`import std.regex`
  exposes `Match` bare and as `regex.Match(…)`; or `import Match from std.regex`). The names are free for
  user types **only when the owning module is not imported** — a user `struct Response` without `import
  std.request` is their own type. But importing the type **and** declaring a same-named `struct` in the
  same module is a collision, rejected at check (`type 'Response' is reserved (builtin)`) — never accept-
  then-trap: the user layout would shadow the native shape and fault at runtime on a field mismatch. (This applies
  to `Match`/`Response`/`ProcResult` and every import-gated std struct.) A merely-similar name (`struct
  ResponseBox`) stays legal. **`Ref` (`std.ref`) is different**: it backs the `ref` keyword, so it is a
  full **reserved program-global** (like `Result`/`Option`/`Iterator`) — always present, usable bare
  with **no import**, and a user `struct Ref` is *always* rejected as reserved. `import std.ref` is a
  harmless no-op kept for compatibility.

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
