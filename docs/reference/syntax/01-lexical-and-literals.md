# Chezzi — Syntax Reference

A scannable cheat-sheet for the whole language. Revise from this; feed it to an LLM as context.
For the *why* behind each choice see [`spec.md`](../spec/01-overview.md); for token names see
[`src/lexer/mod.rs`](../../../src/lexer/mod.rs).

Chapters (`chezzi docs syntax` prints them in this order):

1. [Lexical basics and literals](01-lexical-and-literals.md) (§1, §2)
2. [Variables and types](02-variables-and-types.md) (§3)
3. [Operators](03-operators.md) (§4)
4. [Functions](04-functions.md) and [function values](05-function-values.md) (§5)
5. [Control flow, indexing, comprehensions](06-control-flow.md) (§6, §6b, §6c)
6. [Structs](07-structs.md) (§7)
7. [Generics](08-generics.md), [protocols](09-protocols.md) and [protocol hooks](10-protocol-hooks.md) (§7b)
8. [Enums and pattern matching](11-enums-and-match.md) (§8)
9. [Errors](12-errors.md) (§9)
10. [Testing and program entry](13-testing-and-entry.md) (§9c, §9b)
11. [Strings and interpolation](14-strings.md) (§10)
12. [Pipe and concurrency](15-pipe-and-concurrency.md) (§11, §11b)
13. [Imports and modules](16-imports.md) (§12)
14. [FFI](17-ffi.md) (§12b)
15. [Native declarations, stdlib, full example](18-native-decls-and-example.md) (§12c to §13)

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

- **Blocks = indentation** (spaces only; a tab in indentation is a lex error). A block opens after a
  `:` line. A line that holds only spaces or tabs, or only spaces or tabs and then a comment, is
  skipped as a blank line, as in CPython. A tab before code is still an error.
- **Source encoding:** files are UTF-8. One byte-order mark (`U+FEFF`) at the start of a file is
  stripped, as CPython and Go do. A `U+FEFF` anywhere else, including a second one at the start, is a
  lex error. A `chezzi.toml` whose first bytes are `EF BB BF` parses as the same file without them.
- **Logical lines** end at a newline. A CRLF pair (`\r\n`) is one newline everywhere, inside string
  and bytes literals too, as in CPython, Go and Rust; a lone `\r` is not a newline. Blank / comment-only lines are ignored. A struct, enum, protocol
  or native body takes one declaration per logical line: two fields, two variants, or a variant and a
  method on one line is a parse error.
- **Identifiers:** letter or `_`, then letters/digits/`_`. Case-sensitive.
- **Doc-comments:** the plain `#` lines *immediately above* a declaration (`fn`/method, `struct`,
  `enum`, `protocol`, `type` alias, top-level binding) are its doc.
  - LSP hover shows the doc for free functions, methods, struct constructors and top-level bindings.
    For `enum`, `protocol` and `type` aliases the doc is parsed and attached but does not show on hover.
  - An enum-variant constructor name hovers its signature at the declaration and at a use
    (`Val(int)` → `fn(int) -> Col`, generic `Full(T)` → `fn(T) -> Box[T]`). A variant has no doc of its own.
  - The doc is the contiguous run of comment lines with no blank line before the declaration. Stacked
    lines join with newlines (one leading `# ` stripped per line). A trailing comment on the decl line
    is not a doc. Doc-comments never affect type-checking or execution.
  - Reinstall the LSP after an upgrade (`cargo install --path . --features lsp --bin chezzi-lsp`).

## 2. Literals

```chezzi
42            # int   (i64)
1_000_000     # int   — '_' is a digit-group separator (only between digits)
0xFF 0b1010 0o17   # int — hex / binary / octal literals ('_' ok between digits, and ONE right after the prefix: `0x_ff`)
-9223372036854775808   # int — the i64::MIN boundary literal (the magnitude 2^63 is legal ONLY when
              #       immediately negated; a bare `9223372036854775808` is "number too large")
3.14          # float (f64)
1_234.567_8   # float — '_' works in both parts
6.022e23  1e3  1.5e-9  2E10  1e+5   # float — scientific notation (any exponent ⇒ float, so 1e3 = 1000.0)
true  false   # bool
"hello"       # str
'hello'       # str — single quotes are equivalent to double (same escapes & interpolation)
"""say "hi" out loud"""  # str — triple-quoted; unescaped quotes allowed inside (same escapes & interpolation)
'''it's "ok"'''  # str — triple single-quote, equivalent
"hi {name}"   # str with interpolation — see §10
"emoji \u{1F600}, A=\u{41}"   # str — \u{HEX} unicode escape (1-6 hex digits)
r"\d+\s"      # str — raw string: VERBATIM; no escapes (\ is literal), no interp ({ } are literal)
r'C:\tmp'     # str — raw, single-quote / uppercase R"…" both work; cannot contain its own quote
r"""{"k": [1,2]}"""  # str — triple raw: embeds quotes + braces verbatim (best for JSON / brace-heavy)
b"\x01\x02AB"  # bytes — byte-string literal; \xHH hex byte + \n \t \r \\ \" \' \0; no \u, no interp
b'\xff'       # bytes — single quotes / uppercase B'…' both work; raw byte >=0x80 must use \xHH
bytearray.from_iter([1, 2, 3])  # bytearray — MUTABLE byte buffer; no literal, see below
[1, 2, 3]     # List[int]
{"a": 1}      # Map[str, int]
```

**`bytes` — immutable byte sequence (Python `bytes` model).**

- Escapes: `\xHH` (two hex digits, one byte `0x00`–`0xFF`, the only way to write a byte ≥ 0x80) and
  `\n \t \r \\ \" \' \0`. `\u{…}` and a raw non-ASCII source char are rejected. No interpolation.
- `b[i]` → `int` 0–255 (out of range is a recoverable panic). `b[a..b]` → `bytes` over byte offsets
  (open bounds, `Range` step, `reversed()`, negative). `for x in b` yields `int`. `b.len()` is the
  byte count.
- `==`/`!=` are structural. `bytes` is `Hashable` (a valid `map`/`set` key). `b[i] = x` is a type error.
- `str(b)` / `print(b)` / interpolation use the Python `b'...'` repr (printable ASCII literal, others `\xHH`).

**Raw strings — `r"..."` / `r'...'` and triple `r"""..."""` / `r'''...'''`.** A raw string is an
ordinary `str`, lexed verbatim:

- No escape processing: `r"\d+"` is a backslash, `d`, `+`; `r"C:\tmp"` is a literal Windows path.
- No interpolation: `r"{}"` prints `{}`, and `r"{x}"` stays `{x}` even with `x` in scope. Use it
  instead of doubling braces (`"{{}}"`).
- The `r`/`R` prefix applies only when a quote follows at once, so a variable named `r` is unaffected (same as `b`).
- The short form cannot contain its own closing quote. The triple form embeds lone quotes, for
  example JSON: `r"""{"k": [1,2]}"""`.
- Not supported: a raw-bytes prefix `rb"..."` and Rust-style `r#"..."#` delimiters.

**`bytearray` — the MUTABLE sibling (Python `bytearray` model).** No literal:
`bytearray()` (empty), `bytearray.zeroed(N)` (N zero bytes), `bytearray.from_iter(xs)` (the bytes of
any iterable of `int`: a `bytes`, a `bytearray`, a `List[int]`, a `Range`; each element 0–255).

- `ba[i]` → `int` 0–255. `ba[i] = x` mutates in place; the value must be 0–255 and the index in range,
  else a recoverable panic. `ba[a..b]` → a NEW `bytearray` (copy, byte offsets).
- `for x in ba` yields `int`. Methods: `ba.len()`, `.push(int)`, `.pop() -> int?`,
  `.extend(bytes | bytearray | List[int])`.
- `==`/`!=` are structural; `b"a" == bytearray.from_iter([97])` is content-equal (as in Python).
- `bytearray` is NOT `Hashable` (mutable, like `list`/`set`/`map`).
- Repr: `bytearray(b'...')`. `bytes.from_iter(ba)` gives an immutable snapshot; `bytearray.from_iter(b)` a mutable copy.
- A task gets a `bytearray` by handle, like a `list` (M3.4): a write in the task lands on the parent's buffer.
- Not supported: a `byte`/`u8` scalar, non-UTF-8 codecs, base64/hex.

**Built-in conversions.**

| Conversion | Form | Gives | Notes |
| --- | --- | --- | --- |
| str → bytes (UTF-8) | `s.encode()` | `bytes` | method on `str`; always succeeds (str is UTF-8 internally) |
| bytes → str (UTF-8) | `b.decode()` | `str` | method on `bytes`; **recoverable** fault on invalid UTF-8 |
| bytearray → str (UTF-8) | `ba.decode()` | `str` | identical to `bytes.decode()` (decodes the current buffer) |
| any iterable → list | `List.from_iter(it)` | `List[T]` | `it` is any **for-iterable**; `T` is the element type. `List[T]()` / `List()` are the empty forms. |
| any iterable → set | `Set.from_iter(it)` | `Set[T]` | dedup; `T` must be `Hashable`; `Set[T]()` / `Set()` (0 args) is the empty set |
| iterable of 2-tuples → map | `Map.from_iter(it)` | `Map[K, V]` | `it` yields `(K, V)` pairs; last-wins on dup keys; `K` `Hashable`. `Map[K, V]()` / `Map()` are the empty forms. |

`.encode()`/`.decode()` are UTF-8 only; there is no encoding-name argument.
`"héllo".encode().decode() == "héllo"` round-trips. `b"\xff\xfe".decode()` faults recoverably
(`recover:` catches it).

`List.from_iter(it)` / `Set.from_iter(it)` / `Map.from_iter(it)` (the static `from_iter` of each type's
`native struct` decl in `std/prelude.chz`) accept any for-iterable, the same set `for x in it` accepts:
`list`, `set`, `str` (per-char `str`), `bytes`/`bytearray` (per-byte `int`), `map` (`(key, value)` pairs),
`Channel` (`List.from_iter(ch)` drains it until the channel is closed and empty), a `Range` value, and a user
struct with `next(self) -> T?`. They do not need an `Iterable[T]` bound. A call on the type's name
makes only an empty value: `List([1])` is a type error whose help names `List.from_iter(xs)`.

- `List[T]()` / `Map[K, V]()` / `Set[T]()` take a turbofish that pins the types.
- The bare `List()` / `Map()` / `Set()` take their type from the expected type on the same statement
  (`xs: List[int] = List()`), like `[]` / `{}`. An untyped `xs := List()` is an error (see
  "Inference" in §10). The literals stay the idiomatic empty forms.
- A turbofish on the static checks the elements: `List[int].from_iter([1, 2])` is fine,
  `List[int].from_iter(["a"])` is a static error (`iterator element type str does not match the
  declared element type int`). The elements keep their own type, so `List[int?].from_iter([5])` is
  an error too (help: `write each element as \`?x\``), and `List[int?].from_iter([?5])` checks.
- `Map.from_iter(it)`'s element must be exactly a 2-tuple `(K, V)`; anything else is a static type error.

> **`Map.from_iter(it)` vs `xs.map(f)` do not clash.** `Map.from_iter(pairs)` is a static method of the type;
> `xs.map(f)` is the `List` method. `Map.from_iter([(1, "a")])` builds a `Map[int, str]` while
> `[1, 2].map(double)` transforms a list.

**Multi-line literals & trailing commas.** Inside `[]`, `{}` and `()` newlines and indentation are
ignored, so a literal, a call's arguments or a parameter list may span lines. One optional trailing
comma is allowed before the closing delimiter: `[1, 2,]` is `[1, 2]` (a lone `[,]` / `(,)` / `f(,)`
is an error):

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

**`(x)` is `x`; `(x,)` is a one-element tuple**, as a value, a type and a pattern. It prints with
the comma: `print((1,))` writes `(1,)`, as in Python, and its type prints `(int,)`. `fn one() -> (int,)`
returns one (Rust `(i64,)`); `a, := t` unpacks one (CPython `a, = t`); a nested one is `(b,)`
(`a, (b,) := (1, (2,))`); a `match` arm takes `(a,)`. A value list takes no trailing comma:
`x := 1,` is an error.

**Tuple elements are read with `.0`, `.1`, … (Rust's spelling), never `t[0]`.** The index must be a
literal. Tuples are immutable, so `.N` is read-only. Accesses chain: `t.0.1` is the second element of
`t.0` (the lexer never reads `0.1` after a field `.` as a float):

```chezzi
t := ("a", 1)
print(t.0)        # a
print(t.1)        # 1
# t[0]  → type error: cannot index into (str, int)   (a tuple is not Indexable)
# t.0 = "b"  → type error: cannot assign to field '0' of (str, int)
a, b := t         # destructuring is the other way in — see §3
a, b := 1, 2      # a value list destructures the same way (Go, CPython)
a, (b, c) := (1, (2, 3))   # a nested target takes parentheses; `_` skips a slot
x := 1, 2         # a bare tuple ends a statement: `x` is `(1, 2)` (CPython); so do `x = 3, 4`,
                  # `return a / b, a % b` and `yield a, b`
```

**One unpacking grammar.** `:=`, `=`, `for`, a comprehension clause and a `wait:` arm share one
target grammar: names separated by commas, a nested target in parentheses, `_` for a skipped slot
(`a, (b, c) := t`, `for k, (a, b) in xs`, `[a for a, (_, c) in xs]`).

- The whole target takes no outer parentheses: `(a, b) := t`, `(a,) := t` and `for (a, b) in xs`
  are errors (CPython accepts all three). A one-name group `(a) := t` is `a := t`.
- Outside `match` only tuples unpack. The element of any iterable unpacks the same way:
  `for a, b in {(1, "x")}` over a Set, a Map's `(key, value)` pair, a generator, a channel.
- A struct or an enum variant unpacks in `match` only. There is no list pattern and no `*rest`.
- Arity errors: `tuple pattern has 2 element(s), but the value has 3`; `tuple pattern cannot match a
  value of type int`.
- `_` is never a value: `for _ in xs: print(_)` is `cannot use '_' as a value`.

Use `.N` where you cannot bind names, for example in a single-expression closure:
`xs.sort_by_key(fn(p) -> int: p.1)`.
