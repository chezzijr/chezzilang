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
-9223372036854775808   # int — the i64::MIN boundary literal (the magnitude 2^63 is legal ONLY when
              #       immediately negated; a bare `9223372036854775808` is "number too large")
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

**Tuple elements are read positionally with `.0`, `.1`, … (Rust's spelling), never `t[0]`.** The index
must be a literal — it is part of the type, not a runtime value — and tuples are **immutable**, so
`.N` is read-only:

```chezzi
t := ("a", 1)
print(t.0)        # a
print(t.1)        # 1
# t[0]  → type error: cannot index into (str, int)   (a tuple is not Indexable)
# t.0 = "b"  → type error: cannot assign to field '0' of (str, int)
a, b := t         # destructuring is the other way in — see §3
```

Destructuring (`a, b := t`, or `for a, b in pairs`) is usually what you want; `.N` matters where you
can't bind names — inside a single-expression closure, for instance `xs.sort_by_key(fn(p) -> int: p.1)`.

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
and `&= |= ^= <<= >>=` (int-only, mirroring the bitwise operators). Because `x OP= v` is `x = x OP
v`, it also accepts a **struct/enum/newtype whose operator overload** (an `add`/`sub`/`mul`/… method,
or a numeric newtype's auto-flow) makes `x = x OP v` type-check — e.g. `a += V(10)` for a `struct V`
with `add`. It is rejected exactly when `x OP v` is itself a type error (a type with no matching
overload, or a mismatched operand). No implicit widening — `int /=
float` is a type error (the result would be a float, which can't flow back into an `int` slot).
The index expression is evaluated **exactly once** (`t[f()] += 1` calls `f` once, as in Python).

Because `x OP= v` is `x = x OP v`, a compound assign to a **user index target** (`obj[k] += v`, §7b)
**reads the LHS through `index`** — so its type is `index`'s **return**, not `set_index`'s `val`, and
the two must agree (§7b's coherence rule): a compound on an incoherent pair — e.g. `index -> str` with
`set_index(_, val: int)` — is a **check-time error** (`type S does not satisfy IndexSet (…)`), never a
runtime fault. A plain `obj[k] = v` never reads, so it only type-checks against `set_index`'s `val`.
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

### `const T` — immutable bindings

`const T` is a **binding modifier** (in the type slot of a single-name typed let) that freezes the
**name**: the checker rejects any later reassignment. It is an **immutable binding**, *not* a compile-time
constant (Rust `const`/Go `const`) — the RHS is any runtime expression, evaluated once. Think JS
`const` / Java `final`.

```chezzi
PI: const float = 3.14159
ANSWER: const int = 6 * 7     # runtime RHS is fine — const ≠ constexpr
PI = 3.0                      # ✗ type error: cannot reassign const binding 'PI'
ANSWER += 1                   # ✗ every compound form is caught too
```

- **Shallow.** `const` freezes the binding, not the object it points at. A `const` container's own
  contents stay mutable — only the name can't be rebound:

  ```chezzi
  xs: const List[int] = [1, 2]
  xs.push(3)                    # ✓ mutating THROUGH the const is fine
  xs[0] = 9                     # ✓ (index/field assignment is object mutation, not a rebind)
  xs = [4]                      # ✗ rebinding the NAME is the error
  ```
- **Where it's allowed.** Locals + module globals **only**, and a **single-name typed** let. `const`
  is a **parse error** on a parameter and on a `:=`/destructuring binding. An explicit type is required
  (`const` sits in the type slot).
- **No laundering via re-declaration.** A live const cannot be re-declared in the **same scope** —
  `PI := 9.0` or a second `PI: float = 9.0` after `PI: const float = 3.14` is a **type error**, so
  the guarantee can't be dropped by swapping `=` for `:=`. A genuine **inner-scope** shadow (a fresh
  local of the same name in a nested block/fn) is still fine — it leaves the outer const untouched.
- **Across modules.** A `const` module global (and a native constant like `math.pi`/`e`/`inf`/`nan`)
  carries its const-ness to importers: `import PI from m; PI = x` and the qualified `m.PI = x` both
  report *"it is declared const in module 'm'"* rather than the generic snapshot/field message. (Note
  that *any* imported global is already read-only — a from-imported value is a snapshot copy — so the
  const marking sharpens the message, it doesn't add the restriction.)

#### Re-declaring an ordinary binding — rebind anywhere, retype only in a fn

Re-declaring a name with `:=` (or a second typed let) is legal, but what it *means* differs by scope,
and the asymmetry is deliberate:

```chezzi
x := 1
x := 2          # ✓ same type — Python-style late binding, the SAME storage slot
x := "9"        # ✗ type error: module-level binding 'x' re-declared with a different type

fn f():
    y := 1
    y := "9"    # ✓ a fn-local re-declare is a FRESH binding (Rust-style shadowing)
```

- **A module global is one storage slot, and its type is frozen at the first declaration.** Both `x`s
  above are the same slot, so code on *either* side of the re-declaration is typed against the old
  type: a closure written before it (`f := fn() -> int: x`) would hand a `str` out of a fn declared
  `-> int`, and a `fn` that *writes* the slot (`fn setx(): x = 42`) would put an `int` into what is now
  a `str`. Rebinding the value is fine; **changing the type is a type error**.
- **A fn-local (or block-local) re-declare is a genuinely fresh binding**, so it may change type and a
  closure made earlier keeps the *old* one — the same as Rust's `let` shadowing. This includes a
  binding inside a top-level `if:`/`for:`/`while:` body: those are inner scopes, not the module scope.
- **A refinement is not a retype.** The carve-out is one-sided: the new type is allowed only when it
  *fills in* what the old one left open, so `x := []` then `x := [1]`, `y := {}` then `y := {"a": 1}`,
  and `z := None` then `z := Some(1)` all stay legal. Going the other way is a retype and is rejected —
  `x := []` then `x := 42`, and `x := 1` then `x := None`, both fire (the second would hand a `None` out
  of a closure declared `-> int`).
- **Narrowing counts as a change too** — `v: Any = 1` then `v := "s"`, or `s: Shape = Circle(1)` then
  `s := Circle(2)`, are rejected even though nothing *reads* a lie. The slot's declared type is what is
  frozen, and an earlier writer typed against `Any`/`Shape` can still store a non-`str`/non-`Circle`.
- **Escapes:** **re-annotate** at the declared type (`s: Shape = Circle(2)` and `v: Any = "s"` are both
  accepted — the annotation restates the frozen type instead of changing it), **rename**, or move the
  pair into a `fn`.
- **Imports are hoisted, so the rule compares source position** — but only while the previous binding
  is still the import's. `import COUNT from lib` followed by `COUNT := "s"` is rejected like any other
  retype; a `let` that a *later* `import` happens to collide with (`x := 1` … `import COUNT as x from
  lib`) is **not**, because the import binds first at runtime, so nothing before the `let` can read it.
  Once a `let` has taken the name over, a further re-declaration is judged against *that* let's type, so
  a later `import` of the same name does not license it.
- **The from-import hand-back (`import COUNT from lib` then `COUNT := COUNT + 1`) keeps working, at the
  same type only.** Handing the name back at a *different* type is a retype of the same slot; there is
  no annotation escape (`COUNT: int = 0` declares the same `int`), so **rename**.
- **A destructuring `let` is covered too, per name.** `x := 1` then `x, y := ("s", 2)` is rejected —
  the destructure writes the same one global slot, so it was the same lie (measured before the fix:
  check-clean, then a `str` out of a closure declared `-> int`). A name that is **fresh** in the tuple
  is a first declaration and may be any type (`x := 1` / `x, y := (2, "s")` is fine), and — like the
  single-name let — a fn-local or block-scope destructure is a fresh shadow and may retype. A
  destructure also cannot **un-const** a prior `X: const int = 1`, even at the same type.
- **A top-level `fn` shares the slot too, and re-declaring one is judged on its readers.** `fn helper()`
  and a later `helper := 3` are the same storage slot, and the `fn` is defined into it *before any
  statement runs* — so **where the `fn` sits does not matter**: `f := fn() -> int: helper()` / `helper
  := 3` / `fn helper() -> int:` is rejected exactly like the same three lines in the other order. What
  does matter is whether anything **above** the re-declaration was already typed against the fn: if so
  it is a type error, because those call sites keep the old signature while the slot holds the new
  value. If every reader comes *after*, the re-declaration is just a shadow and stays legal — `fn f(a:
  int, b: int = 2)` then `f := fn(a: int) -> int: a * 100` then `f(1)` is fine (a plain value call, no
  default spliced in), which is what CPython does too. Same for a `from`-imported fn.
- **Optional arity counts as part of the signature here.** A re-declaration may not be *stricter* than
  what the previous binding promised: `fn helper(a: int = 77)` (callable as `helper()`) re-bound to
  `fn(a: int) -> int` is rejected, because a call compiled to omit the argument would hand the deleted
  function's default to the new one. The other direction — the new binding accepting *more* omissions —
  keeps every existing call valid and is allowed.
- **Reading an imported name *above* its own `import` is an error.** Imports are hoisted, so the name
  is bound from line 1 whatever line the `import` sits on — which let a closure written above it be
  typed against the import and then be refilled by a later `let` (`f := fn() -> str: x` / `x := 1` /
  `import COUNT as x from lib.st` checked clean and printed `1` out of a `fn() -> str`). So the read
  itself is rejected: *"'x' is used before its `import` on line 3"* — move the `import` above the line
  that uses it. This also rejects `print(COUNT)` above `import COUNT from lib`, which the hoist makes
  technically work, because both ancestors refuse it (CPython raises `NameError`, Go will not even
  parse an `import` placed after a declaration) and it reads as a use-before-definition. A
  from-imported **fn** is the same rule, in both spellings (`g := h` and `h()`), so the two do not
  disagree; a **same-module** top-level `fn` is untouched and stays position-independent. It is a rule
  about **value/callable** reads only: a *type* name used above its import is fine, and — unlike Go —
  an `import` may still sit anywhere at top level, as long as nothing above it reads *that import's
  binding*. A read that **shadows** the name — a parameter, a fn-local `:=`, a loop variable, a
  block-scope local — resolves to its own binding, not the import's, so it is legal wherever it sits:
  `fn circumference(pi: float, r: float)` above `import pi from std.math` is fine.
- **A deferred read counts as a read** — a top-level `fn` body that reads an imported name above the
  `import` is rejected too, and here the ancestors split (both measured): CPython *accepts* it, since
  the body runs after the import; Go still refuses, since it takes no late `import` at all. Chezzi
  follows Go, because the hoist makes the sound and the unsound case indistinguishable **at the read
  site**: the same read is fine until some later `let` refills the slot, and nothing there tells the
  two apart. Move the `import` up; do not expect this to be loosened to "immediate reads only".

### Closure capture — uniformly by reference

Capture is **by reference, always**. A closure (and a `spawn:` / `parallel:` / `defer:` block)
shares the *closest binding* of each captured name: reads see later writes, and a write through the
capture is visible in the defining scope and across sibling closures. There is no by-binding-kind
distinction any more — a plain local and a global both share the live binding.

| binding | captured as | `x := 10; f := fn() -> int: x; x = 20; f()` |
|---|---|---|
| plain local | shared (live) | `20` |
| global | shared (live) | `20` |

A closure captures **only the names its body actually references** (its free variables) — not every
local visible in the enclosing scope. So an unrelated non-sendable sibling in scope (another closure
value, a live generator) that the task never touches does **not** block a `spawn` / `parallel:`
airlock crossing; only names the task really uses are checked for sendability.

Two rules cover everything:

1. **Capture is by reference.** A closure reads a captured name's *current* value, so a write after
   the closure is created is visible: `x := 10; f := fn() -> int: x; x = 20; f()` → `20`. A captured
   **loop variable** rebinds into a **fresh** cell each iteration (matches Go ≥1.22), so
   `for i in [0,1,2]: fns.push(fn() -> int: i)` yields closures that return `0`, `1`, `2` — not three
   `2`s. A variable declared *outside* the loop stays one shared cell.
2. **The task boundary is the one place sharing stops.** Across `spawn` / `parallel:` (a real OS
   thread) a plain captured local is **snapshot-copied** into an independent per-task cell — one cell
   per **binding**, not one per reference, so *sibling closures that cross together still share it
   inside the task* (rule 1 survives the crossing: two closures over one local, sent through a
   `Channel` or handed to a `spawn` as two separate args, read and write the same cell on the far side,
   exactly as in Go). "Together" means **one task**: two *separate* tasks each get their own snapshot,
   but the two halves of ONE task that reaches a cell through *both* a captured local and a module
   global do share it — those are two separate serializations that are made to agree on the binding
   (`gaps.md` W7-4c). Writes in
   the task are **not** visible to the parent. This is the one deliberate divergence from Go
   (`x := 0; parallel: spawn: x = x + 1; print(x)` → `0`, not `1`); it is the memory-safety line.
   For genuine cross-task shared mutation use `Shared[T]` / `RwShared[T]` / `Atomic` / `Channel[T]`,
   which cross the airlock by reference (see [`concurrency.md`](concurrency.md)). The copy is *why*
   forgetting `Shared` is a harmless logic bug (an isolated stale value) rather than a data race —
   Chezzi has no borrow checker to prove a shared mutation is locked, so the safe default is to copy.

**The checker WARNS when that copy silently costs you a value.** A captured binding whose only write
is inside a `spawn:` body, read again after the join, is a **non-fatal warning** on stderr naming the
binding and citing the write's line (exit code unchanged — the semantics above are deliberate, so this
is a warning, not an error). It exists because of the failure mode: a `for r in results:` over the
stale (still-empty) list runs **zero** iterations, so every `assert` inside is skipped and the program
exits `0` — a green test that tested nothing.

```chezzi
results: List[str] = []
parallel:
    spawn:
        results = ["ok", "ok"]     # ← warning cites this line
for r in results:                  # 'results' is read here as its pre-`spawn:` value
    assert r == "ok"               # zero iterations before the warning existed
```

It covers a reassignment, a compound assign, `xs[i] = v`, `p.field = v`, `m[k] = v`, and the in-place
container mutators (`push`/`pop`/`insert`/`remove_at`/`extend`/`sort`/`sort_by`/`sort_by_key`/
`reverse` on a list, `remove`/`update` on a map, `add`/`remove` on a set, `push`/`pop` on a bytearray).
It stays **silent** where the write really does survive: through a `Shared`/`RwShared`/`Atomic`/
`AtomicInt`/`Channel`/`Executor`/`Socket`/`Listener`/`Writer`/`Reader` handle (those cross by handle),
inside a `defer:` block **in the parent** (same frame, same cell, no airlock), when the parent
overwrites the binding before reading it, and when the read happens only inside the task. A `defer:`
block nested *inside* a `spawn:` body is on the far side of the airlock like any other task statement —
its write is lost and it warns like one.

A parent-side write only **supersedes** the lost one — and so silences the warning — when it replaces
the *whole* binding (`xs = [...]`). An in-place mutator (`xs.push(v)`) and a compound assign (`n += 1`)
both **read** the stale copy before writing it, so they warn at the write itself.

Six deliberate ceilings, all of them under-warning rather than over-warning:

1. **Per frame** — the taint does not cross a `fn` boundary in either direction. A module global written
   in a task in one function and read in another is not flagged (it *is* flagged when both happen in
   the same body, or at module top level); and a read inside a **nested `fn` declared in the parent** is
   likewise silent, even of a captured local whose write really was lost. The **closure** spelling of
   the same read *does* warn, because a closure body is an expression evaluated in the parent's own
   frame: `g := fn() -> int: xs.len()` after the join warns and returns 0, while `fn g() -> int: return
   xs.len()` returns 0 silently.
2. **Lexical, not dataflow** — a read placed textually *before* the `spawn:` is not flagged even though
   the write cannot reach it either.
3. **Builtin containers only** — a user struct method that mutates `self` (`p.bump()`) is not counted as
   a write; nothing in the checker says which methods mutate, and treating every method as a write
   would false-positive on every getter.
4. **No scope coordinate** — the taint is keyed by bare name, so any *new* binding of that name (a
   `:=`, a loop variable, a `match`/`wait:`/destructuring binding, a parameter) clears it. That is what
   keeps a loop variable that merely *shadows* the name from being reported, at the cost of missing a
   later stale read of the outer binding once a block-local shadow has appeared.
5. **A partial write through an index/field target** (`m[k] = v`, `p.f = v`) untaints silently, unlike a
   mutator, because the checker cannot tell whether it supersedes the task's write (`m["a"] = 2` after a
   task-side `m["a"] = 1` does; `m["b"] = 2` does not), and it declines rather than warn on noise.
6. **A write made only through a closure or nested `fn` declared inside the task** is not tainted — the
   nested body has its own frame, so `spawn: bump := fn(): xs.push(1)` then `bump()` leaves `xs.len()`
   at 0 after the join with nothing reported (same for the `fn bump():` spelling). Dropping the taint
   there is what stops the nested body reporting the *parent's* pending write as its own.

**Mutating a captured local.** A **closure body is a single expression** (`fn(x): expr`), so a closure
cannot contain a reassignment statement — `fn(): n = n + 1` is a *parse error*. Three ways to write
through a captured binding: (a) a **method call**, which *is* an expression, so a closure can mutate a
captured heap value — `bump := fn(): xs.push(2)`; (b) a **`defer:` / `spawn:` block**, whose body
*is* statements, so a reassignment is fine there — `defer: n = n + 1`; or (c) a **nested `fn`
declaration** (below), which also has a statement body — `fn bump(): n = n + 1`. (A bare `n = n + 1`
in the enclosing scope always works; it's only *inside a closure value* that you need a method call.)

If you relied on the old snapshot-at-creation behaviour, take an explicit copy: `snap := x` and
capture `snap` (a fresh binding nothing else writes is effectively frozen).
Runnable demo: [`examples/closure_capture_scopes.chz`](../examples/closure_capture_scopes.chz).

### Nested function declarations

A `fn` statement written **inside** another function body (or any block) is a **first-class local
function** — a closure with a name and a multi-statement body. It is not just sugar for a top-level
fn; it captures the enclosing scope exactly like a closure value:

- **Lexical nearest-scope.** The name resolves to the *nearest* binding in both the type-checker and
  the runtime, so a nested `fn f(x: int)` **shadows** a top-level `fn f()`. Its body is fully
  type-checked (a wrong-typed `return` is a compile error), and a call site is checked against the
  *nested* signature — `f()` with zero args against the shadowing `fn f(x: int)` is a **check-time**
  arity error, not a run-time fault.
- **Recursion.** A nested fn may call **itself** (`fn fact(n: int) -> int: … fact(n - 1)`), just like
  a top-level fn.
- **Uniform by-reference capture.** It captures outer bindings **by reference** under the same cell
  model as any closure — reads see later writes, and because its body *is* statements it can
  **reassign** a captured local (`fn bump(): x = x + 1`), with the write visible in the defining scope.
  A captured loop variable rebinds into a fresh cell each iteration (Go ≥1.22), and across the
  `spawn` / `parallel:` airlock a plain captured local is snapshot-copied (isolated), identical to a
  closure's capture — see the two rules above.

```chezzi
fn main():
    n := 0
    fn fact(k: int) -> int:          # recursive nested fn
        if k <= 1:
            return 1
        return k * fact(k - 1)
    fn bump():                       # statement body → can reassign a captured local
        n = n + 1
    bump()
    bump()
    print(fact(5))                   # 120
    print(n)                         # 2 (write is visible in the enclosing scope)

main()
```

**v1 limits.** A nested fn may **not** be generic (`fn id[T](x: T)` inside a body is rejected — declare
it at the top level), and **mutual recursion** between two sibling nested fns is unsupported: a nested
fn is only in scope *after* its own declaration, so `a` referencing a later-declared sibling `b` is a
`unknown name 'b'` error (declare such a pair at the top level instead). A nested fn may **not** be
named after a **builtin / constructor** the runtime resolves before a local — a reserved builtin
(`print`/`range`/`int`/`List`/`Channel`/…), a same-module **struct** or **newtype** constructor, or a
builtin **variant** ctor (`Ok`/`Err`/`Some`/`None`) — because the backend would run the builtin while
the checker saw the local; it is a `nested function name '…' is reserved` compile error (a nested fn
*may* share a **user enum variant's** name — those aren't bare-callable, so there is no divergence).
All of these are clean compile-time rejects, never a check-OK/run-fault.

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

**One-way `int`→`float` widening — an UNTYPED CONSTANT only (Go's rule).** An untyped int **constant**
expression adapts to a `float` context and is converted to a real `f64`. A **typed** `int` **value**
never implicitly converts — write `float(x)`. The reverse (`float`→`int`) is always a type error (lossy).

An untyped int constant is an int literal, unary `-`, and the arithmetic operators `+ - * / %` composed
over those (`1`, `-5`, `1 + 2`, `2 * 3`). Anything carrying a declared type is TYPED and does not adapt:
a name, a call result, a field, an index — so `i := 1; x: float = i`, `x: float = i + 1`, and
`x: float = cmp.max(1, 2)` (a fn RESULT is a typed int, even with constant args) are all type errors,
each naming the fix (`a typed int never widens to float — write float(x)`).

An untyped int constant adapts at every value-DEFINITION sink: a typed binding (`x: float = 1 + 2` →
`3.0`), a `float` function / method parameter (coerced at the CALLEE prologue, from the DECLARED param
type), a `float` parameter DEFAULT (`fn g(a: float = 3)`), a `-> float` return, a `float` struct field
(`P(3)` for `v: float`), and a **mixed-numeric-constant** collection literal — a list/map literal with ≥1
untyped float constant infers `List[float]` / `Map[_, float]` and coerces its untyped int constants
(`[1, 2.3]`, `[1, -2.5]`, `[1 + 1, 2.5]`), as does an annotated `xs: List[float] = [1, f]` /
`m: Map[str, float] = {"a": 1}` / `xs: List[float] = [1, 2]` (the annotation is the type CONTEXT). A
`float` sink spelled through a type ALIAS (`type F = float`; `x: F = 1`, `fn g(z: F)`, `v: F`) is a float
sink like any other. Because the conversion is real, the value behaves as a float everywhere —
`x: float = 3` makes `x / 2 == 1.5` (float division), not `1`. The mixed-type arithmetic / comparison
operators (`1 + 2.0`, `1 < 2.3`, `1 == 2.3`) follow the same one-way rule.

Four boundaries follow from the rule (all are the SAME rule — the sink must be DECLARED `float`, since
that declaration is what the backend coerces from — not exceptions):
- A call through a function **VALUE** never widens (`f := id[float]` / `f: fn(float) -> float = h`; write
  `f(1.0)`). The coercion lives in the callee prologue, driven by the callee's DECLARED param type — a
  generic fn instantiated at `float` declares `T` and is generic-erased at runtime, and a `fn(float)`
  value cannot be told apart from it, so neither adapts. A fn-typed struct FIELD is a fn value too.
- A **generic-erased** slot never widens: a method param declared as the type variable (`fn set(self, x: T)`
  on a `Box[float]`) is `T` at runtime, so `b.set(1)` is an error — write `b.set(1.0)`. A param declared
  `float` on the same generic struct adapts normally.
- A collection annotation must be SPELLED as one: `xs: List[float] = [1, 2]` adapts, but through a
  whole-collection alias (`type LF = List[float]`; `xs: LF = [1, 2]`) it does not (write `[1.0, 2.0]`).
  An aliased ELEMENT is fine — `type F = float`; `xs: List[F] = [1, 2]`.
- A **variadic** `float` param (`fn f(...zs: float)`) adapts its untyped int constants only when an
  untyped float constant sibling is present (`f(1, 2.5)` ✓, `f(1, 2)` ✗ — write `f(1.0, 2.0)`): the args
  are packed into a `List[float]` the callee prologue cannot coerce.
- The element widening of a mixed-numeric-CONSTANT literal needs a NUMERIC element type to ask for it.
  An `Any` element SLOT declines it — at EVERY position the slot reaches a literal, so
  `xs: List[Any] = [1, -2.5]`, `f([1, -2.5])` for `fn f(xs: List[Any])`, `f(1, -2.5)` for
  `fn f(...xs: Any)`, a `List[Any]` struct-constructor argument and a `-> List[Any]` return all keep
  the `1` an `int`, exactly as CPython's `[1, -2.5]` does. (`Any` is the empty top protocol, not a
  numeric type, and the slot already sanctions the heterogeneous literal.) An alias spelling of the
  top type decides the same way (`type A = Any; xs: List[A] = [1, -2.5]`), but a generic type param
  named `Any` shadows the protocol and is not an `Any` slot. A TYPED int element is never touched
  either way (`a := 1; xs: List[Any] = [a, -2.5]` keeps `1`), and the hint stays on the IMMEDIATE
  literal — a nested one is un-annotated and unifies as usual (`n: List[Any] = [[1, -2.5]]` →
  `[[1.0, -2.5]]`). An `if`/`match` ARM inherits the slot, so `f(if c: [1, -2.5] else: [2, -4.0])`
  keeps its `1` an `int` too; the SCALAR numeric-mix path is untouched — `x: Any = if c: 1 else: 2.5`
  still unifies its branches and stores `1.0`. And the SPELLING of a position never changes the
  answer: `[1, -2.5] |> f()` means exactly what `f([1, -2.5])` means, piped or not.

Un-annotated, there is no type context, so **no** adaptation: `f := 2.5; xs := [1, f]` is an error
(`list elements differ: int vs float`) — annotate `xs: List[float] = [1, f]`. Likewise a TYPED int
element never widens, annotated or not: `a := 1; xs: List[float] = [a, 2.3]` is an error; write
`[float(a), 2.3]`.

Anti-lossy cases stay type errors: `y: int = 2.3`, `fn f() -> int: return 2.3`, a `float` into a
`List[int]`, and an `int`→`float` into a **newtype** (nominal — no widening across its boundary).
Widening is **scalar-or-element-at-the-sink** — a nested / type-argument float slot is NOT widened:
`List[List[float]] = [[1]]`, `float? = Some(3)`, `float! = Ok(3)`, and a non-literal RHS
(`List[float] = f()`) all stay type errors; write explicit floats (`[[1.0]]`, `Some(3.0)`) or a literal.
One further scoped restriction: a plain
reassignment `x = 3` to a `float`-declared local is rejected (a reassignment target is type-blind, like
`p.x = 3`).

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
> shift (`<<`) that drops a significant bit overflows like `+ - * /` — a recoverable
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

**Return-only type params recover from an inferable closure/fn body.** A generic higher-order
function whose type parameter appears **only in its return** (e.g.
`fn applyone[U](x: int, f: fn(int) -> U) -> U`, `fn mymap[U](xs: List[int], f: fn(int) -> U) ->
List[U]`) recovers that `U` from the body of the closure/fn passed for `f` — exactly as the builtin
methods `.map`/`.fold` do. So `applyone(5, fn(x): x * 2)` yields `int` (and `+ 1` type-checks) and
`mymap([1,2,3], fn(x): str(x))` yields `List[str]`. The closure body is re-inferred with its param
types pinned, then its return flows back to fix `U` — the same principle as omitting a function's
return type and inferring it from the body. This works whether the body is a direct expression, a
`.method(…)` call, or a **nested generic call** (`fn(x): ident(x)` for `fn ident[T](x: T) -> T`). When
`U` is instead **already pinned** by a sibling value argument or an explicit slot (`fn f[U](init: U, g:
fn(int) -> U) -> U` with `init = 0` ⇒ `U = int`), the closure's return is *checked* against that pin,
so a mismatching body (`fn(x): str(x)` where `U = int`) is a clean type error, not laundered. A
return-only param that **no** body can pin (`fn make[U]() -> U`) stays genuinely un-inferable.

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
expression's type (`fn ten(): 10` infers `-> int`); otherwise **all** the body's `return` branches
(plus an implicit trailing/inline expression) are typed and **merged** with a join. A body with no
value-returning `return` infers `nil`. Param types stay required. The join `J(a, b)` is: (1) equal
types → that type; (2) mixed `{int, float}` branches **conflict** — an inferred return is *not* a
widening sink, so annotate `-> float` to opt into the coercion (widening emits `Op::CoerceFloat` only
at an explicit sink); (3) the **same** type-constructor (`Result`/`Option`/`List`/`Map`/
`Set`, or the same generic struct/enum) with differing type-args → **merge slot-wise** (each slot: one
side `?`/un-inferred fills from the other; two concrete slots must be **equal**, no widening inside
payloads — `Result[int]` and `Result[float]` **conflict**). The `Result` **error slot** is special:
two *different* `Err` payload types that **both satisfy the `Error` protocol** (`return Err("s")` vs
`return Err(myErr)`) do **not** conflict — they unify to the built-in `Error` protocol (see below); a
payload that does **not** satisfy `Error` keeps the strict equal-or-conflict rule. (4) otherwise → a
**conflict** error
`cannot infer return type: conflicting branches (X vs Y); add a -> annotation`. There is **no
common-supertype / protocol / `Any` search** for the T-slot: two distinct concrete types (e.g. two
structs that both have a `speak()` method) *conflict* — a protocol return must be spelled explicitly
(`-> Shape`).

So `fn res(): if …: return Err("a")` then `return Ok("h")` infers `Result[str, Error]` (the `Ok`
branch pins `T=str`; the error slot defaults to `Error` because the `Err` payload `str` **satisfies**
`Error`). A concrete error type is honored as-is only when written explicitly (`-> Result[str, str]` /
`-> int!DbErr`). Slots that stay un-inferable after the merge are resolved at a **finalize** step: the
`Result` **error slot** becomes the built-in `Error` protocol when it is un-pinned or its payload
**satisfies `Error`** (so `fn ok(): return Ok(5)` is `Result[int, Error]`, matching the `T!`
shorthand); a concrete payload that does **not** satisfy `Error` (e.g. a struct without `message`) is
**preserved** so a bogus `.message()` on it is still rejected. **Any other**
residual un-inferable slot — a `Result`/`Option` value slot, a `List`/`Map`/`Set` element — is an error
`cannot infer return type of '<name>'; add a -> annotation`. Hence `fn err(): return Err("x")`,
`fn none(): return None`, and `fn f(): return []` are each rejected (the value type is un-inferable, the
return-position analogue of the empty-collection diagnostic) — annotate them (`-> str!`, `-> int?`,
`-> List[int]`).

A function whose **sole body is a diverging call** — `fn boom(): panic("msg")` (or `exit(...)`) — is
**not** un-inferable: it never returns a value normally, so its return type defaults to `nil` (like a
void body), and callers type-check. (An annotated diverging body — `fn b() -> int: panic(...)` — is
already valid: bottom fits any return position.)

Inference is **order-independent**: a recursive call contributes no type mid-analysis (it is absorbed,
the concrete branches decide), and forward references / mutual recursion resolve via a fixpoint — so a
callee defined *after* the caller still yields the caller's precise inferred type. A function that is
genuinely un-inferable (pure self- or mutual recursion with **no concrete base case anywhere**) leaves a
residual un-inferable return and is rejected the same way; annotate it with an explicit `-> T`. This all
applies uniformly to **struct/enum methods** *and* **closures** (a free `f := fn(): Ok(5)` gets
`Result[int, Error]`; a free `fn(): Err("x")` is rejected) as well as free functions: an inferred method
return flows to call sites (`P(3).val()` is typed by the inferred return, not `Unknown`) and to
**protocol satisfaction** (an inferred `compare(self, o)` yielding `bool` fails `Comparable`, which
requires `-> int`, exactly as an explicit `-> bool` would).

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
`= 1 + 2`, `= GLOBAL * 2`, `= compute()`).

A default is **evaluated once per omitting call**, and it is **resolved and evaluated in the module
that DECLARES it** — never in the caller's. Those are two separate rules and Chezzi follows a
different ancestor on each:

* **Scope — same as Python, Ruby and Kotlin.** The names a default expression sees are the definer's:
  its own module globals, its own functions, its own imports. A caller that happens to declare a
  same-named global (or a local that shadows one) cannot reach in. So `g.chz` holding `K := 7` +
  `fn f(x: int = K)`, imported by a `main.chz` that declares its own `K := 99`, prints **`7`** —
  exactly what the two-module Python equivalent prints.
* **Timing — a deliberate divergence from Python.** Python evaluates a default **once, at `def`
  time**, and shares that one object with every call, which is the classic mutable-default footgun
  (`def f(xs=[])` accumulates). Chezzi re-evaluates on **every omitting call**, so `f(n: int = bump())`
  returns `1`, `2`, `3` on three calls, and `f(xs: List[int] = [])` hands each call a fresh list.
  Passing the argument explicitly does not evaluate the default at all.

Because parameters are not in scope in the declaring module's top level, a param-referencing default
(`y: int = x + 1`) is rejected: *"default value cannot reference parameter 'x' (a default is evaluated
on its own, where parameters are not in scope)"* — CPython raises `NameError` on the same shape.
This includes a reference made from inside an **interpolated fragment** (`x: str = "n={n}"`), which
used to slip past the check because it runs before `"…{…}…"` is rewritten. Everything else a fragment
can spell is still fine — a global, a call, arithmetic (`x: str = "{tag()}-{1+2}"` is legal) — so what
is rejected is the parameter reference, not the interpolation.

Callers may also pass arguments **by name**:

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
`g()`). A default is an arbitrary expression, so it **does** have an observable order, and it is the
same one: **every** filled slot evaluates in **parameter-declaration order**, whether its value came
from a positional argument, a named one, or a default, and a default runs **once per omitting call**.
Measured — `fn f(x = p("d-x"), y = p("d-y"), z = p("d-z"))` called as `f(z=p("arg-z"), x=p("arg-x"))`
prints `arg-x`, `d-y`, `arg-z`. Scope: free functions
(own module, `from`-imported, or module-qualified `mod.f(...)`), struct constructors, **and struct
methods** (`p.greet(punct="?")`, `p.scale()` filling a default) — a method's default is compiled in
its declaring module like any other, and resolves there however the caller reaches it (see *"Where a
default is compiled"* below). Because a method's receiver type is
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
positionally" error. A **static** method (no `self` parameter) takes defaults like any other — its explicit arguments
start at parameter 0, so `S.mk()` fills them. Defaults are **not**
supported on **closures** or on **enum variant constructors** — note this is the variant
*constructor*; an enum's *methods* take defaults just like struct methods. (Per §above, a default may
be any expression that doesn't
reference another parameter — a literal, a global, arithmetic, or a call; only param-referencing
defaults are rejected.)

**Where a default is compiled, and the five rules that follow from it.** A non-literal default is
compiled **once**, as a hidden zero-arg function in the module that declares it; an omitting call site
calls that function. (A self-contained literal — `= 1`, `= -1`, `= 1 + 2`, `= None`, `= []`, a
brace-free string — is still copied inline. That costs no call and behaves identically in every
program that does not *shadow* the name: `None` and `nil` are keywords to the lexer, but a local
binding called `None` in the caller does reach the copy — `fn f(x: int? = None)` called from a body
containing `None := 5` reports `argument 1 of 'f': expected Option[int], found int` at the
declaration. That corner predates this design and is unchanged by it.) Five
consequences are worth writing down, because each is a rule you can hit:

1. **`?` cannot propagate *out of* a default.** `fn f(x: int = getr()?.len()) -> int` is a compile
   error: *"a default expression cannot propagate with `?` — defaults are evaluated in their defining
   module, which has no caller to propagate to; use `??` or return an Option"*. An error escaping into
   the *caller* from an expression owned by the *definer* is exactly the coupling this design removes.
   It also **widens**: a `Result`-typed parameter whose default propagates *inside* its own scope now
   works, where it used to be rejected — `fn f(x: int!str = Ok(getr()?.len()))` compiles, and returns
   `4` on the `Ok` path and the `Err` unchanged on the other. Option-mode `?.` and `??` never
   propagated, so both are unaffected (`x: Option[int] = geto()?.len()`, `x: int = geto() ?? 0`).
2. **A default always resolves in the module that DECLARES it — including where the caller cannot
   name that module.** Because a method call is resolved by *name* before types are known, a method
   default declared in module `a` can be reached from a module `z` that never imports `a`; a
   protocol-typed parameter is the ordinary way that happens (`z` declares the protocol, `a` declares
   the struct that satisfies it, and `z` cannot import `a` without a cycle). Where the definer *is* a
   transitive dependency, a hidden `from` import binds the provider and the call resolves through it.
   Where it is not, no import may be synthesized — an import edge is resolved when the *caller's*
   module loads, and a non-dependency can load later — so the call site instead holds a **direct,
   call-time reference** to the definer's provider. Either way the answer is the definer's.

   This used to be a documented **hazard**: the expression was copied into the call site and resolved
   in the caller's scope, so `z`'s own `av()` won (measured: `510` where the definer wrote `11`), and
   a name `z` could not see was a compile error (`unknown name 'u'` in module `z`) even though the
   definer's module imported it perfectly well. Both are gone. The equivalent two-module CPython
   program prints the definer's value, and so does this.

   Passing the argument explicitly is unchanged and still reads the *caller's* scope — that is the
   difference between passing a value and omitting one.

3. **A default that needs itself is a compile error.** `fn f(x: int = f())` is refused, naming the
   parameter: *"the default for 'x' of 'f' is cyclic: evaluating it requires evaluating the default for
   'x' of 'f' again"*. Mutual and indirect provider cycles are caught the same way.
4. **A `Self`-typed parameter's default gets a provider on a non-generic host, and the inline clone
   on a generic one.** A provider is a free top-level `fn` declared `-> <the parameter's type>`, and
   `Self` names the receiver type, which a free fn cannot spell (see *"`Self`"* below). On a
   **non-generic** host `Self` names exactly one concrete type, so it is substituted for that type in
   the provider's declared return type and the default behaves like any other: `fn combine(self,
   other: Self = mkq())` — and any default whose type merely mentions `Self`, like `List[Self]` —
   resolves in its defining module. On a **generic** host `Self` is `Q[T]`, whose `T` is still unbound
   in a free fn, so no provider can be written for it — the **callee** fills it instead, from a
   prologue compiled into the declaring module where `Self` and `T` are both in scope, and the call
   site simply omits the argument. The same route covers a default whose type or expression mentions
   an enclosing **type parameter** and one whose expression literally spells `Self` (`= Self.mk()`).
   So every non-literal default resolves in its definer, whichever route it takes.

   The one shape that cannot: a **keyword** call that supplies a later parameter while omitting an
   earlier callee-filled one (`G(1).m(k=3)` over `fn m(self, xs: List[Self] = mkl(), k: int = 9)`).
   A short call drops a suffix; it cannot leave a gap. That is a compile error naming the parameter
   to pass explicitly — not a silent fall back to the caller's scope.

5. **A cycle routed through an ordinary function is a runtime fault, not a compile error.**
   `struct S: n: int = mk().n` with `fn mk() -> S: return S()` type-checks clean and then faults with
   `maximum call depth (10000) exceeded (infinite recursion?)`, rc 1, identically —
   the same shape as CPython's `RecursionError` on the equivalent program. A documented limit, not a
   defect: the compile-time check sees provider→provider edges, and this cycle's edge runs through
   `mk`.

A default may also be a **variadic call** (`fn f(a: int = sum_all(1, 2), ...xs: int, tail: int =
sum_all(3, 4))`), in the pre-variadic slot and in the keyword-only tail alike; that shape used to be
rejected and now matches CPython's `def f(a=sum_all(1,2), *xs, tail=sum_all(3,4))` byte for byte.

Built-ins take no named arguments, with **one** exception: **`print`** accepts `sep=` (default `" "`,
joins the positional args) and `end=` (default `"\n"`, appended after) — both `str` (see `docs/stdlib.md`).
So `print("a", end="")` writes `a` with no trailing newline, and `print("a","b", sep="-", end="!")`
writes `a-b!`. Any other named argument on a built-in is an error. `print`'s signature is the
file-backed variadic decl `native fn print(...args: Any, sep: str = " ", end: str = "\n") -> nil`.

**Variadic parameters (`...name: T`).** A parameter written `...xs: T` is **variadic**: it collects
the surplus trailing positional arguments into a fresh `List[T]` (Go/Swift `T...` style).

```chezzi
fn sum_all(...xs: int) -> int:
    total := 0
    for x in xs:                 # `xs` is a `List[int]`
        total = total + x
    return total

sum_all(1, 2, 3)                 # 6
sum_all()                        # 0  — zero args → empty list
```

Rules: **at most one** variadic per signature; it must carry an element type (`...xs: T`, never bare
`...xs`); it may **not** carry a default. Everything **after** a variadic is **keyword-only** — the
variadic eats all trailing positionals, so a following parameter can only be supplied by name. A
post-variadic parameter *with* a default is an optional keyword arg; *without* a default it is a
**required keyword arg**:

```chezzi
fn labeled(prefix: str, ...xs: int, sep: str = ", ") -> str: ...
labeled("nums: ", 1, 2, 3)             # sep defaults to ", "
labeled("dash: ", 4, 5, 6, sep="-")    # keyword-only `sep`
```

Variadics are allowed on free functions, methods, and `native fn` decls — **not** on closures or
`extern` (C) functions (the C ABI needs fixed per-arg types; see `docs/ffi-and-packaging.md §5`). Used
as a first-class **value**, a variadic fn takes the collapsed `List[T]` slot (`g := sum_all; g([1,2,3])`
works, `g(1,2,3)` does not) — the same fixed-value-form rule as `print`.

**`Any` (the top type).** `Any` is an **empty structural protocol** — zero required methods, so **every**
type satisfies it (scalars `int`/`float`/`bool`/`str` and `nil` included, not just structs/enums). It
is the honest element type of a universal slot such as `print(...args: Any)`, and can annotate any
binding or parameter (`x: Any = 42`, `fn log(v: Any) -> nil`). It is **not** dynamic typing: an `Any`
value carries no methods, so you can pass it around and display it but not call methods on it (a
downcast `cast[T]` is a documented future addition — see `docs/future.md`). `Any` is a reserved
protocol name (a program may not redeclare it). `Any` is now **expressible** as an ordinary empty
protocol — it is defined in the prelude as `protocol Any:` with a lone `pass` body — and **any**
user empty protocol behaves identically to it (see [`pass`](#the-pass-keyword) below): an empty
protocol is a general accept-all top type, not a special case keyed on the name `Any`.

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
may omit only a **trailing** run of defaulted parameters — `h := hasdefault; h()` fills the default
(the CALLEE does it, from the declaration, so the value never had to carry it), and `h(a=1, b=7)` may
omit a trailing `c`, but a value call cannot leave a HOLE before an argument it does supply
(`h(1, c=9)` over `fn f(a, b=2, c=3)`), because a short call is exactly "fewer values pushed" and
cannot express a gap — call the function directly by name for that shape; **(2)**
first-class **built-in** function values (`p := ord`) take **no** keyword arguments (labels are a
user-function surface).

How few arguments a function value may be called with is part of what it means to store one: a
binding typed from `fn a(x: int = 1)` may be called with none, so a function that *requires* an
argument cannot be assigned into it (`h := a; h = b` over `fn b(x: int)` is a type error). The
reverse is fine — a defaulted function is strictly more permissive, so it flows into a plain
`fn(int) -> int` slot. Named arguments through a value evaluate in **parameter-declaration order**, the
same as a direct named call, and work in `defer`/`spawn` position too (`defer d(name="Zoe")`). Resolution
is fully static (the checker rewrites the keyword call to a positional one; the runtime `Op::Call` /
`DeferCall` / `SpawnCall` stay positional), so the VM produce identical output.

**A GENERIC fn as a value.** A generic function (`fn ident[T](x: T) -> T`) becomes a usable **value**
once its type parameters are **pinned** — either with an explicit **turbofish** or against a **known
concrete `fn(...) -> ...` type** (an annotation, a HOF parameter, a return position, or an assignment
target). The value then has the fully-substituted concrete function type; calling it works like any
other fn value.

```chezzi
fn ident[T](x: T) -> T:
    return x

g := ident[int]                      # turbofish pins T=int  ⇒ g : fn(int) -> int
print(g(5) + 1)                      # 6

h: fn(int) -> int = ident            # annotation pins T=int
print(h(5) + 1)                      # 6

fn applyit(f: fn(int) -> int, x: int) -> int:
    return f(x)
print(applyit(ident, 5) + 1)         # 6 — HOF parameter pins T against the param type

print([1, 2, 3].map(ident))          # [1, 2, 3] — a builtin HOF slot fn(int) -> U also pins T=int

fn getf() -> fn(int) -> int:
    return ident                     # return position pins T against the declared return type
```

The pin is checked strictly: an **unsatisfiable** target (`g: fn(str) -> int = ident` — `ident` can't be
both) is a type error, a **bound violation** (`addone[str]` where `str` is not `Add`) is rejected, a
**turbofish arity mismatch** (`pair[int]` for a two-param `pair[A, B]`) is a clean error, and the value
keeps its **concrete** type downstream (`g := ident[int]` then `s: str = g(5)` is rejected — `g(5)` is
`int`). The pin works through a **builtin container HOF parameter slot** exactly as through a
user-defined HOF: passing a bare same-module generic fn to `.map`/`.filter`/`.fold` (and the other
closure-taking container methods) pins its `[T]` from the element type — `[1,2,3].map(conv)` for a
`conv[T](x: T) -> str` type-checks to `List[str]`, and `[1,2,3].fold(0, add)` for an `add[T: Add]` pins
`T=int` and enforces the bound — even though those methods also carry their own result type parameter.
The runtime is generic-**erased** — the value *is* the underlying function — so an indirect call
adds no overhead and behaves identically.

A **bare, un-pinned** generic fn value — `g := ident`, with no turbofish and nothing that determines
`[T]` — is rejected **at the read**, whether or not it is ever called. This is Go's rule
(`cannot use generic function id without instantiation`); the diagnostic names the undetermined
parameters and the working spellings:

```chezzi
g := ident        # 'ident' is generic and T is not determined here, so it cannot become a function
                  # value — instantiate it (`ident[<T>]`), or give this position a concrete function
                  # type (`fn(<T>) -> <T>`), writing a real type in place of each `<…>`
```

The same read is the same error inside a `[...]`/`{...}` literal, in a `return` from a fn with an
**inferred** return type, as a `print` argument, and in a generic **constructor** / generic **free fn**
argument whose slot is not a function type (`Bx(ident)`, `take(ident)` on `fn take[U](f: U) -> int`) —
nothing there determines `T` either. A generic with
**two or more** type parameters is only fixable by giving the position a concrete function type: a
fn-value turbofish carries exactly one type argument (`pair[int]` for `pair[A, B]` is an arity error),
so the diagnostic does not offer it. First-class (rank-N) polymorphism — one binding used at two
different types — is a future addition; Go and Rust refuse it too.

**Argument position asks the same question.** A bare generic fn handed to a HOF — a user one, or any
builtin container HOF (`map`, `filter`, `fold`, `sort_by`, `sort_by_key`, `min_by`, `max_by`,
`take_while`, `drop_while`, `count`, `position`, `for_each`/`fold_entries`, `Shared.update`,
`RwShared.write`) — is refused with the *same* diagnostic when the slot cannot determine its type
parameters: `[1,2,3].filter(pred)` for a `pred[T](n: int) -> bool` whose `T` appears nowhere, and
`[1,2,3].map(mk)` for a return-only `mk[T](n: int) -> List[T]`, are both errors. Go answers the same
(`in call to takeBool, cannot infer T`). The check runs at the **end of the call**, once every
argument has had its chance to pin: `[1,2,3].fold(0, pick)` for `pick[T](a: T, b: T) -> T` is pinned
by the accumulator, which is the *first* argument while `pick` is the second — and a user generic
method may spell the two slots either way round (`b.app(ident, 5)` works as well as
`b.app2(5, ident)`). An **empty** receiver is the one carve-out, and it suppresses only the *report*:
its element type is `?`, which is a slot nothing filled rather than a type parameter nothing
determined, so `[].map(ident)` and `[].map(mk)` still type-check and print `[]`. The **pin is never
suppressed** — a slot the other arguments determine still pins straight through an empty receiver, so
`[].fold(0, add)` runs and prints `0`, exactly as Go's `Fold([]int{}, 0, add)` returns `0`. And the
carve-out is the *receiver's* `?` only: a type parameter of the method itself that ends up
undetermined is still reported, so `Bx(0).two(ident, ident)` on
`fn two[U](self, f: fn(U) -> U, g: fn(U) -> U) -> List[U]` is an error (Go: `cannot infer U`).

**The HOF may be generic itself.** When the slot belongs to a *generic* callee, that callee's own type
parameters are pinned first — by the other arguments, a call-site turbofish, or the enclosing
annotation — and only then does the passed fn unify against the now-concrete slot. So for
`fn applyg[U](f: fn(U) -> U, n: U) -> U`, all of `applyg(ident, 5)`, `applyr(5, ident)`,
`applyg[int](ident, 5)` and `xs: List[int] = mklist(ident)` type-check and run, and
`fn twop[A, B](f: fn(A) -> B, a: A) -> B` infers `B` from the passed fn's own return
(`twop(text, 5)` is `str`). Go and Rust infer the same instantiations. What the callee cannot pin is
still refused with the same diagnostic: `nopin(ident)` on `fn nopin[U](f: fn(U) -> U) -> int` has
nothing that determines `U` (Go: `cannot infer U`), `twop(mk, 5)` can reach `B` only through `mk`'s own
undetermined `T` (Go: `cannot infer B`), and `applyg(mk, 5)` cannot match the shapes at all.

One position is deliberately **not** covered, because the concrete type is present but does not reach
the read: a parameter or field **default value** (`fn run(f: fn(int) -> int = id)`) has a concrete slot
the checker does not thread into the expected-type hint, so the bare read is refused there and
`= id[int]` is the spelling that works. (v1 limit: the pin requires a **same-module** generic fn — an
*imported* generic fn used bare as a value stays rejected on every path.)

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
elif x == 0:           # 'elif' (Python-style single keyword), chainable
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

# A range is NOT a value: `a..b` is a syntactic form legal ONLY as the iterable of a `for` loop or
# comprehension, as a slice receiver, or as a `match` pattern. It is lazy (`for i in 0..10000000000`
# never materializes), so it has no runtime representation to bind, print, index, or pass along —
# any other use is a TYPE ERROR at `chezzi check`. Use `range(a, b)` to materialize a `List[int]`:
#   x := 0..3          # error: a range is only valid as the iterable of a `for` loop or …
#   List(0..3)         # error (same) — the ctors take an iterable VALUE, and a range isn't one
xs := range(0, 3)            # [0, 1, 2]         — the materializing escape hatch
print(Set(range(0, 3)))      # a real List[int], so every container ctor / list op accepts it

for item in items:     # iterate a list
    print(item)

for k in counts:       # iterate a map → its keys (insertion order)
    print(k)

for k, v in counts:    # iterate a map's entries → key + value
    print("{k}={v}")

for a, b in pairs:     # destructure a List[(A, B)] — N names over a List[tupleN]
    print("{a}:{b}")   # (one name binds the whole tuple). enumerate/zip live in std.iter.

# `for` over a List/Map/Set iterates a SNAPSHOT of the SPINE, taken once when the loop starts —
# shallow: the elements are the same objects (mutating a struct element IS visible), but pushing,
# popping or inserting inside the body cannot change how many steps run, and the new entries are
# never visited. This is a deliberate DIVERGENCE from Python, which detects the mutation and raises:
m := {"a": 1}
for k in m.keys():         # runs ONCE. Chezzi: 1 visit, m.len() ends at 2, rc=0.
    m["b"] = 2             # CPython: RuntimeError: dictionary changed size during iteration
# The snapshot means the mutation is SAFE (no fault, no aliasing bug) — it is just INVISIBLE to this
# loop. Collect the additions and apply them after the loop if you need to see them. Same rule for a
# `.iter()` cursor (below) and for the callback methods listed under "snapshotting" in stdlib.md.

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

# `Iterable[T]` is a real protocol bound: a generic fn can take ANY iterable — built-in
# list/set/map/str/bytes, a `.iter()` cursor, a generator, or a user struct with `next`/`iter` — and
# recover its element type `T`. (`Iterator[T]` is the stricter sibling: a CURSOR, see below.)
fn first_or[S: Iterable[T], T](xs: S, default: T) -> T:
    for x in xs:           # x is typed `T`
        return x
    return default
first_or([10, 20], 0)      # 10   (T = int, recovered from the list element)
first_or("hi", "?")        # "h"  (T = str)
# The bound is not the only spelling — `Iterable[T]` works in TYPE position too, as a plain
# (non-generic) parameter whose element type comes from the annotation:
fn total(xs: Iterable[int]) -> int:
    n := 0
    for v in xs:           # v is typed `int`
        n += v
    return n
total([1, 2, 3])           # 6   (a set, a map's keys, a cursor or a generator work the same)
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
# but reusing one exhausted cursor yields nothing on a second pass. Driving a NAMED cursor with `for`,
# `List(it)` or `Set(it)` CONSUMES it in place (advances the shared position, exactly like `.next()`),
# so a partial `for … break` then leaves the remainder for a later `next()`/`List()`. A cursor IS sendable across
# `spawn` — it crosses the airlock as a deep copy, like a `list`. `Iterable` / `Iterator` are reserved type names.

# `yield` / generators (run on both VM engines; a live frame-local generator IS sendable across a task airlock — it crosses by value as an independent deep copy of its execution state, incl. one suspended mid-`recover:`; only a module-GLOBAL generator is reach-gated). Any fn that
# uses `yield` is a generator: calling it returns a suspendable iterator, not a value. It runs lazily,
# suspending at each `yield` and resuming on the next `.next()`. The `-> Iterator[T]` annotation is
# OPTIONAL — with no return type the element type `T` is inferred from the FIRST `yield`
# (strict-first-yield); every later `yield` must be assignable to that `T`, else a clear error.
fn count_up(n: int):       # no `-> Iterator[T]`: `T = int` inferred from the first `yield`
    i := 0
    while i < n:
        yield i            # produce a value, suspend until the next .next()
        i = i + 1
for x in count_up(3):      # drives the generator: prints 0, 1, 2
    print(x)
# An explicit `-> Iterator[T]` still works (and validates every yield against `T`):
fn count_up2(n: int) -> Iterator[int]:
    i := 0
    while i < n:
        yield i
        i = i + 1
# A generator can also be a struct method (`fn m(self)`, annotation optional), and a generator value
# is a real `Iterator[T]`: drive it by `for`, pass it to an `[S: Iterator[T], T]` bound, or call
# `.next()` explicitly — it returns `Some(v)` per yield, then `None` once exhausted:
g := count_up(2)
match g.next():            # Some(0)
    Some(v): print(v)
    None: print(-1)
# `return` (bare only) stops a generator early; `defer`/`spawn`/`parallel:`/`wait:` are not allowed
# inside a generator. Inference REJECTS an un-inferable element (`yield []` alone, or an int-then-float
# mix — no silent int->float coercion at a `yield`): annotate `-> Iterator[T]` in that case. `Iterator`
# is a reserved type name. See examples/generators.chz (full showcase), examples/generators_inferred.chz
# (inference-only), and examples/generators_basic.chz.

while cond:
    cond = step()

# `break` exits the innermost loop; `continue` skips to the next iteration.
```

### The `pass` keyword

`pass` is a reserved keyword with two roles.

**(1) A no-op statement.** `pass` does nothing. It is valid anywhere a statement is — a fn/method
body, an `if`/`else` branch, a `for`/`while` body, a statement `match` arm, or a concurrency block —
and is the idiomatic way to write an empty body. A function whose body is a lone `pass` runs, falls
off the end, and returns `nil` — exactly like a lone `return`:

```chezzi
fn todo():
    pass                # empty body; returns nil (same as `return`)

for x in xs:
    pass                # a deliberately-empty loop body
```

`pass` is a **statement**, not an expression, so it is not valid where a single expression is
expected — a closure (`fn(): …`) or an expression-position `match` arm. A no-op closure is `fn(): nil`.

**(2) An empty-body marker for `protocol` and `struct`.** Protocol and struct bodies hold
*declarations* (method signatures / fields), not statements, so a **sole** `pass` line there means
"empty body":

```chezzi
protocol Top:          # zero methods → an accept-all top type (like `Any`)
    pass

struct Unit:           # zero fields → `Unit()` takes no args
    pass
```

An empty **protocol** is a general accept-all top type: with no methods, structural satisfaction is
vacuous, so **every** type satisfies it. This is exactly how the reserved `Any` (see §5) is
defined, and any user empty protocol behaves identically (`fn f(x: Top)` accepts an `int`, a `str`, a
struct…; `xs: List[Top] = [1, "a", true]` type-checks). An empty **struct** has zero fields: `Unit()`
constructs it, it prints as `Unit()`, two `Unit()` compare equal, and it is usable as a `Set` element
or `Map` key (a zero-field struct is intrinsically `Hashable`). It still heap-allocates like any
Chezzi value (no Go zero-size optimization). `pass` must be the **only** line of such a body — `pass`
mixed with a field/method, or a repeated `pass`, is a parse error. An **empty enum** is *not*
supported: `pass` in an enum body is rejected (an enum needs at least one variant).

Because `pass` is a real keyword it cannot be used as a name (variable, parameter, field, function,
type, module alias) — `pass := 5` is an error.

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
machinery (`(0..5)[::-1]` → `[4, 3, 2, 1, 0]`). The slice receiver is one of the few positions a range
literal is legal in at all — it is **not a value** anywhere else (see the range section above); use
`range(a, b)` to materialize a `List[int]`.

**Negative indexing** counts from the end (`xs[-1]` is the last element) for plain indexing *and*
slice bounds, on `list`/`str`, including as an assignment target (`xs[-1] = v`). The out-of-range
rule follows Python's asymmetry: a plain `xs[-100]` on a short list **faults** (`index -100 out of
bounds (len N)`), while a slice bound `xs[-100:]` **clamps** to the start (never faults).

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

**Struct instances are REFERENCE values (Python's object model, not Go's).** `q := p` binds the *same*
instance — it does not copy — so a write through either name is visible through both, and passing a
struct to a function lets the callee mutate the caller's value. Same for a struct stored in a `List`
or as a `Map` **value**:

```chezzi
q := p
q.x = 99
print(p.x)              # 99  — one object, two names (CPython prints 99; Go would print 3)
```

There is **no `copy`/`clone`** — duplicate by re-constructing (`Point(p.x, p.y)`). The one place a
struct *is* snapshotted is as a `Map`/`Set` **key** or a `Set` **element** (see §"Keys are value types
(Go model)" under Maps) — so keys are the language's single value-semantics island; everything else is
by reference. A `spawn:`/`parallel:` boundary is a third model again: values cross **by copy** through
the airlock (§11b).

**Methods are not first-class values.** `p.dist` is not an expression — a method exists only to be
**called** (`p.dist()`); there is no bound-method value. To pass one around, wrap it in a closure:
`f := fn(): p.dist()`. (A struct **field** that is *fn-typed* — `f: fn(int) -> int` — is an ordinary
value: `s.f(3)` and `g := s.f; g(3)` both work. The distinction is field vs method.) Reading a method
name as a value is a check-time error: `type Point has no field 'dist' ('dist' is a method — …)`.

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

A **module-qualified** base takes the **single-arg** type-level turbofish too:
`shapes.Tree[int].Leaf(9)` (qualified enum-variant ctor) and `shapes.Box[int].make(5)` (qualified
static method) both work in expression position, as does the combined form
`shapes.Box[int].make[str]("hi")`. (A *multi-arg* qualified turbofish —
`shapes.Pair[int, str].X` — is not yet supported; write the base same-module, or let the args
infer from the call: `shapes.Pair.X(...)`.)

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
turbofish, multi-arg: `pair.first[int, str](1, "x")`. Static methods on `newtype` are not
supported yet (struct + enum only).

#### Static protocol requirements — calling `T.method(...)` through a bound  (M24)

A protocol may require a **static** method — one whose first parameter is not `self`. A type
witnesses it by declaring that method statically (an instance `default(self)` does **not** witness
it), and a generic bounded by such a protocol may **call it through the type parameter**. This is the
one thing an instance-only protocol cannot express: *construct* a `T` you were never handed:

```chezzi
protocol Default:
    fn default() -> Self          # STATIC requirement: no `self` receiver

struct Counter:
    n: int
    fn default() -> Counter:
        return Counter(7)

fn reset[T: Default](old: T) -> T:
    return T.default()            # dispatches to the CONCRETE type

print(reset(Counter(1)).n)        # 7
```

**How it works — witness passing, not monomorphization.** Generics stay erased: there is still one
body per generic fn and no type argument reaches the VM. A type parameter whose bound carries a
static requirement, *and* whose body needs it, is given a **hidden trailing parameter** holding the
concrete type's runtime identity key; `T.method(...)` compiles to a call that pops that key and runs
the same dispatch as an ordinary `Type.method(...)`. The parameter is charged **only** to a body that
uses one, so a generic that merely *has* a static-carrying bound (`fn tagged[T: Spawnable](x: T) ->
str: return x.tag()`) is untouched and keeps every position it had before.

**What works.** A struct or an enum host; same-module and cross-module calls in every import
spelling (`m.reset(x)`, `import reset from m`, `import reset as again from m`); a `T` inferred from
an argument, pinned by turbofish (`empty[Counter]()`), or fixed by an annotated result
(`c: Counter = empty()`); **forwarding** a still-abstract `T` from one generic into another,
transitively, recursively and mutually; a type parameter declared by a **member** — instance or
static, on a plain or generic host (`h.make(x)`, `Holder.build(x)`, and the member turbofish
`h.make[Counter](x)`); the call inside a **closure** (including one that escapes its defining frame),
a **nested `fn`**, a **`defer:`** block and a **`spawn:`/`parallel:`** block (the witness is a value,
so it crosses the airlock); and the reserved **`Convert[S]`** protocol, whose witness is a static
`convert(x: S) -> Self`:

```chezzi
fn make[T: Convert[int]](seed: T, n: int) -> T:
    return T.convert(n)
```

**What does not, each with a diagnostic naming the workaround.**

| shape | why | do this instead |
|---|---|---|
| `T` declared by the enclosing **TYPE** (`struct Bx[T: Default]` … `T.default()` in a method) | the concrete type is erased once a `Bx` *value* exists — only a value could hold the witness | declare the parameter on the **member** (`fn fresh[T: Default](self, …)`), whose witness rides on the call |
| reading a witness-taking fn as a **function value** — `g := reset`, `reset[Counter]` as a value, passing it to a HOF, a cross-module read | a `fn` value erases which declaration it came from, so no witness can be recovered. **A permanent wall, not a v1 limit** | call it directly, or take a factory closure: `fn make[T](mk: fn() -> T) -> T` |
| a `T` **not determined** at the call site | there is no concrete type to build a key from | pin it (`nodet[Counter]()`) or annotate the result |
| a bound witnessed by a **newtype** or a **scalar** | neither can host a static method | use a struct or an enum |
| a **manifest entrypoint** that takes a witness or any declared parameter (`entrypoint = "src.main:main"` where `fn main[T: Default]()` or `fn main(a: int)`) | it is invoked with no arguments, so nothing supplies the key (or the argument) | give the entrypoint a nullary, non-generic signature and construct / read inputs in a helper it calls. Reported by `chezzi check` and by bare `chezzi run`; an explicit `chezzi run <file>` is script mode and runs the top level regardless |

The turbofish *call* form is fine — `reset[Counter](Counter(1))` works; it is only reading
`reset[Counter]` as a **value** that hits the wall.

**When the hidden parameter is charged.** Only a body that can actually use one pays: it names `T`
in expression position (`T.default()`), or it *forwards* — it calls a witness-taking fn in a way that
could pass its own still-abstract `T` along. A call whose arguments are all concrete forwards nothing
(`fn concrete[T: Default](x: T) -> int: return reset(Counter(1)).n` keeps its value position), and
merely naming a module that exports a witness-taking fn is not a call to it (`lib.plain(1)` costs
nothing, `lib.reset(x)` charges). Anything the rule cannot positively read as concrete charges —
a zero-argument call, a local, a turbofish naming `T`, a generic constructor head — because an
under-charge is a forward the checker then has to refuse.

A **member** forward charges too (`h.build[T](x)`, and `h.build(x)` with `x: T`) — even when that is
the body's ONLY use of `T`, which no free-name reading of the body could see: the type argument is
not a name, and the receiver is not a module. A member call has no callee to resolve before types
exist, so it is judged on **two** conditions, both required: this **call site** carries something of
the enclosing fn's (a type argument naming one of its type params, or an argument mentioning one of
its parameters whose annotation does — at any depth, so `h.build(xs[0])` with `xs: List[T]` charges),
**and** the method **name** is declared as witness-taking somewhere in the program. It therefore
costs nothing to call a method that merely shares a name with a witnessed one (`p.build(1)`), or to
call an ordinary builtin from inside a static-bounded generic — `m.get("a")` fails the first
condition, and `xs.push(x)` with `x: T` on a `List[T]` fails the second, since `List.push` is a
builtin that can take no witness.

Two shapes are still refused for want of a charge. First, the same fence as on the free-fn channel:
a `T` that occurs in **neither** a parameter type nor the return type of its own fn is never charged
for a forward (`fn f[T: Default](h: Holder, k: int) -> int: q := h.mk[T](); return k`, and the
free-fn `q := empty[T]()` equally), because a charge whose type no argument can determine would make
the fn uncallable. Second, a member forward whose `T` reaches the argument through a **local** rather
than a parameter (`v := x` then `h.build(v)`) — the call site alone cannot see where `v` got its
type. Give `T` a place in the signature, spell the member turbofish (`h.build[T](v)`, always
charged), or construct through it directly (`T.default()`, charged regardless).

A type parameter **SHADOWS a same-named declaration for its whole body, in every position** — the
annotation `x: Item`, the static call `Item.tag()`, the type-level turbofish `Item[int].tag()`, the
constructor `Item(99)`, a nullary variant `Item.Red`, and a same-named *function* `foo()`. Chezzi has
one namespace, so this is Go's rule (`func fv[foo any](x foo) int { return foo() }` → *missing
argument in conversion to foo*; `Item{}` under `[Item any]` → *invalid composite literal type Item*).
`fn f[Item: Tagged](x: Item) -> int: return Item.tag()` beside a `struct Item` therefore dispatches on
the *argument's* type. Only the static call can be lowered; every other position rejects, because a
type parameter is erased — it has no constructor, takes no type arguments and has no members but a
bound's static method. A real local binding still wins over the parameter. Rename the type parameter
if you meant the outer declaration.

Static methods still do **not** participate in *instance*-method protocol satisfaction: a protocol
requirement is matched static-to-static and instance-to-instance, never across. Worked demo:
`examples/static_witness.chz`.

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

A bound may equivalently be written in a **`where` clause** after the return type — an alternative
spelling of the same bounds, useful to keep a long parameter list readable. Each `where` entry names
a declared type parameter and lists its protocols (`+`-joined, comma-separated across entries); the
checker merges them into the matching `[T]` parameter, so the two forms are interchangeable:

```chezzi
fn max[T](a: T, b: T) -> T where T: Comparable:   # same as `fn max[T: Comparable](a, b)`
    if a < b:
        return b
    return a

fn combine[A, B](a: A, b: B) where A: Add + Mul, B: Comparable:   # multi-entry, multi-bound
    ...
```

A `where` entry naming a type parameter that isn't declared in `[…]` **and** isn't the enclosing
type's own parameter is an error.

**Conditional methods.** A `where` on a *method* may name the **enclosing struct/enum/newtype's own
type parameter** (not the method's `[U]`): the method is then callable only when the receiver's
concrete type argument satisfies the bound — Rust's `impl<T: Ord> Box<T> { fn top(…) }`. It mirrors
the built-in `List[T].sort` / `sum` (each a `where T: …` on `T` = the list's element). This is a
**checker-only** bound — it lowers to nothing at runtime — so a satisfying instance runs exactly like
an ordinary method on every engine.

```chezzi
struct Box[T]:
    val: T
    fn top(self) -> T where T: Comparable:   # conditional on the RECEIVER's own T
        return self.val

    fn max2(self, other: T) -> T where T: Comparable:
        if self.val < other:                 # the body MAY use the bounded op (`<` needs Comparable)
            return other
        return self.val

b := Box(5); print(b.top())          # 5   — int is Comparable
print(b.max2(9))                     # 9

import std.net
fn f(s: net.Socket):
    bad := Box(s); bad.top()         # ERROR at check time: Socket does not satisfy Comparable
```

Just like a free fn's `where`, the method **body** may use the bounded operation (`<` above needs
`Comparable`) — the receiver bound is in scope on the enclosing type parameter for the whole body.
The receiver-param bound is enforced wherever the concrete type argument becomes known — on an
instance call (`b.top()`) **and** on a static factory reached as `Type.method(…)` (a no-`self`
method: `fn of(x: T) -> Box[T] where T: Comparable` rejects `Box.of(q)` when `q`'s type isn't
`Comparable`).

**Conditional conformance.** When the conditional method *is* a protocol's required method — e.g. a
`compare(self, other: Self) -> int where T: Comparable` makes `Box[T]` *structurally* satisfy
`Comparable` (no companion `eq` is needed: `Comparable` embeds `Eq`, and a type whose `==` is the
structural derive satisfies `Eq` without writing one) — the receiver bound makes that conformance **conditional**: `Box[int]` satisfies
`Comparable` (so `Box(1) < Box(2)` and passing a `Box[int]` to a `[U: Comparable]` generic are both
fine), but `Box[Tag]` (with `Tag` not `Comparable`) does **not** — the `<`, and the bound-check, are
rejected at compile time. The bound is honoured *everywhere* conformance is queried — operator
dispatch (`<`, `+`, …), generic bounds, and protocol-typed parameters — not just at an explicit
`.compare()` call.

**`==` honours the bound too** (`docs/gaps.md` **W7-41** / **W7-45**, both fixed 2026-08-11). A
conditional `fn eq(self, other: Self) -> bool where T: Comparable` is the hook `==`/`!=` dispatch to,
and the equality path consults conformance exactly as `<` does:

```
Box[Tag] == Box[Tag]  →  type error: cannot compare Box[Tag] and Box[Tag] for equality
                         — Box[Tag]'s `eq` requires Tag: Comparable
```

while `Box(1) == Box(2)` still runs and prints `false`. Rust agrees (`impl<T: Ord> PartialEq for
Boxy<T>` leaves `Boxy<Tag> == Boxy<Tag>` un-callable, `error[E0369]`). The same rule covers `!=`,
containers and payloads (`[a] == [b]`, `Some(a) == Some(b)`, tuples, map values, struct fields,
newtype underlyings), `x in xs`, and the builtins whose runtime is `values_equal` —
`list.contains`/`index_of`/`dedup`/`unique`, and every map-key / set-element position, each with its
own message naming the site (*contains() compares List[Box[Tag]] elements for equality — …*,
*map key type Box[Tag]'s `eq` requires …*).

**A protocol-typed (existential) operand or element defers the same bound check to runtime, by
design, not by gap.** Protocols are Go-style interfaces, and comparing two protocol-typed values is
exactly Go's `interface{} == interface{}`: the checker cannot know which concrete witness inhabits
the protocol at a given site (existentials are erased), so it compiles the comparison and the
comparison faults cleanly, at the point it runs, if that witness's `eq` turns out unsatisfied — the
same shape as Go's own `panic: runtime error: comparing uncomparable type …` (`docs/gaps.md`
**W7-52**, resolved 2026-08-12 as ancestor-correct). `Ty::Protocol` also satisfies a `[T: Eq]` BOUND
now, agreeing with the bare `==` on the identical spelling (`fn generic_eq[T: Eq](a: T, b: T) -> bool:
return a == b` fed two protocol-typed values, or `.eq()` directly) — matching Go 1.20+'s widened
`comparable`, which likewise admits an interface type and panics the same way at the comparison if
the witness cannot be compared. Every OTHER protocol still correctly rejects a protocol-typed value
that does not structurally provide it (`[T: Stringable]`/`[T: Hashable]`/… over `Sized_` all reject);
`Eq` is the one protocol this applies to, because `Eq`-satisfaction is defined as exactly what `==`
already accepts, and `==` already accepted a protocol-typed operand.

**A generic body that compares its own type parameter must bound it by `Eq`, at the DEFINITION —
matching both owning ancestors.** `fn f[T](a: T, b: T) -> bool: return a == b` rejects at `f`'s own
declaration — `cannot compare T and T for equality — T is not bounded by Eq (add an `Eq` bound to T:
`[T: Eq]`, or `where T: Eq` on a fn)` — the same shape rustc 1.97.0 produces (`error[E0369]`, *help:
consider restricting type parameter `T` with trait `PartialEq`*) and Go 1.26 produces (`invalid
operation: a == b (incomparable types in type set)`). (The message names both spellings because the
same check also fires at a bare type ANNOTATION — `struct Reg[K: Hashable]: m: Map[K, int]` — where
`where` is not grammar at all.) The same rule covers `in`/`contains`/`index_of`/`dedup`/`unique` over
`List[T]` and a `T` reached as a map key / set element. A type parameter that is never compared pays
no tax — the
obligation is per-USE, not a blanket requirement on every `[T]`. `Comparable` already embeds `Eq`, so
`[T: Comparable]` needs no separate `Eq` bound; `Hashable` does **not** embed `Eq` (Rust's `Hash` has
no `Eq` supertrait either — `HashSet<T>`/`HashMap<K, V>` spell `Eq + Hash` explicitly), so a generic
that builds/indexes a `Map`/`Set` keyed on its own type parameter needs `[T: Hashable + Eq]`
(`docs/gaps.md` **W7-53**). And a type graph nested deeper than **10 000** links is REFUSED at a
`[T: Eq]` bound or an `==` — tied by construction to the VM's own equality depth cap, so the checker
never grants a compare the runtime can't itself perform. A growing (polymorphic-recursion) type graph
is refused far shallower than that, in bounded work (a couple of consecutive re-entries that keep
getting strictly larger) rather than by walking to the cap (`docs/gaps.md` **W7-55**).

**Protocols** are Go-style structural interfaces: a block of body-less method signatures. A type
satisfies a protocol by *having* the methods — there is no `implements` declaration. `Self` inside
a signature refers to the conforming type.

`Self` is also usable in an **inherent** `struct`/`enum`/`newtype` method's signature and body
(param type, return type, local annotation), where it names the enclosing type — `fn dup(self) ->
Self` inside `struct P` returns a `P`, and for a generic `Box[T]` it carries the receiver's own type
args. It resolves to the concrete enclosing type, so a `-> Self` method returning a different type is
a type error. `Self` is meaningful only inside a method; naming it as a free-fn parameter, a struct
field, or a top-level annotation is `unknown type 'Self'`.

```chezzi
protocol Comparable:                 # PREBUILT/reserved — its shape is file-backed in std/prelude.chz
    Eq                               # embedded: ordered ⇒ equatable (Rust's `Ord: Eq`)
    fn compare(self, other: Self) -> int

struct Point:
    x: int
    y: int
    fn compare(self, other: Point) -> int:   # ⇒ Point satisfies Comparable, structurally.
        return (self.x + self.y) - (other.x + other.y)
    fn eq(self, other: Point) -> bool:       #   `eq` is OPTIONAL here — a struct already satisfies
        return (self.x + self.y) == (other.x + other.y)   # `Eq` structurally. Write one only to
                                                          # OVERRIDE `==`, and keep the two agreeing.

print(max(Point(1, 2), Point(3, 0)).x)   # works: Point is Comparable
```

The prebuilt **`Comparable`** protocol (`compare(self, other: Self) -> int`) is the protocol wired to
the **ordering** operators. For any `Comparable` value — including a bare `T: Comparable` —
`< <= > >=` dispatch to `compare` (a negative/zero/positive result means less/equal/greater).
`int`, `float`, and `str` satisfy `Comparable` intrinsically. (It is not the only operator-wired
protocol: `Eq` owns `==`/`!=` below, and `Add`/`Sub`/`Mul`/`Div`/`Mod`/`Neg` own the arithmetic
operators further down.)

```chezzi
print(Point(1, 1) < Point(5, 5))     # true  — `<` calls Point.compare
```

Equality (`==` / `!=`) is **not** routed through `compare` — it has its own protocol, **`Eq`**. (Routing
`==` through `compare` was considered and rejected: a `compare` that ignores some fields would silently
redefine equality, and `<`'s ordering answer is the wrong shape for it.)

The prebuilt **`Eq`** protocol (`eq(self, other: Self) -> bool`) is the protocol wired to `==`/`!=`:
a struct/enum that defines `eq` **owns its `==` / `!=`**, exactly as `compare` owns its `<`. A type that
defines **no** `eq` keeps the structural (field-by-field) equality it always had.

**`Eq` satisfaction is exactly "`==` works on it"**, and writing an `eq` is not what earns it — the
language's structural `==` is an automatic derive, and since 2026-08-11 it tells the protocol system so
(`docs/gaps.md` **W7-41**). So `where T: Eq` is writable over `int`/`float`/`bool`/`str`, `bytes`,
tuples, `List`/`Map`/`Set`, `Option`/`Result`, newtypes, any struct or enum, a **function value**
(a user closure/free fn, or a first-class universe builtin like `ord`/`chr`/`panic`/`print`), and a
**protocol-typed (existential) value** — the same set `==` accepts. Go gives structs `==`
automatically, Rust spells it `#[derive(PartialEq, Eq)]`, Python `@dataclass(eq=True)`; Chezzi's is
implicit. A function value compares by IDENTITY, not structurally — two loads of the same
top-level/nested `fn` def (or the same builtin) are equal, two calls to a factory minting a fresh
nested `fn` are not, and two closures with equal captures are not — matching CPython's `f == g`
exactly and Rust's fn-pointer `PartialEq` (`docs/gaps.md` **W7-54**, fixed 2026-08-12; Go is the one
ancestor that differs, rejecting `f == g` outright). A protocol-typed value defers entirely to its
concrete witness — see the deferred-to-runtime paragraph above (`docs/gaps.md` **W7-52**, fixed
2026-08-12; matches Go 1.20+'s `comparable`, which likewise admits an interface type). The one thing
that revokes the grant is an
`eq` (its own, or one reached through an element / entry / tuple slot / field / payload / newtype
underlying) whose `where` bounds do not hold for the instantiation in hand — that is the `Box[Tag]`
case above. **Still outside the grant, and filed:** a bare `T` with no bound (deliberate — a generic
body is checked once with `T` abstract).

```chezzi
struct Ver:
    maj: int
    pre: str
    fn compare(self, o: Ver) -> int:
        return self.maj - o.maj
    fn eq(self, o: Ver) -> bool:                 # `==` asks THIS, not the fields
        return self.maj == o.maj

print(Ver(1, "alpha") == Ver(1, "beta"))         # true  — `pre` differs, `eq` says equal
print(Ver(1, "alpha") != Ver(1, "beta"))         # false — `!=` is the same dispatch, negated
```

For a struct/enum, `a.eq(b)` and `a == b` are **one** dispatch in both directions, so a `[T: Eq]` body
may spell either. (A *newtype* cannot get into that position at all: its `==` unwraps to the
underlying's native equality, so declaring an `eq` on one is a compile error — see the newtype section
below.) Dispatch is by the operands' **runtime type**: both sides must be the same
struct/enum type or the comparison stays structural `false` without calling user code, and for an enum
it is the *enum* that decides — one `eq` also answers `Shape.Circle == Shape.Square` (Rust `PartialEq` /
Python `__eq__` compare across variants).

`eq` is **not** a reserved name, so the operator dispatches only to the hook's exact signature,
`fn eq(self, o: Self) -> bool`. A wrong arity, a concrete non-`Self` operand, or a non-`bool` return
is rejected at the **declaration**, not left to answer wrongly at the operator:

```chezzi
struct A:
    v: int
    fn eq(self) -> bool:      # type error: 'eq' … must take exactly one operand
        return true
```

The one alternative shape that stays legal is a **generic** operand — an ordinary method that merely
shares the name (Rust allows an inherent `eq` beside `PartialEq`; Python namespaces the hook as
`__eq__`). `==` leaves it alone and stays structural:

```chezzi
enum Opt[T]:
    Some(T)
    None
    fn eq(self, x: T) -> bool:               # an ordinary method, NOT the Eq hook
        return true

print(Opt[int].Some(1).eq(7))                # true  — the method still works
print(Opt[int].Some(1) == Opt[int].Some(2))  # false — `==` stays structural
```

**Through a protocol bound, `.eq()` is the protocol's equality — never the same-named ordinary
method.** A *concrete* receiver keeps calling the ordinary method (Rust's inherent-wins rule); a
receiver whose type is a bound type parameter resolves to the protocol, exactly as Rust resolves
`a.eq(b)` under `T: Eq` to `<T as PartialEq>::eq`. The three spellings therefore give three answers,
and they are the three rustc gives:

```chezzi
struct Key:
    n: int
    fn eq[U](self, o: U) -> bool:            # an ordinary method, NOT the Eq hook
        return true

fn eqm[T: Eq](a: T, b: T) -> bool:
    return a.eq(b)

print(Key(1).eq(Key(2)))                     # true  — concrete receiver: the ordinary method
print(Key(1) == Key(2))                      # false — `==` stays structural
print(eqm(Key(1), Key(2)))                   # false — through the bound: the PROTOCOL's equality
```

A real `eq` **hook** is still dispatched through the bound, of course — that is what makes `Eq`
user-overloadable. The rule is one sentence: through a bound, `.eq()` means whatever `==` means for
that receiver, so the method spelling and the operator can never disagree (`docs/gaps.md` **W7-53**).

A newtype cannot satisfy `Eq` through its own `eq` **method**: a newtype's `==` always unwraps to the
underlying's native equality, so declaring `eq` on one is rejected at the declaration site (the method
could never agree with the operator). It satisfies `Eq` anyway — via that same unwrapped equality, which
is a working `==` — so `where T: Eq` accepts a newtype over any underlying.

A user `eq` reaches **every** equality site, not just the operator — `Map`/`Set` key lookup (`m[k]`,
`has`, `get`, `remove`, `in`, `add`), `x in xs`, `list.contains`/`index_of`/`dedup`/`unique`, set
algebra, and the recursive element/field/entry compares inside `==` on a container:

```chezzi
struct K:
    a: int
    b: str
    fn hash(self) -> int: return self.a
    fn eq(self, o: K) -> bool: return self.a == o.a

x := K(1, "x")
y := K(1, "y")
print(x == y)          # true
print(y in [x])        # true
m: Map[K, int] = {}
m[x] = 10
print(m[y])            # 10
print([x] == [y])      # true — the recursion reaches the element's `eq`
```

**Writing a correct `eq` is your job, and these are the limits — read them before you write one.**
The first two are rules carried over from Python; the rest are ceilings Chezzi does not (and largely
cannot) enforce.

* A **container** short-circuits on identity first (`x is y or x == y`, CPython's
  `PyObject_RichCompareBool`), so `[x] == [x]` is `true` even for an `eq` that answers `false` for
  everything. The bare `==` **operator** has no such shortcut and always calls `eq`, so `x == x` for
  that same type is `false`.
* **`hash` and `eq` must agree — this is the implementor's contract, and it has a hard structural
  limit.** A `Map`/`Set` probe can only ever scan the buckets of `hash(key)`, so an `eq` *coarser*
  than its type's `hash` is **unreachable**, not merely wrong: two values that `eq` calls equal but
  that hash differently will never meet. Rust and Python leave this to the implementor too; Chezzi
  does not try to enforce it. **What to do:** key `hash` and `eq` on the **same fields** — `eq` may
  read fewer fields than the struct has, but never fewer than `hash` does.
* **Keep `eq` reflexive (`x.eq(x)` is `true`) or a value stops finding itself in a container.** A
  `Map`/`Set` key is a **snapshot** (the value model, `§Keys are value types` below), so the probe
  compares your value against a *copy*, not against the same object — the identity short-circuit
  above cannot save you. An `eq` that answers `false` for everything makes `x in {x}` and `x in m`
  both `false`:

  ```chezzi
  struct Never:
      a: int
      fn hash(self) -> int: return self.a
      fn eq(self, o: Never) -> bool: return false     # never reflexive — a bug, not a feature

  x := Never(1)
  s: Set[Never] = {x}
  print(x in s)       # false — the stored key is a COPY, so identity never fires
  print([x] == [x])   # true  — same object, identity short-circuit fires
  ```
* **An `eq` that mutates the very container it is being probed against answers from the container it
  LEFT behind — never a crash.** The probe re-reads its candidate list each step *and* re-validates
  the matched position after each compare; if the entry moved, the whole probe restarts (CPython
  `lookdict`'s `DKIX_KEY_CHANGED`). So `m.get(k)` inside such an `eq` returns `k`'s value in the
  post-mutation map, not a shifted neighbour's and not an out-of-range panic. An `eq` that mutates
  on *every* call makes the probe spin, exactly like an `eq` containing `while true`. Don't do it.
* **`Atomic[T].cas` compares structurally and never calls a user `eq`** — it holds the value's lock
  across the compare, and re-entering user code there would deadlock. Two layers keep that true:
  the checker **rejects a payload type that REACHES a user `eq`** — its own, or one on any element,
  entry, tuple slot, struct field, enum payload or newtype underlying the structural compare would
  recurse into (use `Shared[T]`, which has no `cas`, for such a type) — and the VM **switches the
  `eq` hook off** for the duration of the compare, so the guarantee does not depend on that walk
  being able to see through a protocol existential. See [`concurrency.md`](concurrency.md).
* **A generic type's instantiation is ERASED at runtime, so `Any` can hand `eq` a foreign operand.**
  `a: Any = Box(1)`, `b: Any = Box("x")`, `a == b` type-checks (the `Any` escape hatch from the
  disjoint-type rule) and dispatches `Box`'s `eq`, whose body then faults on `int` vs `str`. It is a
  normal recoverable fault, catchable with `recover:` — and it is what CPython does with the same
  program (`TypeError` out of `__eq__`). There is no per-instantiation tag to dispatch on; keep the
  static type if you want the compile-time answer.
* **`match` never dispatches `eq`.** A literal pattern is `int`/`str`/`bool` only, and a struct/enum
  arm matches by variant and *binds* fields rather than comparing them — so a user `eq` cannot change
  which arm is taken.

**`Comparable` embeds `Eq`** (mirroring Rust's `Ord: Eq`): a type ordered must also be equatable. It
still does — but since a struct/enum satisfies `Eq` structurally, **`compare` alone is enough**:

```chezzi
struct Ver:
    maj: int
    fn compare(self, o: Ver) -> int:
        return self.maj - o.maj
    # no `eq` needed — Ver satisfies Comparable, and `==` stays structural
```

M23 shipped a rule that a type defining `compare` must define `eq` too; it was **dropped 2026-08-11**
(`docs/gaps.md` **W7-41**) because measurement falsified its premise in both directions. A
field-complete `compare` that agrees with structural `==` exactly was a **false reject**, while a
`compare` reading a *subset* of the fields plus an `eq` reading a different subset — this file's own
`Ver` example above — sailed through. So it never enforced coherence; it enforced *"you typed the word
`eq`"*. Rust, the owning ancestor, permits manual `Ord` beside a derived `Eq` (a clippy lint, not an
error). Keeping `compare` and `eq` agreeing remains the implementor's job — the checker cannot verify
`eq(a, b) == (compare(a, b) == 0)` and does not pretend to. int/float/str keep satisfying `Comparable`
intrinsically (the scalar grant short-circuits before the embed is flattened).

`==` / `!=` between **provably-disjoint types is a compile error** — `1 == "a"`, `Box[int] ==
Box[str]`, or two different structs can only ever answer `false`, which is always a bug in the source.
This is a **deliberate divergence from Python** (which answers `False` at runtime): Chezzi is
statically typed, so it follows mypy's `--strict-equality`, Go, and Rust here. The question asked is
**"can these two ever be the same value?"** — *not* "is one assignable to the other", which is a
stricter, different question (it forbids write-through aliasing, and `==` never writes). So
everything the runtime can genuinely compare stays legal:

* the runtime's cross-type pairs — a mixed `int`/`float` (`1 == 1.0`) and `bytes` vs `bytearray`
  (content-equal, Python parity) — **at any depth**, so `[1.0] == [1]` and `{"k": 1.0} == {"k": 1}`
  compile and answer `true`, exactly as in Python;
* a **protocol existential against a type that conforms to it** (`sh: Shape` vs a `Sq`, an `Error` vs
  your error struct, in either operand order);
* **two different existentials** (`Shape` vs `Error`) — one concrete type can conform to both, so the
  pair is inhabited;
* an existential **nested inside a container, generic struct, or concurrency handle** — `List[Error]`
  vs `List[MyErr]`, `Map[str, Error]` vs `Map[str, MyErr]`, `Box[Error]` vs `Box[MyErr]`,
  `Option[Error]` vs `Option[MyErr]`, `(Error, int)` vs `(MyErr, int)`, `Shared[Error]` vs
  `Shared[MyErr]`. (Note these same pairs are *not* mutually **assignable** — a mutable container's
  type argument is invariant — but they can still hold equal values, which is all `==` asks.)
* any comparison involving an **erased type parameter**, bare or nested at any depth (`a == 1` and
  `xs == [1]` inside `fn f[T: Eq](a: T, xs: List[T])`; `a == b` inside
  `fn cmp[T](a: Channel[T], b: Channel[int])`; a free `T` in a parameterized protocol's own arguments,
  `Container[T]` vs a conforming `Bag[int]`). A `where T: <scalar>` bound is the exception: it *pins*
  `T` to that scalar, so `fn f[T](a: T, b: int) where T: str` still rejects `a == b`. This is the
  CO-INHABITANCE question — "can these ever be equal" — a separate axis from **whether `T` may be
  compared at all**: `T` itself still needs an `Eq` bound wherever it is genuinely walked for equality
  (above), so `fn f[T](a: T, xs: List[T]): return a == 1` rejects at ITS OWN declaration
  (`T is not bounded by Eq`) even though `int` and `T` would be co-inhabitable once `T` is bound. A
  HANDLE's own element type (`Channel[T]` vs `Channel[int]`) is the identity-compare exception — the
  handle compares by identity, never by its element, so `cmp`'s `T` needs no `Eq` bound at all.

A conforming existential is not a blanket pass: a **non**-conforming concrete stays an error at every
depth (`sh: Shape` vs a `str`, `List[Shape]` vs `List[str]`, `Container[T]` vs an `int`), and neither
is an erased parameter — a *handle* whose element types are disjoint is still disjoint
(`Channel[int]` vs `Channel[str]`). When you *want* the dynamic answer on a
genuinely disjoint pair, compare through the `Any` existential — widening **one** side is enough
(`u: Any = a; u == b`), since `Any` is the top type and disjointness is then not provable, and the
runtime's type-tag guard decides.
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

The embed set flattens at **every** use site, not only at a bound. Through a value annotated with the
protocol (an *interface value*) and through a bounded type parameter alike: an embedded method is
callable, `in` resolves an embedded `Contains`, `[]`/`[a:b]` an embedded `Index`/`IndexSet`/`Slice`,
and unary `-` an embedded `Neg`. A protocol value also **satisfies the protocols it embeds**, so a
`Person` (embedding `Named`) is accepted where a `Named` is wanted — Go's interface-to-interface
assignment. Type arguments stay **invariant** throughout: a `Container[str]` value never satisfies
`Container[int]`, embedded or not.

**Object safety — `Self` in a parameter position.** A protocol value erases which type it holds, so
two values of one protocol need not be the same witness — `a + b` over `a: Vecish, b: Vecish` could
hand a `W` to `V`'s `add`. A method whose signature **takes** `Self` is therefore not usable where
two witnesses could meet: calling it on a value (`a.add(b)`) is rejected, the operator forms are
rejected (every operator protocol's method is `(self, Self) -> Self`, so `+ - * / %` and `<` are all
affected), and a protocol value may not be the **witness for a generic type parameter** whose bound
needs such a method (`sum2[T: Vecish](a: T, b: T)` has two `T` slots). Bind the operands together
with a generic parameter over a CONCRETE type and it all works:
`fn plus[T: Vecish](a: T, b: T) -> T: return a + b` called with two `V`s.

Everything else about the value is unaffected — it still passes to a parameter of its own protocol,
still satisfies the protocols it embeds, and `Self` in the **return** is fine (it widens to the
protocol), which is why `fn neg(self) -> Self` and unary `-` stay usable. Rust's object-safety rule
is the same; Go bans `Self` from interfaces outright.

An embed's type argument must name a real type, and may name the owner's type parameter only as the
whole argument (`protocol Bag[T]: Contains[T]`) — nested inside another type (`Contains[List[T]]`),
or naming a type that does not exist, is rejected at the declaration rather than silently becoming a
requirement that accepts anything.

```chezzi
protocol Arithmetic:        # builtin/reserved — shape file-backed in std/prelude.chz
    Add + Sub + Mul + Div

protocol VectorSpace:       # embeds two protocols and adds its own requirement
    Arithmetic + Neg
    fn dot(self, o: Self) -> int

fn combine[T: Arithmetic](a: T, b: T) -> T:   # +, -, *, / all available on T
    return (a + b) * (a - b) / b

protocol Named:
    fn name(self) -> str

protocol Person:            # embeds Named, adds its own requirement
    Named
    fn age(self) -> int

fn only_named(n: Named) -> str:
    return n.name()

fn show(p: Person) -> str:       # an INTERFACE VALUE, not a bound
    return p.name() + " " + only_named(p)   # embedded method; Person satisfies Named
```

Indexing and slicing are overloaded through the prebuilt **`Index[K, V]`** (read `obj[k]` via
`index(self, key: K) -> V`), **`IndexSet[K, V]`** (mutable `obj[k] = v`, adds `set_index(self, key: K,
val: V)`), and **`Slice[R]`** (`obj[a:b:c]` via `slice(self, start: int? = None, end: int? = None,
step: int? = None) -> R` — each component is `None` when omitted) protocols.
Built-in `list`/`map`/`str` satisfy them intrinsically (`str` is read-only — `Index`/`Slice` but not
`IndexSet`); a struct defining the matching methods becomes indexable/sliceable. Because they are real
protocols, a generic can be bounded by them — `K`/`V`/`R` are recovered at the call site like
`Iterator[T]`'s element:

`IndexSet[K, V]` **requires `Index[K, V]` too** (as Rust's `IndexMut: Index`). A **compound**
`obj[k] += v` is `obj[k] = obj[k] OP v` (§3), so it *reads* through `index` and writes the result back
through `set_index` — the two must be **coherent** there: `index`'s return must fit `set_index`'s
`val`, and both must key on the same `K`. An incoherent pair used in a compound is a check-time error
(`type S does not satisfy IndexSet (index returns str but set_index's val is int)`) instead of the
runtime fault it used to be. A **plain** `obj[k] = v` never reads, so an asymmetric pair (a safe-read
`index -> V?`, a widening writer) stays legal there. `index` alone is legal (a read-only type);
`set_index` alone is not index-assignable.

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

**Membership (`in`)** is overloaded through the prebuilt **`Contains[Item]`** protocol (Python's
`__contains__`): a struct/enum defining `contains(self, item: Item) -> bool` makes `x in that_value`
dispatch to it, yielding `bool`. Built-in `list`/`set`/`str` test element/substring membership and
`map` tests **key** membership intrinsically (unchanged); the item type must be compatible with `x`.

```chezzi
struct Bag:
    items: List[int]
    fn contains(self, x: int) -> bool:
        return x in self.items

b := Bag([1, 2, 3])
print(2 in b)          # true   — `contains` dispatched
print(9 in b)          # false
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
protocol is also a first-class **value/annotation type** — `c: Container[int]` is a valid parameter,
return, field, or reassignment slot (an *existential*: any type that satisfies `Container[int]` is
accepted, and the protocol's own methods plus everything its embeds require are callable on it —
except a method taking `Self`, which is bound-only; see object safety above). The concrete args are witnessed
**statically at every store/pass boundary** (assigning a value into the slot checks conformance
there) and then **erased at runtime** (methods dispatch by name, like every protocol existential). A
method that returns the protocol's param **recovers** the carried arg — `c.get(0)` on a
`Container[int]` yields `int`, not the bare `T`:

```chezzi
fn first(c: Container[int]) -> int:   # existential value slot
    return c.get(0) + 1               # c.get(0) recovers to int
```

Value-position parameterized protocols are **strictly invariant**: `Container[int]`, `Container[str]`,
and bare `Container` are three distinct, non-interchangeable types (exact-arg match — no
`Iterator[int]`→`Iterable[int]` value-position subsumption). A **bare generic** protocol used as a
value type stays an existential with unbound params, so a struct whose method returns a concrete type
does **not** conform to it — supply the args (`Container[int]`) to use it as a value.

> **Protocols are module-local (by design, pre-freeze).** A `protocol` defined in one module cannot be
> reached from another in *any* form: not as a qualified type (`mod.Named`), not via bare-import
> (`import Named from mod` — a `struct`/`enum`/`newtype` bare-imports, a protocol does not), and not as
> a generic bound (`[T: mod.Named]` — the bound grammar is bare-identifier-only). Use a protocol only
> within its defining module; share cross-module contracts via a concrete type or a function parameter.
> (Cross-module protocol *export* is a possible future milestone, not a current feature.)

The prebuilt **`Iterable[T]`** and **`Iterator[T]`** are parameterized bounds with extra magic: they
**recover** `T` from the iterand's element (by unifying it), rather than requiring it written out. `T`
then flows into the body's loop variable and the return type. (User protocols take their args
explicitly; only these two recover them.) The two differ in WHAT they accept — the same split as Rust's
`IntoIterator` vs `Iterator`, or Go's `range` vs an iterator value:

* `[S: Iterable[T], T]` — **anything you can iterate.** Built-in `list`/`set`/`map`/`str`/`bytes`/
  `bytearray` (str → str, map → its keys) intrinsically, plus a `.iter()` cursor, a generator's
  `Iterator[T]`, and any struct with `next(self) -> Option[T]`. Use this whenever the body just does
  `for x in xs`. **This is the one you want by default.** (A struct with ONLY `iter` satisfies the
  protocol but does not get `T` recovered — see the mechanics below.)
* `[S: Iterator[T], T]` — **a cursor**: something that HOLDS a position, so the body may call
  `s.next()` directly. A `.iter()` cursor, a generator, or a struct with `next`. A RAW collection does
  NOT satisfy it (a fresh cursor per `next()` would hand back element 0 forever) — pass `xs.iter()`.

```chezzi
fn to_list[S: Iterable[T], T](xs: S) -> List[T]:
    out := []
    for x in xs:            # x : T
        out.push(x)
    return out
to_list("ab")              # ["a", "b"]   (T = str)
```

Mechanically: `Iterable[T]` promises only `.iter() -> Iterator[T]` (a fresh cursor), where
`Iterator[T]` additionally promises `.next()`. Every `Iterator` IS `Iterable` (its `iter()` returns
self), so a generator and a user `next`-struct satisfy `[S: Iterable[T]]` too; a struct with only
`iter(self) -> Iterator[E]` (no `next`) satisfies it as well and is for-iterable via a one-time
`.iter()` — though for THAT one the element recovery does not fire **in BOUND position**, so bound it
with a concrete arg (`[S: Iterable[int]]`) or annotate the parameter `Iterable[int]` (the annotation IS
the element type, so nothing has to be recovered). The cursor's type is the existing `Iterator[T]`
existential — there is no new value type.

**`next` wins by NAME.** A struct that declares a `next` at all is iterated through `next`, never
through `iter` — that is how the runtime picks, so it is how the type-checker picks. A struct that
declares a MALFORMED `next` (extra params, or a return that isn't `Option[E]`) is therefore **not
iterable at all**; it does not silently fall back to its `iter`. Drop the bad `next`, or fix it.

`Iterable[T]` also works in **TYPE position**, not only as a bound: `fn f(xs: Iterable[int])` takes any
iterable as a plain (non-generic) parameter, and the body may `for v in xs`, comprehend `[v for v in
xs]`, or `List(xs)` it. `T` comes from the annotation. Like every other protocol existential it is
strictly **invariant** in its args — a `List[int]` is an `Iterable[int]` and NOT an `Iterable[Any]`
(read-only covariance is deliberately not part of the model; a bare `Iterable` with no arg is an
existential with unbound params and is not iterable at all). `Iterator[T]` in type position is
unchanged and still additionally allows `.next()`; an `Iterable[T]`-typed value does not (take a cursor
with `xs.iter()` first).

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

The same auto-flow carries the `List` methods that need it: `.sort()`, `.min()`/`.max()` (and
`.min_by`/`.max_by`) order a `List[Meters]` by the wrapped scalar, and `.sum()` returns the
**newtype** — `[Cents(3), Cents(1)].sum() -> Cents(4)`, and an *empty* `List[Cents]` sums to
`Cents(0)` (Go's `type Cents int`). Integer overflow still faults. This is the numeric auto-flow, so
a *generic* newtype, a newtype **of** a newtype, and a `newtype Name = str` are rejected by `.sum()`
exactly as their `+` is.

A newtype may carry its own **methods** (a trailing-colon block, like a struct/enum), and satisfies
the **non-operator** prebuilt protocols by defining the relevant method — `str(self)` (Stringable
display override) and `hash(self)` (so it can be a `map`/`set` key — opt-in, *not* inherited from the
underlying) — so it passes into those protocol-bound generics (`fn show[T: Stringable](x: T)`). The
**operator** protocols (`Add`/`Sub`/`Mul`/`Div`/`Mod`/`Neg`/`Comparable`/`Eq`) are **not** satisfiable by a
newtype method: a newtype's own `add`/`div`/`compare`/… is never dispatched as an operator (the
same-type arm always auto-flows to the underlying's native op/ordering/equality), so only a **numeric**
underlying supplies them — a numeric newtype satisfies `Add`/`Sub`/`Mul`/`Div`/`Mod`/`Comparable`
intrinsically (native same-type ops above), while a `newtype Name = str` with an `add` (or `compare`)
method does **not** pass `fn twice[T: Add](x: T)` (or `fn sorted[T: Comparable](xs: T)`) — its `<`
would silently use the underlying's native ordering, never the method, so the checker rejects it.
(`eq` never even reaches that question: declaring one on **any** newtype is a compile error — see
below — so no newtype satisfies `Eq` through a method, and only a numeric one satisfies it at all.)

Because of that, a **numeric** newtype may not *define* a method named after an operator it actually
inherits (`add`/`sub`/`mul`/`div`/`mod`/`compare`) — and **no** newtype, numeric or not, may define
`eq` — it is a **compile error at the declaration**:

```chezzi
newtype Score = int:
    fn add(self, o: Score) -> Score:      # error: operator method 'add' on a numeric newtype
        return Score(99)                  # is never dispatched as an operator …
    fn doubled(self) -> Score:            # fine — ordinary methods are unaffected
        return Score(int(self) * 2)
```

Without the rule the two spellings disagreed for that receiver: `.add()` dispatched the user's method
(the miss-only intrinsic never shadows one) while `+` auto-flowed to `int`'s native op, so
`twice(Score(1), Score(2))` gave `99` and `Score(1) + Score(2)` gave `3`. A numeric newtype inherits
its underlying's operators; **use a `struct` if you need your own arithmetic.** For the *arithmetic and
ordering* names the rule is narrow — non-numeric and generic newtypes are unaffected, since they have
no such operator to disagree with.

**`eq` is the one name where that "no operator to disagree with" premise is false, so it is rejected on
EVERY newtype** — numeric, non-numeric, or generic. `==` is defined on **every** underlying, so a
`newtype Name = str` with an `eq` method would have `Name("a") == Name("b")` unwrap to `str`'s native
equality (`false`) while `Name("a").eq(Name("b"))` ran the method — the identical two-spellings-disagree
shape, one type-kind over:

```chezzi
newtype Name = str:
    fn eq(self, o: Name) -> bool:   # error: operator method 'eq' on a newtype is never dispatched
        return true                 # as an operator — a newtype's '==' always unwraps to str's …
```

Unlike a struct/enum — where a *generic* operand (`fn eq(self, x: T)`) marks the method as an ordinary
one and leaves `==` structural — **no** signature rescues `eq` on a newtype: there is no hook to tell it
apart from, because a newtype's `==` dispatches to no user method at all. Rename the method, or use a
`struct` (whose `eq` **does** own its `==`). This is deliberate divergence from Rust — where a tuple
struct may `impl PartialEq` and `==` uses it — and from Python's `__eq__` on a wrapper class: making a
newtype's `==` dispatch was implemented and rejected for `compare` (a numeric newtype's intrinsic grant
is unconditional, so a heterogeneous `List[Eq]`/`List[Comparable]` would take the user's answer for a
same-newtype pair and the native one for a newtype/underlying pair — equality/ordering that is not
transitive, with no fault).
`neg` is the one operator-named method a numeric newtype MAY still define, because unary `-` has no
newtype path at all (`-m` on a `newtype Meters = float` is already the error `cannot negate Meters`).
With no operator to disagree with, a `neg` method is simply the only spelling of negation available.

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

The scalar types **`int`/`float`/`bool`/`str` intrinsically satisfy `Stringable`** — all four
stringify, so they flow into a `[T: Stringable]` generic with no method to define (mirroring the
intrinsic scalar arms of `Comparable`/`Hashable`/`Add`). Inside the erased body a `v.str()` on such a
scalar renders exactly as `str(v)` does. (Like the other intrinsic protocols, this is bound-only:
a direct `(5).str()` on a concrete scalar is still a compile error — use the free `str(5)` builtin.)

**Every intrinsic grant is callable, and equals its operator form.** The same holds for *all* the
protocols a built-in satisfies intrinsically, not just `Stringable`: inside an erased `[T: P]` body (or
through a protocol-typed value like `x: Hashable = 5`) you may call `a.add(b)`/`a.sub(b)`/`a.mul(b)`/
`a.div(b)`/`a.mod(b)`/`a.neg()` on `int`/`float`/a numeric `newtype`, `a.compare(b)`, `x.hash()` on
`int`/`str`/`bytes`/`bool`/a zero-field struct, and `c.index(k)`/`c.set_index(k, v)`/`c.slice(s, e, st)`
on `list`/`map`/`str`/`bytes`/`bytearray`. Each is **defined as** the operator form — `a.add(b)` ≡
`a + b` (same overflow / divide-by-zero fault), `c.index(k)` ≡ `c[k]` (negative indexing and the same
out-of-bounds message), `c.slice(Some(0), Some(2), None)` ≡ `c[0:2]` (the three components are `int?`),
`x.hash()` is exactly the hash `x` gets as a map/set key. A type that defines the method itself always
gets its own. Still bound-only, and with two documented exceptions (`docs/gaps.md` W6-3b/d):
`Iterator`'s `next` on a *raw* collection faults (no cursor position — use `for`, or `.iter()` for a real
cursor); and a numeric `newtype` that DEFINES `add`/`compare`/… gets its own method from `a.add(b)` while
`a + b` keeps auto-flowing to the underlying's native op, so the two spellings disagree for that type
only. `a.compare(b)` on a **NaN** operand never faults — it answers the same **total order** `sort()` /
`.min()` / `.max()` use (`f64::total_cmp`, NaN to one end), while `<`/`<=`/`>`/`>=` stay IEEE (`false` for
every NaN comparison): one shared order, one rule.

**Display-hook resolution.** `print`/`str()`/interpolation use your `str` method as the display hook
**only when it conforms to `Stringable`** — a single `self` parameter and a **`str` return** (whether
that return type is written explicitly, inferred from the body, or a `str` type-alias). `str` is
otherwise a normal method you may define however you like: a `str` method with extra parameters, or
one that returns something other than `str` (e.g. the receiving struct), simply **isn't** the display
hook — those values fall back to the default repr, and a direct `obj.str(…)` call still works as
written. (This is why a `fn str(self) -> S: return self` prints `S(...)` rather than looping.)

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

**A nested `str` is quoted (`str` vs `repr`).** A string printed on its own is its bare characters,
but a string rendered **inside** something else — a list/tuple/map/set element, a struct field, an
enum payload — is quoted and escaped, exactly as in Python. Without it, values that differ print the
same, and printed output is what most tests compare.

```chezzi
print("a, b")                 # a, b
print(["a", "b"])             # ['a', 'b']       — two elements
print(["a, b"])               # ['a, b']         — one element (was identical before)
print([""])                   # ['']             — one empty element (printed as [] before)
print({"k": "v"})             # {'k': 'v'}
print(["it's", "a\nb"])       # ["it's", 'a\nb'] — CPython quote choice + escapes
print(Point(1, 2))            # (1, 2)           — a `str` hook's output is NOT quoted
```

The quote is `'`, switching to `"` only when the string contains a `'` and no `"`. Escapes: `\\`,
`\n`, `\t`, `\r`, the quote, and every **non-printable** character (`\xHH` / `\uXXXX` /
`\UXXXXXXXX`, CPython's widths) — so a no-break space is distinguishable from a space and a
zero-width space from `""`. Printable non-ASCII stays literal (`['é', '😀']`). The same rule applies
inside a wrapper box: `print(Shared(["a"]))` is `Shared(['a'])`. A `str(self)` display hook's result
is the object's own rendering, never a nested string, so it is never quoted.

(One deliberate deviation from CPython: a combining mark such as `U+0301` escapes here and prints
literally in Python — Chezzi reads printability from Rust's Unicode tables, which also treat
grapheme-extend characters as non-printable. Escaping is the unambiguous direction.)

The prebuilt **`Hashable`** protocol (`hash(self) -> int`) governs `map` keys and `set` elements:
`int`/`str`/`bool` satisfy it intrinsically, and a struct satisfies it by defining `hash(self) ->
int`. `map`/`set` are real insertion-ordered hash tables, so **any `Hashable` type can be a key or
element** — a struct key is hashed via its `hash()` and the probe confirmed by `==`, which is the
struct's own `eq` when it defines one (§`Eq`, above) and structural equality otherwise.
`float` is rejected (NaN footgun). Contract: two keys that `==` calls equal must return the same
`hash()` (the implementor owns this, like Rust's `Hash`/`Eq` — and see the `Eq` section for why an
`eq` coarser than its `hash` is structurally unreachable). `bytes` and a zero-field struct (no state
to hash) also satisfy it intrinsically. In an erased `[T: Hashable]` body `x.hash()` returns exactly the
hash the container uses, so the method and membership can never disagree; the numeric value itself is
**unspecified** (a build-dependent 64-bit hash, possibly negative) — rely on consistency, not on a
literal.

**`Hashable` alone is not enough to build, index, OR MENTION a `Map`/`Set` keyed on `T` — you need
`Hashable` AND `Eq`.** `Hashable` does **not** embed `Eq` (Rust's own `Hash` trait has no `Eq`
supertrait either — `HashSet<T>`/`HashMap<K, V>` spell `impl<T: Eq + Hash>` explicitly, not one
bound), so a `[T: Hashable]` generic that only *calls* `T.hash()` compiles fine, but the map/set key
check runs on every `Map[T, _]`/`Set[T]` TYPE the generic's signature or body spells at all — not just
one it *constructs or indexes*. A bare SIGNATURE mention is enough: `fn h[T: Hashable](xs: Set[T]) ->
int: return xs.len()` rejects (measured — Go 1.26 agrees: `invalid map key type T (missing comparable
constraint)`), with no `Set(...)`/`{...}`/`m[k]` anywhere in the body. Needs `[T: Hashable + Eq]` —
the map/set key check runs the SAME `Eq`-bound obligation on a type parameter that `==` does
(`docs/gaps.md` **W7-53**). A concrete key/element (`Set[Box[Tag]]`) was always checked this way; what
changed is that the check now also reaches a free `T` inside a generic body/signature instead of
skipping it as "not yet chosen".

**Keys are value types (Go model).** A `struct`/`enum`/`newtype` key or element is **snapshotted
(deep-copied) when it is stored**, so mutating your original value *after* the insert can never reach
— and corrupt — the stored key. This applies to every insert path: `m[k] = v`, the `{k: v}` and
`{a, b}` literals, `set.add`, `Map.update` / `Map.merge`, and the `Map(it)` / `Set(it)` constructors.
**Map values are *not* copied** (mutating a stored value in place is intended); the transient lookup
key in `m[k]` / `k in m` / `s.has(k)` is not copied either. Scalar keys (`int`/`str`/`bool`/`bytes`)
are already immutable value-copies, so they take no extra clone. The snapshot copies only the mutable
aggregate structure (nested structs/enums/lists/maps/sets); an embedded by-reference sub-value (a
closure, `Channel`, `Shared`, a live generator, …) stays shared by handle — those are identity-
compared, so copying them would break lookup, and they have no mutable field that could corrupt the
key anyway. One ceiling: a **self-referential (cyclic)** key — or one nested deeper than the
structural-depth cap (`MAX_STRUCTURAL_DEPTH`, 10000) — is stored *by reference* (a value-copy of a
cycle can't compare equal to the original, and a too-deep snapshot would alias its tail then miss on
lookup / overflow the host stack), so mutating it after insert can still corrupt the collection —
use shallow acyclic keys if you need the isolation.
One more residual: a key handed back by **`keys()` / iterating a `map`/`set`** is the *stored* key
by reference, not a fresh copy — mutating it in place (`for k in m.keys(): k.x = 9`) corrupts the
collection exactly as mutating a pre-insert original used to. The snapshot protects against mutating
*your* original after insert; it does not freeze a key you deliberately reach back into. Don't mutate
keys you get from iteration.

```chezzi
struct Point:
    x: int
    y: int
    fn hash(self) -> int:
        return self.x * 31 + self.y

label: Map[Point, str] = {}
label[Point(1, 2)] = "here"      # struct key — hashed via Point.hash
print(label[Point(1, 2)])        # here

p := Point(1, 2)
label[p] = "again"
p.x = 9                          # mutating the original can't touch the stored key
print(label[Point(1, 2)])        # still "again" — the stored key was snapshotted
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

A scrutinee can also be an **int/str/bool** (literal arms + a required `_` wildcard), a **tuple**, or
a **struct** (destructured positionally — see below). Patterns **nest**: a variant payload, tuple
element, or struct field may itself be a binding, a literal, a wildcard, a tuple, a struct, or another
variant — including a **nested nullary variant** like the `None` in `Some(None)` (a refutable variant
match, not a binding).

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

A **struct** scrutinee destructures by **positional field binding**, mirroring an enum-variant pattern
— the constructor name is the struct's own name, and each position binds the field in declaration
order. A struct has exactly **one** constructor, so a lone all-binding `Point(x, y)` arm is
**irrefutable** and closes the match with **no `_`** needed. Struct patterns **nest** (`Line(Point(x,
y), _)`), instantiate generics (`Box(v)` on a `Box[int]` binds `v: int`), and admit refutable literal
fields (`Point(0, y)` — which, like any refutable arm, needs a trailing `_` or a whole-value catch-all
binding):

```chezzi
struct Point:
    x: int
    y: int

match p:
    Point(x, y): "at {x},{y}"       # single all-binding arm — exhaustive, no `_`

match p:
    Point(0, 0): "origin"
    Point(0, y): "on the y axis"
    rest:        "at {rest.x},{rest.y}"   # a bare name binds the WHOLE struct value (catch-all)
```

The constructor may be written **bare** (`Point(x, y)`, for a local or `from`-imported struct) or
**module-qualified** (`geo.Point(x, y)` — the only spelling for a struct reached through a whole-module
`import geo`, since the bare name isn't in scope; this mirrors qualified construction `geo.Point(3, 4)`).
Only **user** structs destructure — a native/reserved struct handle (`Socket`, a `regex.Match`)
does not. A wrong constructor name, a field-count mismatch (`Point(x)` on a two-field struct), a qualifier
that is not a module (`E.Point`), and a duplicate constructor arm are all clean **checker** errors, never a
runtime panic. (`let`-destructuring of a struct — `let Point(x, y) = p` — and struct destructuring in
**fn params** are not yet supported; use a `match`.)

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
then `E.A(n)` — without a "duplicate arm" error.) An exact-duplicate **literal** arm (`1:` twice,
`"x":` twice, `1 | 1`) is likewise a `duplicate match arm` error — dead code under first-match — with
the same guard carve-out (`1 if c:` then `1:` is legal). (Range *subsumption* — a literal inside an
earlier covering range — is not yet flagged.)

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
# chained: `elif` chains without parentheses (the final `else` stays mandatory)
grade := if s >= 90: "A" elif s >= 80: "B" else: "F"
```

All arms (and both `if` branches) must agree on a type — with ONE numeric adaptation: an untyped **int
constant** branch beside a float **constant** sibling branch widens to `float` (`x := if c: 1 else: 2.5`
→ `float`; `match n: 0: 1; _: 2.5` → `float`), the exact `literal_numeric_mix` peephole the list literal
`[1, 2.5]` uses (the compiler emits `Op::CoerceFloat` on the int branch, so it is a real float, never an
`int` under a `float`). A **typed** int branch (a variable, a call) does NOT adapt — `a := 5; if c: a
else: 2.5` is a type error (write `float(a)`), same as a typed int element in a mixed list. This is a
property of the if/match EXPRESSION and is distinct from multi-`return` inference (which still conflicts
on `int`/`float` — annotate `-> float`). When every branch is an `Ok(…)` (no `Err`
branch pins the error type), an **unannotated** `if`/`match`-expression's `Result` error slot defaults
to the built-in `Error` protocol — `x := if c: Ok(1) else: Ok(2)` is `Result[int, Error]`, matching the
`T!`/`Result[T]` shorthand and return-type inference (it does not leak an un-pinned error type onto a
later `?`). An explicit annotation (`x: Result[int, DbErr] = …`) still wins. The statement forms — `match s:` /
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
propagated error type must fit the function's error type), an `Option`-`?` needs an `Option`-returning fn. **A
function must return `Result`/`Option` to use `?`** — there is **no `fn main`/entrypoint exception**; a
nil-returning fn (named or nested) that uses `?` is a compile error (the propagated `Err`/`None` would be
silently swallowed). Only **module top-level** code (outside any fn) accepts either kind — the runtime unwinds
the unhandled `Err`/`None` at the program boundary and exits (rc=1). A manifest `module:function` entrypoint may
therefore legitimately be `-> T!` and use `?`; if that entry fn returns `Err`/`None`, `chezzi run` surfaces it
as `unhandled error: <msg>` (rc=1), just like an unhandled top-level `Err`. Mixing kinds — e.g. a `Result`-`?`
inside an `Option`-returning fn — is a compile error. The **`?.` operator below is this same `?`** when
its operand is a `Result`, so every rule in this paragraph applies to it verbatim.

**Optional chaining `?.`** works on **both** carriers; the lowering is chosen by the operand's type:

```chezzi
name := user?.profile?.name ?? "anon"   # Option: None anywhere short-circuits, then ?? defaults
n    := fetch()?.len()                  # Result: propagate the Err (`?`), then `.len()` the value
```

**On an `Option[T]`** — `x?.field` / `x?.method(args)`: `None` short-circuits to `None`, `Some(v)`
applies the access to `v` and re-wraps, so the result is always an `Option` (a field that is itself
`Option` is **not** flattened: `Option[Option[U]]`).

**On a `Result[T, E]`** — `x?.field` / `x?.method(args)` means **`?` then `.`**: propagate the `Err`
out of the enclosing function, then apply the access to the unwrapped `T`. It is identical to the
spaced spelling `x? .field` — same value, same bytecode, same diagnostics — and is therefore subject
to the **same enclosing-function return-kind rule as `?`**: a `Result`-`?.` needs a `Result`-returning
fn (module top-level accepts either kind). Every other rule `?` carries — how it behaves inside
`defer:` and under `recover:`, and that it is rejected inside a `spawn:` block — applies to it
unchanged, because it *is* a `?`. `f()?.len()` is Rust's
own idiom and compiles here for the same reason. There is no longer a whitespace cliff: `f()?.len()`
and `f()? .len()` are the same program.

Because the `Result` form is try-then-**dot**, not a chain of tries, it does **not** auto-try through
a nested carrier: with `a: Option[X]` and `a.b: Result[Y, E]`, `a?.b?.c` is an `Option[Result[Y, E]]`
followed by a field access on a `Result` — an error, and correctly so.

**Null-coalescing `??` is `Option`-only.** `a ?? b` returns `a`'s inner value if `Some`, else `b`; it
is **right-associative** (`a ?? b ?? c` = `a ?? (b ?? c)`). It is deliberately *not* extended to
`Result`: a `Result` carries an error payload `??` would silently discard, and no ancestor offers the
combination (Rust has no coalescing operator; Swift/Kotlin/C#'s is Optional-only, in languages with no
`Result`). `??` on a `Result` is one error pointing at `match`:

```
'??' applies to an Option, found Result[str, str] — a Result carries an error that must be handled:
use a match with Ok/Err arms
```

**Migration note.** `f()?.len() ?? 0` on a `Result` is now an error on the **`??`**: `f()?.len()` is
already an `int`, and `??` takes an `Option`. Drop the `?? 0` (the `?` propagates), or `match` the
`Result` if you want a fallback instead of propagation.

Both operators require the two chars **adjacent** (`x?.f`, `a ?? b`); on a **non-carrier** operand
`?.` is one error, `'?.' applies to an Option or a Result, found int`.

**Unhandled errors at the top level exit the program.** An `Err`/`None` that reaches the top level —
a bare top-level expression statement that evaluates to one (e.g. `compute()` whose result is `Err`),
or a top-level `?` that hits one — terminates the program with `unhandled error: <detail>` and a
non-zero exit code. *Binding* the value handles it (`r := compute()` keeps running; inspect `r`).

**The SAME discarded call inside a function is silently swallowed at runtime** — the asymmetry filed
as `docs/gaps.md` **W8-2**. The asymmetry is **real and justified**: the top-level check *is* the
handling, so nothing is lost there. Where there is no such check the value vanishes without a trace,
and that is exactly where `chezzi check` **warns**, following Rust (which marks both carriers
`#[must_use]` and warns on the drop):

```chezzi
fn g() -> Result[int, Error]: return Err("E")
g()                    # NO warning — the runtime checks it: `unhandled error: E`, rc=1
fn f():
    g()                # warning … the Result returned by 'g' is discarded, and rc stays 0
f()
```

```
warning (line 4, col 5): the Result returned by 'g' is discarded — bind it (`r := g()`), or discard it explicitly (`_ := g()`)
```

A warning is **non-fatal**: the program still type-checks and the exit code is unchanged. The escapes
are Rust's — *bind* the value (`r := g()`, then inspect it) or *discard it explicitly* (`_ := g()`),
which puts the intent on the page. (The hint spells the call back only when the callee is a plain
name; for a method call — `xs.pop()` — it stays elided as `r := …`.)

> ⚠️ **`_ := g()` at the top level DISABLES the runtime check.** The check runs on a bare expression
> statement; binding the value — to `_` or to anything else — is the language taking your word that
> you have handled it. So `_ := main()` at the top of a script turns a failing `main` from `unhandled
> error: …` + rc=1 into a silent rc=0. Write the bare call, or `match` it.

**Which positions warn.** A `spawn:` block, a `defer:` block and a function body each compile to their
own frame, so a drop inside one is invisible to the top-level check and warns — even when the block
itself sits at module top level. Every other block — `if`, `for`, `while`, a `match` arm, `parallel:`,
`recover:`, a `wait:` arm — runs in the *enclosing* frame, so at top level the value is still checked
and nothing warns (inside a `recover:` the resulting abort is caught and surfaces as `r = Err(…)`).

| statement at module top level | runtime | warns |
|---|---|---|
| `g()`, or nested in `if` / `for` / `while` / `match` / `parallel:` / `wait:` | aborts, rc=1 | no |
| nested in `recover:` | caught → `r = Err('unhandled error: …')` | no |
| inside a `spawn:` block or a `defer:` block | silently swallowed | **yes** |
| anywhere inside a `fn` body | silently swallowed | **yes** |
| `defer g()` / `spawn g()` — the **call** forms, in any position | silently swallowed | no — *deliberate*, see below |
| the drop happens on a value typed by a **type parameter** (`fn drop_it[T](x: T): x`) | silently swallowed | no — *a known limit*, see below |

The warning fires wherever the statement's own type is a carrier, so it also skips the positions where
a bare carrier expression isn't a drop at all: an inline-expr body (`fn f() -> T!: g()`, an implicit
return), the trailing expression of a `recover:` block or a value-`match`/value-`if` (that expression
*is* the block's value), and `g()?` / `x ?? d` (which yield the unwrapped payload). `o()?.len()` *does*
warn — optional chaining re-wraps, so the result is still an `Option`.

**`defer` is deliberately excluded.** `defer f.close()` never warns even though `close` returns a
`Result`: `defer f.Close()` is Go's canonical unchecked idiom and the ancestor for the statement. Bind
it inside a wrapper function if you do want the error. The **call form** of `spawn` (`spawn g()`) is
excluded for the same reason — a spawned task's return value is discarded by construction. Both are
real silent swallows; both stay silent on purpose. (The **block** forms — `defer:` / `spawn:` — do
warn: their bodies are ordinary statements, not the fire-and-forget call.)

**A carrier laundered through a type parameter escapes the rule.** The warning fires on the
*statement's own type*, so a generic that swallows its argument is invisible to it:

```chezzi
fn g() -> Result[int, Error]: return Err("E")
fn drop_it[T](x: T):
    x                  # type is `T`, not a carrier — NO warning
drop_it(g())           # prints "after", rc=0; the Err is gone
print("after")
```

This matches Rust exactly — a `T` carries no `#[must_use]`, so `fn drop_it<T>(x: T) { x; }` is silent
there too. It is a known limit of the rule, not a defect.

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

The block's value is its **trailing expression**. A trailing statement-form `match`/`if` counts too:
when every arm/branch produces a value (a total `match`; an `if` with an `else`, every branch ending
in a value), the whole construct is the block's value expression and `Ok` wraps its unified arm/branch
type — so `recover: … ; match x: 3: 100; _: 200` is `Result[int]`, not `Result[nil]`. A tail that
does *not* uniformly produce a value has no single value type, so the block falls back to `Result[nil]`
(value dropped, consumed only via `Ok(_)`) — never an error. This covers a trailing `let`, a non-total
`match`, an `else`-less `if`, **and** a `match`/`if` whose arms produce genuinely *different* types (a
`str` arm next to an `int` arm, or a void `print(...)` arm mixed with a value arm). (A tail that provably
*diverges* — every arm `panic`s — is bottom, matching a direct `recover: panic(…)`.)

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
exactly like an integer overflow. `defer`s run as it unwinds, like any panic — and if one of those
`defer`s **itself** panics while the unwind is in flight, the newer panic **replaces** the one in
progress (the later panic wins; the original message is dropped, matching the last-writer semantics of a
re-raise). Because `panic` never
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
closure, or a name bound to one). The four **universe builtin functions** `print`, `ord`, `chr`, and
`panic` are first-class values, so `defer print("done")` (etc.) works directly — and they can be
bound and passed like any function (`f := ord; f("a")`, a HOF arg). **Type / container / runtime
constructors** (`int`, `str`, `List`, `Map`, `Channel`, `range`, …) and user struct/enum constructors
are **not** first-class values — wrap them: `fn log(m: str): print(m)` then `defer log("done")`.
That constructor rule reaches **every dotted spelling of a constructor**, since they all merely build
a value and throw it away: a variant constructor (`defer E.A(3)`, `defer E[int].A(3)`,
`defer lib.Col.Val(3)`), a module-qualified struct/newtype constructor (`defer lib.Pt(3)`), and a
**native constructor reached through its std module** (`defer concurrency.Shared(0)` — likewise
`RwShared`/`Atomic`/`AtomicInt`/`Executor` — and `defer time.timer(10)`, aliased imports included).
All are rejected by `chezzi check` with the same message as the bare `defer Shared(0)`. An ordinary
**module function** is *not* a constructor and stays a legal target — and a **module name is a
NAMESPACE, not a receiver value**, so `mod.f(x)` is a plain call in `spawn` position exactly as it is
in `defer` position: `defer math.abs(-3)` and `spawn math.abs(-3)`, `defer lib.helper(3)` and
`spawn lib.helper(3)`, aliased imports and witness-taking generics (`spawn lib.reset(c)`) included,
all compile and run (Go accepts `go pkg.F(x)` and `defer pkg.F(x)` alike). Nothing module-shaped
crosses the airlock — the call is replayed inside the task — so the spawn **sendability** rules are
unchanged: a genuine non-sendable *receiver* (`spawn v.m()`, including a local that merely **shadows**
a module name, and one **bound to** it — `m := math` then `spawn m.abs(-3)`) and a non-sendable
*argument* are still rejected by `chezzi check`. The namespace rule is keyed on an **unbound module
name**, the same clause the compiler lowers on, so `check` and `run` cannot disagree.
A **static method** is *not* a constructor either and *is* an ordinary target, in every spelling
that works as a call — `defer Holder.build(3)`, `defer Gen[int].build(3)`, `defer T.default()` inside
a generic, `defer lib.Holder.build(3)`, and a `from`-imported head. Arguments are evaluated **eagerly at
the statement**, like every other `defer`/`spawn` argument (and like Go's `defer pkg.F(x)`), and
`defer` keeps its LIFO order. Both spawn too.
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
restriction inside the block) the body is ordinary statements — built-ins are fine. The block runs
in the **same task** and **captures its free variables by reference** (like any closure), so it sees
their **latest** values when it runs at scope exit — `x := 1; defer: print(x); x = 99` prints `99` —
and **reassigning** an enclosing local inside the block mutates the shared binding. (This differs from
the call form `defer f(x)`, whose *arguments* are still evaluated eagerly at the `defer` point.) One
rule remains: a `?` short-circuit inside the block is **discarded** (a cleanup body has no
error-return contract, like a deferred call whose `Err` result is dropped). A **`return` inside a
`defer:` block is a compile error** (`'return' is not allowed inside a defer block`) — the block is
its own closure and Chezzi has no named return values, so it could never affect the enclosing
function's result. This covers a `return` anywhere in the block, including inside an `if`/`for`/
`match`/`wait:` arm in it; a `return` inside a nested `fn` *declared* in the block is fine (it
returns from that `fn`).

```chezzi
fn handle(conn: Conn):
    x := 1
    defer:                        # both lines run at scope exit, top-to-bottom
        log("closing")
        conn.close()
    defer:
        log("x = {x}")            # prints "x = 2" — captured by reference, read at exit
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
constructing the suite instance once. Tests run on the **M:N OS-thread engine** (like `chezzi run`);
`CHEZZI_THREADS` sizes its worker pool. Known
limit: an assert that faults inside *imported* (non-test) code reports the test file's path, not the
library file's.

The dedicated native suite lives in **`tests/chz/`** (`spec/` for language behavior, `stdlib/` for
module behavior, `suites/` for lifecycle-hook suites) — kept separate from `examples/` (print-and-golden
demos). It runs via the `cargo test` gate `chz_suite_passes` (`tests/chz_suite.rs`, its own process),
which runs the whole `tests/chz/` suite and asserts every test passes;
`tests/chezzi_threads_cli.rs` then runs it again at `CHEZZI_THREADS=2`. A fault's *message*
**can** be asserted in-language via `recover:` — `r := recover: <expr>` then
`match r: Err(e): assert e.message().contains(...)` — so fault-path tests port here too; only
compile-time checker diagnostics (`rejects`/`ok`) and engine internals (AST/bytecode/GC, scheduler
timing) stay in Rust.

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

> **Manifest mode cannot forward program arguments — this blocks the CLI use case.** Program args are
> only recognised *after* a file path, and there is **no `--` terminator**, so from a scaffolded project:
>
> ```
> $ chezzi run --dir logs      → chezzi run: unknown flag '--dir'
> $ chezzi run -- --dir logs   → chezzi run: unknown flag '--'
> ```
>
> `go run . --dir x`, `python -m pkg --dir x` and `cargo run -- --dir x` all forward. Until this is
> fixed (`docs/gaps.md` **W8-10**), a CLI must be run in the **file** form — `chezzi run src/main.chz
> --dir logs` — which runs the top level only, so drop the `:main` suffix from `entrypoint` and end
> `src/main.chz` with an explicit `main()` call. Note the trap that pairs with this: the file form of a
> `:main` project is a **silent no-op with `rc=0`** (top level runs, `main` is never called).

## 10. Strings & interpolation

```chezzi
name := "chezzi"
age := 30
print("hi {name}, age {age}")     # {expr} interpolates
print("sum: {a + b}")             # any expression
print("brace: {{not interpolated}}")   # '{{' / '}}' = literal braces
```

> **Interpolation is ALWAYS on, which silently eats regex quantifiers.** `"\\d{4}-\\d{2}"` is not the
> pattern you typed — `{4}` and `{2}` are interpolation holes, so the string is `\d4-\d2`. That is still
> a *valid* regex (a digit then a literal `4`), so it compiles, matches nothing, and reports nothing:
>
> ```chezzi
> print("\\d{4}-\\d{2}")     # \d4-\d2      (python3: \d{4}-\d{2})
> print(r"\d{4}-\d{2}")      # \d{4}-\d{2}  ← use this
> ```
>
> **Use a raw string `r"…"` for anything containing `{n}`** — regexes above all, but also format
> templates and JSON fragments. Doubling the brace (`"\\d{{4}}"`) works too but reads worse.
> (`docs/gaps.md` **W8-1**.)

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

**What goes inside `{…}`.** Any expression, with no second-class rules — a call in a fragment takes
**named arguments, defaults, and variadics** exactly like the same call written outside the string
(fragments are parsed with the rest of the module, before call arguments are normalized). The
fragment scanner is **quote- and depth-aware**, so a `}` inside a nested string literal or inside
`(`/`[`/`{` belongs to the expression rather than closing the fragment; whitespace padding around the
fragment is insignificant.

```chezzi
fn greet(name: str, greeting: str = "hi") -> str:
    return "{greeting}, {name}"

print("{greet('ada')}")              # hi, ada          ← default applies inside a string
print("{greet(greeting='yo', name='ada')}")   # yo, ada ← named args, any order
```

```chezzi
print("{ {1, 2}.len() }")     # 2          ← set literal's brace is nested, and padding is fine
print("{ {'a': 1}['a'] }")    # 1
print("{d['a}}b']}")          # value at key `a}b` — the quoted brace does not close the fragment
```

Past the top-level `:` the rest of the fragment is the **format spec** — literal text, not an
expression — so any character may be the fill (`"{x:'>5}"` → `''''7`, `"{x:(>5}"` → `((((7`).

Two limits, both shared with CPython < 3.12. The **lexer** ends the string at the first unescaped
delimiter, so a fragment cannot nest the *same* quote style (`"{d["k"]}"` is a lex error — use
`"{d['k']}"`). And a nested literal is itself a normal Chezzi string, so it interpolates too and a
literal brace inside it is still doubled (`'a}}b'` above is the key `a}b`).

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
print("{12345.678:.2e}")  # scientific (signed 2-digit exp) → "1.23e+04"
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
- **type**: `d` int · `f` fixed float · `x`/`X` hex · `b` binary · `o` octal · `e`/`E` scientific
  (default precision 6, exponent always signed and zero-padded to ≥2 digits, e.g. `1.234568e+05`) ·
  `%` percent (×100 then `%`). A float type char (`f`/`e`/`E`/`%`) promotes an int.

A **bare** `{expr}` (or `{expr:}` with an empty spec) renders a whole float with a trailing `.0` —
e.g. `5.0`. An **unknown type char** or trailing junk in the spec is a **parse error**. A
**type/value mismatch** (e.g. `{name:d}` on a string, `{x:.2f}` on a string, `{x:d}` on a float,
`{x:.3d}` precision on an int, zero-pad on a non-number) is now **caught at compile time by `chezzi
check`** whenever the value's static type is a **concrete scalar** (`int`/`float`/`str`/`bool`) — a
provably-wrong spec/type pairing is a static error, in the spirit of Chezzi's statically-typed model
(this is a **deliberate divergence from Python**, where such a mismatch is a runtime `ValueError`).
The **runtime** validation stays as an identical backstop (same wording, single-sourced in
`spec_valid_for_scalar`): it still fires for a value whose type the checker can't pin to a concrete
scalar — a generic `fn show[T](v: T): "{v:.2f}"` instantiated with a `str`, an `Unknown`, or a
protocol existential — where the mismatch is only knowable at run time. The spec is parsed once,
one module (`src/fmtspec.rs`), so its output is byte-identical across runs. The `:` split is bracket/quote-aware — a `:` inside an index, string key, or
slice (`{m["a:b"]}`, `{xs[1:2]}`) is *not* the spec separator. **Ternaries:** a bare interpolated
ternary `{if b: a else: b}` works (its colons are part of the expression, not a spec); to attach a
spec to a ternary, **parenthesize** it — `{(if b: 1 else: 2):>5}`.

**Plain float formatting matches CPython `repr()`/`str()` exactly, with ONE exception: `NaN` casing.**
Chezzi prints `NaN` where CPython prints `nan` (`inf`/`-inf` agree). It is the single differing byte
across the whole float surface, and it differs on **every** path — bare `print`, `str()`,
interpolation, container element, `{x:.2f}`, `{x:>8}`. Test against `NaN`, or compare with
`math.is_nan` instead of a string. (`docs/gaps.md` **W8-18**.) A bare float — `print(x)`,
`str(x)`, or a `{x}` interpolation with no spec — uses **scientific notation when the decimal
exponent is `< -4` or `>= 16`**, and fixed-point otherwise. So `1e16` prints `1e+16`, `1e15` prints
`1000000000000000.0`, `0.00001` prints `1e-05`, `1.5e300` prints `1.5e+300`, and `-2.5e-8` prints
`-2.5e-08`; whole floats inside the fixed range keep their trailing `.0` (`1.0`, `1000.0`). The
mantissa is **shortest-round-trip-correct** (the fewest digits that parse back to the same `f64`) and
the exponent always carries an explicit sign, zero-padded to ≥2 digits — byte-identical to Python's
`repr`. When the two shortest candidates are *exactly* equidistant from the value, the tie breaks
**to even** like CPython, not away from zero: `print(771.5462036132812)` is `771.5462036132812`, not
`…813` (the exact value is `771.54620361328125`) — `docs/gaps.md` §W7-32. `json.stringify` routes through the same formatter, so a large float serializes as
`1.5e+300` (valid JSON, round-trips through `json.parse`), not a 300-digit expansion.

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
s.parse_int()    s.parse_float()  # → Result[int,str] / Result[float,str] (Ok/Err(msg) on bad input)
"a" + "b"        # concatenation
```

The `ends_with`/`replace`/`repeat`/`reverse`/`pad_left`/`index_of`/`count`/`strip_prefix`/
`strip_suffix`/`split_lines` methods forward to the matching `std.string` free fns (no import needed).

A character is just a 1-char `str` (Python-style — there is no `char` type): index with `s[i]`,
iterate with `for c in s:` or `s.chars()`, and bridge to codepoints with `ord`/`chr`.

List methods (built in): `xs.push(x)` `xs.pop()` `xs.len()` `xs.reverse()` `xs.contains(v)`
`xs.index_of(v)` `xs.sum()` (numeric, or a scalar numeric `newtype` → that newtype; empty `-> T(0)`)
`xs.sort()` (ascending, in place); `xs.concat(ys)→list` (new list) and
`xs.extend(ys)` (append in place, → nil); higher-order `xs.map(f)` `xs.filter(p)` `xs.fold(init, f)`;
`xs.sort_by(fn(a, b) -> int)` — a custom comparator (negative = `a` before `b`), stable, in place;
and `xs.sort_by_key(fn(x) -> K)` — sort by a derived key (`K` Comparable: int/float/str, or a struct
defining `compare`), stable, in place.

> **Empty-collection element typing (refine-on-first-use).** An un-annotated empty `[]` / `{}` /
> `Set()` has no element/key type yet; the **first** mutating op on the binding — `.push`/`.add`/
> `.insert`/`.extend`, or `m[k]=v` — **pins** the element/key/value type, and later ops are checked
> against that pinned type. So `out := []; out.push(1)` is `List[int]` and a later `out.push("s")` is a
> type error (it would read as `List[int]`). A **heterogeneous / protocol** collection therefore needs
> an explicit annotation — `shapes: List[Shape] = [circle, square]` (or `shapes: List[Any] = [1, "a",
> true]` for the top type). The annotation is **expected-type-directed**: the declared element type is
> driven onto each element, so a literal whose elements have differing concrete types is accepted as long
> as **every** element is assignable to the declared element type (each satisfies the protocol / `Any`);
> only when some element does *not* fit the declared type does the usual `list elements differ` error
> fire. (An `= []` empty binding plus later `.push` also works and is equally valid.)
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
>
> Because the pin is **scope-wide** (not source-order forward), a **non-pinning** use of the binding
> that appears *before* the pinning op still sees the resolved element type — the checker resolves the
> binding's element type from the whole scope, then checks each use against it. So `a := []; a.sort();
> a.push(1)` type-checks (`a` is `List[int]`: the later `push` pins it, and the earlier `sort` is
> checked against that pinned `int`). This is what makes **bound-checked methods** compose with
> refinement: `sort`'s `where T: Comparable` and `sum`'s `where T: Add` — and a user **conditional
> method**'s `where` on the receiver type param — enforce against the *resolved* element/type
> argument, never a transient `Unknown`. A genuinely never-pinned empty still fails at the binding
> with `cannot infer element type of empty collection` (above) — not with a spurious `does not satisfy
> Comparable`/`Add`.

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

**Line continuation:** a line whose **first** token is `|>` continues the previous line, so a chain
can be written one step per line with no parentheses. A **trailing** `|>` (a line *ending* in `|>`)
is **not** a continuation — it is a parse error. Only the exact `|>` continues (`|`, `||`, `|=` do not).

The **offside rule still binds**: the `|>` line must be indented **at least as deep** as the block it
continues. A `|>` line *shallower* than the open block closes that block, and is then a parse error —
it is never absorbed back into the body it sits outside of:

```chezzi
fn f() -> int:
    r := 1
    |> dbl()       # OK — same indent as `r := 1`, and deeper is fine too
    return r
|> dbl()           # error: unexpected '|>' — column 0 is outside f's body
```

```chezzi
import std.iter

total := [1, 2, 3, 4]
    |> iter.filter(fn(x: int) -> bool: x % 2 == 0)   # → iter.filter([1,2,3,4], ...)
    |> iter.map(fn(x: int) -> int: x * 10)
    |> iter.sum()
print(total)                                         # 60

# equivalent without pipe:
# iter.sum(iter.map(iter.filter([1,2,3,4], fn(x: int) -> bool: x % 2 == 0), fn(x: int) -> int: x * 10))
```

(The pipe's right side must be a free **call**, so a method like `xs.sum()` is not reachable from
`|>` — that's why `std.iter` carries free-function forms.)

## 11b. Concurrency — `spawn` / `parallel:`  (see [`concurrency.md`](concurrency.md))

> **Implemented — shipped through Tier-D.** `chezzi run` defaults to the real OS-thread M:N scheduler
> (`Channel`/`Shared`/`Executor`, netpoller-backed `std.net`); size its worker pool with
> `--threads=N` / `CHEZZI_THREADS` (`0` = all cores). It is the only engine — the cooperative
> `--serial` VM was removed 2026-08-16. Full
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
  proceeds. `spawn` returns immediately (the parent continues) and the task **starts there**, running
  beside the rest of the body — Go's `go f()`. The barrier guarantees *completion*, not start.
- **Every function body (and the module top level) is an implicit nursery** (M-C) — a bare `spawn`
  is legal anywhere and joins at the body's `return`/end (the module top level joins at program exit).
  `return`, fall-through, and `?` are all join points (tasks run, *then* control leaves); `defer`s run
  after the join. An explicit `parallel:` is an *inner* sub-nursery for an earlier join. A `spawn`
  always binds to a nursery **in its own function** — a task can't outlive the function that spawned
  it (the function-boundary rule).
- **`return` inside a `spawn:` block is a compile error** (`'return' is not allowed inside a spawn
  block`) — a spawned task runs on its own, so there is nothing for it to return to (Chezzi has no
  named return values). Send the value on a `Channel`/`Shared` instead. The error names the block the
  `return` is *lexically* in (a `spawn:` inside a `defer:` reports "spawn block", once), and covers a
  `return` nested in an `if`/`for`/`match`/`wait:` arm inside the block. A `return` in a `parallel:`
  body is fine (it runs in the parent frame), as is one inside a nested `fn` declared in a `spawn:`
  block.
- **`?` (and `?.`) inside a `spawn:` block is a compile error** for the same reason (`'?' is not
  allowed inside a spawn block: a spawned task has no caller to propagate to`) — a `?` *is* a return,
  and the nursery discards a task's `Err` by design, so the propagation would be silently swallowed.
  This holds whatever the enclosing function returns, and whether or not the `spawn:` sits inside a
  `recover:` or a `defer:` — those boundaries stop at the task. Send the error on a
  `Channel`/`Shared`, or `match` it inside the task. Legal next door: a `?` in a `parallel:` body
  (parent frame), one in a nested `fn`/closure *declared* inside the task (it has its own caller),
  one in a `defer:`/`recover:` *inside* the task (same frame), and one in an argument to the call
  form `spawn f(g()?)` (arguments evaluate in the parent, before the task starts).

```chezzi
fn fetch_all(urls: List[str]):
    for u in urls:
        spawn fetch(u)        # no `parallel:` needed — joins when fetch_all returns
    print("dispatched")       # the tasks are already running (Go's `go f()`); they JOIN at
                              # end-of-function, so print order against them is undefined
```
- **`Channel[T]`** — a mailbox (buffered FIFO): `ch.send(v)`, `ch.recv() -> T`,
  `ch.try_recv() -> T?` (non-blocking poll — `Some(v)`/`None`, never blocks or faults), `ch.len()`,
  `ch.close()`, `ch.try_send(v) -> bool` (safe `send` — `false` if closed, never faults). After
  `close()`: `send` faults, `recv` drains then faults, `try_send` returns `false`. Drain a channel to
  completion with **`for v in ch:`** — it blocks per value and ends cleanly once closed-and-drained
  (Go's `for v := range ch`). Values **move/copy** across the boundary; the sender can't reuse a sent
  value.
- **`Shared[T]`** (`import std.concurrency`) — the cross-task mutable box: `s.get()` / `s.set(v)`
  / `s.update(fn(x): ...)`, synchronized and **sendable**. For an in-task mutable value to close over
  or mutate through, use a plain one-field `struct` (a struct is a shared reference). The mutation
  ladder is `value` (copied) → a mutable `struct`/collection (in-task) → `Shared[T]` (cross-task).
  `Shared`/`RwShared`/`Atomic`/`AtomicInt`/`Executor` require `import std.concurrency` (whole-module
  licenses all; `import Shared from std.concurrency` per-name) — they are NOT global builtins. They stay
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
- **`AtomicInt`** — the monomorphic **lock-free** int atomic (Rust `AtomicI64` / Java `AtomicInteger` /
  Go `atomic.Int64` style). `Atomic[T]`'s int-only sibling with no `[T]`, so it is a genuine lock-free
  `AtomicI64` (not a `Mutex`). Same method surface, all int-typed, `add`/`sub` always available (int is
  always numeric) and overflow-checked. Constructed `AtomicInt(v)` (one int arg). ~2.7× faster than
  Mutex-backed `Atomic` on a contended int counter — reach for it for a hot cross-task counter/flag.
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
  at declaration (a `struct int` / `struct List` is rejected `type 'X' is reserved (builtin)`). The 21
  prebuilt PROTOCOL names (`Any`/`Comparable`/`Eq`/`Stringable`/`Hashable`/`Error`/`Add`/`Sub`/`Mul`/`Div`/`Mod`/
  `Neg`/`Arithmetic`/`Iterator`/`Iterable`/`Index`/`IndexSet`/`Slice`/`Convert`/`Contains`/`PathLike`) are reserved the same way — usable
  as a bound (`[T: Comparable]`) but not as a `struct`/`enum`/`newtype`/`type` decl name (a user
  `protocol Comparable:` is likewise rejected `reserved (builtin)`). Their SHAPE (method sigs + embeds)
  is file-backed in `std/prelude.chz` as plain `protocol` decls (phase 5c) — a drift-guarded mirror of
  the Rust seed — but protocol CONFORMANCE (`int`/`float` satisfying `Add`/`Comparable`/`Neg` intrinsically
  with no method; `Iterator` via `iter_elem`; structural satisfaction for user structs) and OPERATOR
  BINDING (`+`→`add`, `<`→`compare`, `for`→`Iterator`, `[]`→`Index`, `[:]`→`Slice`) stay Rust-wired.
  (All 21 are file-backed — the 20 above plus `Any` (the top type, reserved the same way): `Any` is
  `protocol Any:` + `pass` (empty), and
  `Iterable`'s `iter(self) -> Iterator[Elem]` return type resolves to the same `Iterator[T]` value type the
  Rust seed uses, so their shapes mirror cleanly like the rest.) `Convert[S]` (`fn convert(x: S) -> Self`,
  a STATIC method — the extensible type-conversion protocol, Rust `From`) is **complete**: reserved,
  bound-only (`[T: Convert[str]]`; a value annotation is rejected — a value can't invoke a static
  ctor), structurally witnessed static-slot-aware, and callable through the bound
  (`fn make[T: Convert[int]](seed: T, n: int) -> T: return T.convert(n)`) via M24's witness passing —
  see §7a "Static protocol requirements".
  `PathLike` (`fn as_path(self) -> bytes`, W7-8) is the **path INPUT** position of the whole std
  filesystem surface: `str`/`bytes`/`bytearray` satisfy it intrinsically and `path.Path` structurally,
  so `fs.exists("x")`, `fs.exists(b"x")` and `fs.exists(p)` all work with no annotation. It is what
  lets a non-UTF-8 filename reach a syscall byte-exactly instead of through a `str` that cannot
  represent it — see `docs/stdlib.md` §`std.fs` / `path.Path`.
- **`wait:` (select)** — race several channel `recv`s AND `send`s; the first ready arm wins (deterministic
  source-order priority, not Go's random fairness). `wait:` then arms: recv `v := ch.recv():` (or
  `result = ch.recv():` / `_ := ch.recv():`), **send** `ch.send(v):` (a bare `.send()`, binds nothing —
  ready when the channel can accept the value: bounded-with-space / unbounded / closed→faults), an optional
  non-blocking `else:` (must be last), and `timer` arms for timeouts. A closed+empty *recv*-arm is skipped;
  a closed *send*-arm faults `"send on a closed channel"`; all-closed + no ready send + no `else` faults.
  **Shipped**, including the multi-channel blocking park and a bounded send-arm that parks until a
  receiver frees a slot. See
  [`concurrency.md §6d`](concurrency.md), `examples/wait_select.chz`, and `examples/wait_send.chz`.
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
- **Sendability:** a captured local — AND every module global — crosses into a task as an **independent
  per-task copy** (writes in the task stay local; a module global is deep-copied at the
  spawn boundary just like a captured local, so reassigning or in-place-mutating either inside a task is
  fine and simply invisible to the parent). Sendable types
  (scalars/str/containers+structs of sendable/`Channel`/`Atomic`/`AtomicInt`/`Shared`/`RwShared`/a `std.cancel`
  `Token`/closures/**protocol existentials** — Task 2, Go `chan interface` parity) cross the airlock;
  a native handle (or a witness carrying an FFI/native handle) does not. To share mutable state across tasks use a
  `Shared`/`Atomic`/`Channel` (they cross by shared handle, so a task-side write IS visible to the parent).

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

**`import` is TOP-LEVEL only.** An `import` inside a function body or any nested block is a **parse
error** (`import must be a top-level declaration`) — like `extern`/`native`. (It used to parse and
check clean while being a complete no-op: the resolver only scans module-level statements, so a nested
import never resolved, never bound, and never ran the module body.)

**A module's bound name may not be a reserved builtin.** The bound name — the alias, or the last path
segment when un-aliased — lands in the VALUE namespace, where it would beat the builtin of the same
name in expression position. So a reserved bound name is **rejected**:

```chezzi
import lib.int              # error: module name 'int' is reserved (builtin) — alias it: import lib.int as ints
import lib.geo as Ok        # error: import alias 'Ok' is reserved (builtin)
import lib.int as ints      # ok — and `int("5")` keeps working
```

The same rule covers a `from` import, which binds into the same namespace — a module global or
function whose *bound* name is reserved (aliased or not) is rejected; alias it:

```chezzi
import str from lib.sh          # error: imported name 'str' is reserved (builtin) — alias it
import str as s from lib.sh     # ok — and `str(5)` keeps working
import Shared from std.concurrency   # ok — a reserved TYPE member licensing the builtin itself
```

The reserved set is the builtin callables + reserved type names + `nil` + the builtin variant ctors
(`Ok`/`Err`/`Some`/`None`). (The std string module is `std.string` for exactly this reason: `str` is a
reserved scalar/ctor name.) A collision with a *user-declared* type of the same name is not covered by
this rule — name your modules and your types apart.

**The named-import form is `import X from M`, not Python's `from M import X`** — the module path comes
*last*, so every import statement starts with the `import` keyword (`from` at statement start is a
parse error: *unexpected 'from' in expression*). Semantics are Python's; only the word order differs.

**`import X from M` is a SNAPSHOT** (Python-identical): the value is copied into this module at import
time. A later write to the module's own global (`M.bump()`) is **not** visible through the bare name —
read `M.COUNT` for the live value. A **container** is the same heap object, so mutating *through* the
binding works; **rebinding** it is rejected (consistent with the qualified form, where `st.COUNT = 5`
already errors):

```chezzi
import COUNT, LST from lib.st
LST.push(7)     # ok — same heap object as lib.st's LST
COUNT = 99      # error: cannot assign to 'COUNT' imported from module 'lib.st' (a from-imported
                #        global is a snapshot copy — call a mutator fn in that module, or use a
                #        Shared). Writing through the module (`st.COUNT = 5`) is rejected too:
                #        a module global is writable only from inside its own module.
COUNT := 99     # ok — a fresh binding this module owns; `COUNT = 100` after it is fine too
```

**A module global that HOLDS a function is callable through the module** — `m.G()`, not just
`g := m.G` then `g()` (CPython and Go both accept the direct call). A non-callable member says so:
`module 'm' member 'N' is not callable (it has type int)`.

```chezzi
# l.chz
import k
BARE := k.one     # a global whose TYPE is a fn
# main.chz
import l
print(l.BARE())   # ok — calls through the module member
```

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

A **module-qualified generic fn** takes the member-level turbofish and honors the expected-type hint,
so a type param that appears only in the return type is reachable through `M.f`:

```chezzi
import lib.geo                       # fn empty_list[T]() -> List[T]
xs := geo.empty_list[int]()          # turbofish
ys: List[str] = geo.empty_list()     # …or solved from the annotation
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
`Ok`, `Iterator`, the std type surface on `import std.*`, FFI widths) stay global/bare always. An
imported `type` alias is transparent (its body resolves in the defining module's scope, carrying any
FFI-width license).

## 12b. Dynamic C-ABI FFI — `extern "lib":`

Call C functions in a shared library directly, with full static type-checking. An `extern "lib":`
block (indentation, not braces — `{` is a map literal) lists body-less C signatures; each becomes a
module-global callable, bound at module init by `dlopen` + `dlsym` and dispatched at runtime via
`libffi`. A missing library or symbol fails at startup.

> **The library name is passed straight to `dlopen`, so it is PLATFORM-SPECIFIC and NOT portable.**
> Every example below and in `examples/ffi*.chz` spells the Linux glibc names `libc.so.6` / `libm.so.6`;
> on **macOS** those do not exist and the block dies at module init with a `dlopen` failure — use
> `libSystem.B.dylib` (it carries both libc and libm). There is no per-platform selection syntax and no
> `libc`-alias resolution: pick the name for the host you run on. Note the repo's FFI goldens are
> `#[cfg(target_os = "linux")]`, so `cargo test` is **green on a Mac with the entire FFI surface
> unexercised** — don't read a green suite as evidence FFI works there (`docs/gaps.md` **W8-11**).

```chezzi
extern "libm.so.6":         # macOS: "libSystem.B.dylib"
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
field/…) with a *not C-marshallable* error. Calls run inline, so a slow C call pins its worker.

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
structs** (`Pair[int]`) have no fixed C layout and are rejected. A **zero-field** struct (`struct Empty:
pass`) is rejected too (*struct 'Empty' has no fields and cannot be C-marshallable*) — C has no empty
struct and libffi cannot build a call interface for one. (A transparent `type P = Point` alias
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
`0`. The callback runs identically on every run, and it fires on the calling thread
under `--parallel` (no cross-thread hand-off).

**A callback C STORES is not supported — and says so, loudly.** The trampoline is only valid for the
duration of the extern call that received it. If C keeps the pointer (`signal`, `atexit`, GLib/GTK,
`pthread_cleanup_*`) and invokes it *later*, the process aborts with:

```
chezzi FFI: callback invoked after the extern call that received it returned; stored/cross-thread callbacks are not supported
```

on stderr, and dies on **SIGABRT** (shell status 134). This is a defined, diagnosable failure, not a
crash: the trampoline is deliberately **leaked and poisoned** rather than freed, so it can never
execute freed memory (it used to segfault — gaps.md W6-8). The abort path calls nothing but `write(2)`
and `abort()` — both async-signal-safe, since the realistic caller is a C signal handler — so the
program's own **buffered stdout is discarded**, exactly as on any other crash (CPython loses it on
`abort` too). The diagnostic itself is never lost: it goes straight to fd 2 and is never queued. The
check is a *runtime* one; the checker
cannot reject it, because the identical `fn(int) -> int` param is correct for `qsort`, which invokes
the callback *during* the call.

A callback invoked on **any thread other than the one that made the extern call** — a library that
spawns its own thread, a signal delivered elsewhere — aborts the same way, whether or not the call has
returned:

```
chezzi FFI: callback invoked from a thread other than the one that made the extern call; stored/cross-thread callbacks are not supported
```

The price of that guarantee is a **leak**: every extern call that actually hands C a callback leaks its
trampoline (~400 B, plus a W^X page pair from libffi's closure pool), so a callback-passing extern call
in a hot loop grows both memory and mapping count. It degrades cleanly — when the pool can no longer
grow, the call raises the ordinary recoverable error `cannot allocate a callback trampoline for
argument N to 'f': the FFI closure pool is exhausted`, catchable with `recover:`.

**C output does not go through the runtime's stdout — and the broken-pipe halt cannot see it.** A
Chezzi `print` goes to the VM's sink, which is line-atomic across tasks, unbuffered, and raises a
recoverable fault the moment the reader closes (`stdout closed (broken pipe)`). A C function writes the
file descriptor itself, so **none of that applies to it**: its bytes sit in the C library's own buffer
(block-buffered when stdout is a pipe, so they usually appear *last*, at exit), and a closed reader is
invisible to the runtime — `chezzi run x.chz | head -1` on a loop of C writes runs forever instead of
faulting. Rust sets SIGPIPE to `SIG_IGN` process-wide and the loaded C library inherits that
disposition, so the signal a plain C program would die from never arrives either.

```chezzi
extern "libc.so.6":
    fn puts(s: str) -> int

print("chezzi-1")
_ := puts("ffi-2")
print("chezzi-3")
_ := puts("ffi-4")
# piped:  chezzi-1  chezzi-3  ffi-2  ffi-4   <- NOT source order; io.flush() does not change it
```

This is the same bargain as `ctypes` in Python and `cgo` in Go — both measured doing exactly this,
ordering included — and the same "outside the runtime's guarantees" contract `std.ffi`'s pointers
already carry. **Want the runtime's stdout? Return the string and `print` it.**

**If you must write from C, the C function's own return value is your error channel — and you have to
declare its exact width to see it.** The failure is real but the sign is not free: `puts` returns a C
`int`, and a bare `int` marshals as C **`long`** (see the fixed-width section below), so its `-1`
arrives as `4294967295` and every `< 0` guard silently never fires. Same call, same iteration, one
character of difference in the declaration:

| declared return | `puts`'s value once the reader is gone | `if r < 0` |
|---|---|---|
| `fn puts(s: str) -> int` | `4294967295` | **never fires** |
| `fn puts(s: str) -> int32` | `-1` | fires, at the same write |

```chezzi
import std.io
import int32 from std.ffi

extern "libc.so.6":
    fn puts(s: str) -> int32     # C `int` — NOT bare `int`, which is C `long`

fn main():
    i := 0
    while i < 200000:
        if puts("line") < 0:
            io.eprint("stdout is gone, stopped at i={i}")   # the runtime will NOT fault for you
            return
        i = i + 1

main()
```

Under `| head -1` that stops at `stopped at i=1638` — deterministic, because the C library reports the
dead pipe when its 4 KiB buffer first reaches `write(2)`, not on the call that filled it. Declared
`-> int` instead, the identical program runs to 200 000 and never notices.

**Deferred FFI features (with design notes + the callback feasibility ladder in
[`docs/ffi-and-packaging.md §1b`](ffi-and-packaging.md)):** the **rest of callbacks** (#4 — *stored* /
*cross-thread* callbacks a C library keeps and calls later or from its own thread — these **abort**
loudly today, see above; harder than in Python because `--parallel` has no GIL to serialize the
re-entry, plus they need a GC-rooting registry)
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
runs/engines, so printing it would be nondeterministic). A `ptr` is **sendable** (a plain
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

**Every `char*` return is UTF-8-validated.** A Chezzi `str` is UTF-8, so a non-UTF-8 buffer — a
latin-1 payload, or a `strchr`-style pointer landing mid-codepoint — is a clean **fault** naming the
offending byte offset, on `str`, `owned_str`, `str?` and `owned_str?` alike (an `owned_str` buffer is
still freed before the fault propagates, so there is no leak). It is never a silently mangled string
with U+FFFD in place of the bad bytes. Read raw/binary bytes with `std.ffi`'s `load_uint8_at` instead.
This matches `Socket.read`, which likewise refuses a binary payload rather than lossily decoding it.

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
(`Channel`/`Shared`/`RwShared`/`Atomic`/`AtomicInt`/`timer`/`Executor`), any of your `struct`/`newtype`/
enum-variant names, or a **builtin variant ctor** (`Ok`/`Err`/`Some`/`None`) — those resolve to a special op before a
plain call, so the extern would be silently shadowed; the checker rejects the collision (*'…' is a
builtin/reserved name*), in either declaration order and reported exactly **once**. A **type** name is
*not* a collision and is accepted: an `enum`'s own name, `Result`/`Option`, and a std-module layout name
whose module you never imported (`Match`) are not callable, so nothing shadows the extern.

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
  `load_*`/`store_*` (every C scalar width + `load_str`, which UTF-8-validates like an extern `str`
  return, each with an `_at(p, off)` byte-offset form) —
  for struct fields, return buffers, and C output-params. You can also **make your own C-laid-out
  buffer** via `std.ffi` `alloc(nbytes)`/`alloc_zeroed(nbytes)` (libc `malloc`/`calloc` → a raw `ptr`)
  and release it with `free(p)` (**manually freed** — `defer ffi.free(p)`; never auto-freed). Unsafe
  like `ctypes`: a bad pointer segfaults; double-free / use-after-free / out-of-bounds is UB; only the
  NULL base pointer is guarded (recoverable error). See `stdlib.md §std.ffi`.

**Deferred (v1 limits):** *stored / cross-thread* callbacks (sync scalar callbacks **shipped** — see
above; a stored one **aborts** with a named message on SIGABRT rather than segfaulting), varargs, a **GC-tracked auto-freed owned-buffer type** + bulk-copy helpers + `realloc` (the
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
into every program, like `std/prelude.chz`), not hidden in the compiler.

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

The universe builtins `print`, `ord`, `chr`, `panic` (fns) and `int`, `float`, `str`, `bytes`,
`bytearray` (ctors) are declared this way in `std/prelude.chz`. `print` is now expressible as the
variadic decl `native fn print(...args: Any, sep: str = " ", end: str = "\n") -> nil` (its lowering
still uses the specialized print opcodes — the decl is the checker-only signature authority), retiring
the last engine-synthetic signature. `range` + the `List`/`Map`/`Set` container ctors remain built-in
for now (their type-arg-driven generic identity is not a flat signature).

## 12d. `native struct` — native-type signatures in Chezzi (prelude/std-only)

The **type-level** analog of `native fn`/`native ctor`. A body-less `native struct Name:`
declares a native (Rust-backed) type's **checker signature** — its field layout — in Chezzi; the runtime
layout + method dispatch stay **native** (name-keyed). Like `native fn`, it is **prelude/std-only** and
**top-level only**.

```chezzi
# std/regex.chz — a file-backed, import-gated native module (phase 4b)
native struct Match:              # regex.Match's SIGNATURE (fields-only)
    text: str
    start: int
    end: int
    groups: List[str]

native fn find(pat: str, s: str) -> Result[Option[Match]]   # a native MODULE MEMBER
```

- A `native struct` body may declare **fields** and/or bodyless **`native fn` methods** (phase 4c-net):
  a `native fn` inside the body is an **instance method** and — like a user-struct method — declares a
  leading bare `self` as its first parameter (`native fn read(self, n: int) -> Result[str]`); it is
  harvested into the type's method table (harvest **strips** the `self` receiver, so the recorded sig is
  the call-arg shape) and checked via the normal method-resolution path (this is how `std.net`'s
  `Socket`/`Listener` declare `read`/`write`/`accept`/`close`). A `native fn` inside the body **without**
  a leading `self` is a parse error (`native instance method must declare 'self' as its first parameter`)
  — the self-less form is **reserved** for a future native *static* method (not yet supported); a
  module-level (free) `native fn` conversely may **not** take `self`. A **plain** `fn` method WITH a body
  IS now allowed (phase 4c-followup): it is **compiled** to bytecode (like an enum/struct method — no
  `StructDef`/`tid`) and dispatched via `Program::native_methods`, so a native struct may **mix** bodyless
  Rust-backed `native fn` sigs with pure-Chezzi bodied methods on one handle (first user: `std.io`'s
  `Reader.lines()`, a generator over `read_line()`). Such bodied methods **are type-checked** (the body is
run through the normal fn-body pass, so an ill-typed body is rejected). The same mix is allowed at a native
module's **top level** — a `std/*.chz` file may declare bodyless `native fn`s and ordinary bodied `fn`s side
by side (first user: `std.math`'s `divmod`, a pure-Chezzi `(q, r)` helper next to native `gcd`/`lcm`): the
bodied fn is harvested as a real member (callable qualified or via `import NAME from PATH`), its body is
type-checked, and it is bound at runtime by running the module toplevel, so Rust-backed and Chezzi-backed
members coexist in one namespace. A native file is still a real `.chz`, so it may itself `import` other
modules and use them from a bodied fn (e.g. `import std.string`). A `test` method or a field `= default` inside the body
  is still a parse error. **Asymmetry (deliberate, for now):** a **`native enum`** (`Option`/`Result`)
  still rejects a bodied method (`native enum methods are not supported`) — extending bodied methods to
  native enums is a symmetric follow-up, not yet wired (no native enum needs one today).
- A `native struct` may be **generic** (`native struct Shared[T]:`, phase 4c-concurrency): its method
  sigs may reference the type params (`native fn get(self) -> T`, `native fn set(self, v: T) -> nil`), and each
  call site **substitutes** the value's element type (`Shared[int].set` expects `int`) — the same subst
  the generic-struct machinery uses. This is how `std.concurrency` declares `Shared[T]`/`RwShared[T]`/
  `Atomic[T]` (and non-generic `Executor`). A method whose sig a plain harvested decl can't express (a
  return recovered from a closure argument's return type, `RwShared.read`; a discard-return `submit`) is
  declared with an **unannotated** closure param and re-typed post-harvest (checker-side metadata).
- A `native struct` maps its method table **ADDITIVELY onto an EXISTING reserved type** — it never mints
  a fresh nominal `Ty::Struct`. Besides the import-gated opaque handles above, the always-linked universe
  prelude (`std/prelude.chz`, phase 5a-containers) declares `native struct List[T]` / `Map[K: Hashable, V]`
  / `Set[T: Hashable]`: their method sigs (`push`/`pop`/`get`/`keys`/`union`/…) are harvested onto the
  RESERVED `Ty::List`/`Ty::Map`/`Ty::Set`, but the **literal syntax** (`[…]`/`{k:v}`/`{1,2}`) and the
  **turbofish ctor** (`List[int]()`) stay compiler-wired (their type-arg-driven element identity is not a
  flat sig). The higher-order `map`/`filter`/`fold`/`sort_by`/`sort_by_key` are **also file-backed**: a
  native method may declare its **own** generic param after the name (`native fn map[U](self, f: fn(T) -> U)
  -> List[U]`, `fold[U]`, `sort_by_key[K: Comparable]`), and the generic solver **recovers a
  return-position type param from an (even unannotated) closure argument's body** via a *closure-return
  loop-back* — this bidirectional inference is general (not map-special), so `Box(3).apply(fn(x): x + 1)`
  on a user `fn apply[U](self, f: fn(T) -> U) -> U` recovers `U = int` too. (`sort` IS file-backed as
  `native fn sort(self) -> nil where T: Comparable`; `sum` is `native fn sum(self) -> T where T: Add` but
  keeps a residual numeric check-gate — its true requirement is Monoid, so `where T: Add` alone is too
  broad.) A type param may carry a bound
  (`Map[K: Hashable, V]`), letting the internal `Map[K, V]`/`Set[T]` return types resolve at harvest.
- **Prelude/std-only:** a `native struct` (or `native fn`) in an ordinary user `.chz` is a **checker
  error** (*native struct declarations are only allowed in standard-library modules*); nesting is a
  parse error.
- **Native members of any file-backed std module** (phase 4b/4d/4c): `native fn`/`native struct` are no
  longer limited to the always-linked universe prelude (`std/prelude.chz`). A **normally-imported** file-backed
  std module (`std/regex.chz`, phase-4d `std/math.chz` / `std/io.chz` / `std/os.chz` / `std/rand.chz`
  / `std/fs.chz`, and phase-4c `std/net.chz` / `std/concurrency.chz`) declares its native type + functions
  **in-module**; the checker
  harvests them as the module's **signature source** (the type's field layout + the fns' signatures),
  while the runtime **values** stay bound natively (name-keyed via `native_members`). This is the
  import-gated **native-module-member** mechanism: `regex.Match`/`regex.find` (or `math.sqrt`) are reached
  exactly as before (`import std.regex` / `import Match from std.regex` licenses the bare `Match`;
  `regex.find(...)` qualified), the runtime + bytecode are unchanged, and it is **both-engine
  byte-identical**. It retired the earlier file-less companion-stub shortcut, and in phase 4d the
  hand-built `native_module_sig` arms for the five pure-function modules. (Checker-side metadata a
  `native fn` decl can't express — `math.pi`/`e`/`inf`/`nan` module values, `math.abs`/`sign`'s numeric
  polymorphism, and hover docs — is re-attached post-harvest.)

## 12e. `native enum` — reserved builtin-enum variant shape in Chezzi (prelude/std-only)

The **enum** analog of `native struct` (phase 5b). A body-less `native enum Name[T…]:` declares a
**reserved** builtin enum's **variant shape** (variant names + payload types) in Chezzi. Like
`native struct` it is **prelude/std-only** and **top-level only**, and it maps **ADDITIVELY onto an
EXISTING reserved type** — it never mints a fresh nominal `Ty::Enum`. The only native enums are the two
most deeply-wired builtins, declared in the always-linked universe prelude (`std/prelude.chz`):

```chezzi
native enum Option[T]:            # reserved Ty::Option — Some(T) / None
    Some(T)
    None

native enum Result[T, E]:         # reserved Ty::Result — Ok(T) / Err(E)
    Ok(T)
    Err(E)
```

- The body is its **variants** (an identifier with an optional `(typeList)` payload — reusing the
  ordinary `enum` variant grammar), optionally followed by bodyless **`native fn` methods** with a
  leading bare `self` (harvested into the enum's method table like native-struct methods; variants must
  precede methods, a self-less or plain-`fn` method is a parse error). `Option`/`Result` carry **no**
  methods. Generics use the same `[T…]` params as an ordinary enum (a param may carry a bound).
- **SHAPE-only, not the wiring.** The variant shape is file-backed as a **drift-guarded MIRROR**; the
  `?` operator, exhaustive `match`, top-level error propagation, and `Ok`/`Err`/`Some`/`None`
  **construction** all stay **Rust-wired** (the identity stays `Ty::Option`/`Ty::Result` via
  `resolve_type`; the variant set is synthesized inline from that `Ty` shape). The checker harvests the
  decl and asserts its variant set byte-matches the inline shape, so the `.chz` source-of-truth can
  never silently drift from the Rust wiring. `Result` is spelled in its faithful two-slot form
  `Result[T, E]` with `Err(E)`; the surface `Result[T]` → `E = Error`-protocol default is injected by
  `resolve_type`, not encoded in the variant.
- **Prelude/std-only:** a `native enum` in an ordinary user `.chz` is a **checker error** (*native enum
  declarations are only allowed in standard-library modules*); nesting is a parse error.

## 13. Standard library (v1)

> **The complete library reference — every global builtin, type method, runtime type, and `std.*`
> module with signatures — lives in [`stdlib.md`](stdlib.md).** This section is a short orientation.

Always available (no import): `print`, `range`, `int()`/`float()`/`str()`,
`ord(s)→int` (first codepoint), `chr(n)→str` (codepoint → 1-char string), `Set()`/`Set(list)`,
`panic(msg)` (raise a recoverable fault; see `recover:`), plus methods on the core types
(`list`/`map`/`set`/`str`/`bytes`/`bytearray`).

Modules are `import std.X` then `X.func(...)`. Importable:
`std.io`, `std.math`, `std.string`, `std.cmp`, `std.os`, `std.json`, `std.process`, `std.fs`,
`std.time`, `std.regex`, `std.request`, `std.net`, `std.ffi`, `std.iter`, `std.cancel`.

A few cross-cutting notes (full detail in `stdlib.md`):

- `min`/`max`/`clamp` live in **`std.cmp`** as generic `[T: Comparable]` functions (int/float/str and
  any struct/enum with a `compare` method — `Comparable` embeds `Eq`, and a struct/enum satisfies `Eq`
  structurally, so no `eq` is required); `list.sort()` is likewise Comparable.
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
  ResponseBox`) stays legal.

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
