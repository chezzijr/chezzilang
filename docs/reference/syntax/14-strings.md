## 10. Strings & interpolation

```chezzi
name := "chezzi"
age := 30
print("hi {name}, age {age}")     # {expr} interpolates
print("sum: {a + b}")             # any expression
print("brace: {{not interpolated}}")   # '{{' / '}}' = literal braces
```

> **Interpolation is always on, but a hole whose whole text is digits renders literally**, so a regex
> quantifier survives (as in CPython):
>
> ```chezzi
> print("\\d{4}-\\d{2}")     # \d{4}-\d{2}  (python3: \d{4}-\d{2})
> print(r"\d{4}-\d{2}")      # \d{4}-\d{2}
> ```
>
> A raw string `r"…"` is the recommended spelling for a regex: it keeps every backslash literal too.

**Quote styles.** `"…"` and `'…'` are interchangeable (same type, escapes and interpolation). Inside
`"…"` a `'` is literal; inside `'…'` a `"` is literal. `\'` and `\"` work in either style.

**Triple-quoted strings.** `"""…"""` and `'''…'''` have the same escapes and interpolation; an
unescaped quote inside is literal. (Regular strings already span newlines.)

```chezzi
print("""She said "hello" to {name}""")   # unescaped quotes + interpolation
print('''it's a "quoted" word''')          # apostrophes and double-quotes, literal
```

**Raw strings (`r"…"`).** Any quote form with an `r`/`R` prefix is taken verbatim: no escapes, no
interpolation (full rules in §2).

```chezzi
x := 5
print(r"{}")                  # {}        ← literal braces, no doubling needed
print(r"\d+\s")               # \d+\s     ← literal backslashes, no escapes
print(r"{x}")                 # {x}       ← raw: NOT interpolated
print("{x}")                  # 5         ← normal string still interpolates (raw is opt-in)
print(r"""{"k": [1,2]}""")    # {"k": [1,2]}   ← triple raw embeds quotes + braces
```

**Escapes.** `\n` `\t` `\r` `\\` `\"` `\'` `\0` and `\u{HEX}` (1-6 hex digits naming a Unicode scalar
value: `\u{41}` → `A`, `\u{1F600}` → 😀). A surrogate (`D800`-`DFFF`), a value above `10FFFF`, an empty
`\u{}`, a missing brace, a non-hex digit or any other unknown escape is an error. An escape resolves
once, in the literal's text, and an escaped brace is text: `"\u{7B}x\u{7D}"` is `{x}`, not a hole.

```chezzi
print("tab\tgap, quote \"x\", path C:\\tmp")
print('single quotes work too: {name}, with \'apostrophe\' and \u{2728}')
print("literal {{x}} vs value {x}")
```

### What goes inside `{…}`

Any expression. A call in a hole takes named arguments, defaults and variadics like the same call
outside. The lexer reads the hole's code once, in place, with the rules of the code around it:

1. **A nested literal may use the enclosing quote** (CPython 3.12+): `"{d["k"]}"`, and `"{"{x}"}"`
   nests an interpolated literal. Holes nest up to **149** levels (CPython 3.14's limit); the 150th is
   `too many nested interpolated strings`. A hole counts one level of expression depth, like a parenthesis.
2. **Hole code has no backslash.** `"{d[\"k\"]}"` and `"{x\t+ 1}"` are errors that name the fix.
   Escapes work in a literal's text, a format spec's text, and a nested literal: `"{'\\n'.len()}"` is `2`.
3. **The first `:` at hole depth** (outside brackets and nested literals) **starts the format spec.**
   `{if c: 1 else: 2}`, `{fn(): 1}` and a bare `match` or `recover:` are errors that say to add
   parentheses: `{(if c: 1 else: 2)}`, `{(if c: 1 else: 2):>5}`. `{x:=5}` is the spec `=5`. A `:` in an
   index, string key or slice (`{m["a:b"]}`, `{xs[1..2]}`) is not the separator.

- A hole is a bracket for layout: a line break inside it is white space, so `{x +⏎ 1}` and a method
  chain over three lines work in `"""…"""` and `"…"`. Padding spaces are ignored.
- A `}` inside a nested string or inside `(`/`[`/`{` belongs to the expression.
- A `#` outside a nested string starts a comment to the end of the line (CPython 3.12+). In a
  multi-line hole it ends at the line break (`"""{x # note⏎}"""` is `x`); on one line it hides the
  closing `}` and is a compile error at the `#`. A `#` in a nested string (`{d['#']}`) or a spec
  (`{n:#x}`) is text.

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

After the first `:` the rest of the hole is the format spec, as literal text, so any character may be
the fill (`"{x:'>5}"` → `''''7`, `"{x:(>5}"` → `((((7`). An escape in a spec resolves once
(`"{x:\u{3E}4}"` pads like `{x:>4}`). The literal's closing quote ends the spec, so a fill equal to it
needs the other quote style.

**Differences from CPython.**
- Every literal interpolates, a nested one too, so a brace in it is doubled: `"{'}}'}"` is `}` (CPython
  `f"{'}'}"`), and a lone `}` in a nested literal is `unmatched '}'`.
- An all-digit hole is text: `"a{4}b"` is `a{4}b` (CPython `a4b`).
- An `extern "lib":` header takes a plain string: `extern "lib{x}"` is
  `an extern library name cannot interpolate`.

### Format specifiers

An interpolation may carry a Python-style spec after a `:` (`{expr:spec}`):

```
{expr:[[fill]align][sign][z][#][0][width][grouping][.precision][type]}
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
print("{-0.0001:z.1f}")   # z: a rounded-to-zero float drops its '-' → "0.0" (CPython 3.11+;
                          # rejected on an int type and a string, as in CPython)
print("{greeting:.5}")    # string precision truncates → "hello"
print("{1234567:,}")      # thousands grouping         → "1,234,567"
print("{1234567:_}")      # underscore grouping        → "1_234_567"
print("{255:#x}")         # alternate form: radix prefix → "0xff"
print("{1234567.0:g}")    # general format              → "1.23457e+06"
print("{42:=10}")         # sign-aware fill              → "        42"
print("{42: d}")          # leading-space sign           → " 42"
w := 6
print("|{name:<{w}}|")    # width measured at runtime    → "|chezzi|" (w=6 → "|ab    |" for "ab")
print("{3.14159:.{2}f}")  # runtime precision            → "3.14"
```

- **align**: `<` left, `>` right, `^` center, `=` sign-aware fill, with an optional **fill** char
  before it (default space). Numbers default to right, everything else to left. `=` pads between sign
  and digits (`{-42:=10}` → `"-       42"`); it is numeric-only. With the `0` flag or a `0` fill, `=`
  pads with zeros and widens the grouped digits like the `0` flag (`{1000:=08,}` → `"0,001,000"`).
- **sign**: `+` forces `+` on non-negative numbers, ` ` leaves a space (`{42: d}` → `" 42"`), `-` is
  the default. Numeric-only.
- **`#`**: on `x`/`X`/`o`/`b` adds `0x`/`0X`/`0o`/`0b` (`{255:#x}` → `"0xff"`); on a float forces a
  decimal point (`{0.5:#.0f}` → `"0."`), except on a non-finite value (`{x:#f}` on infinity is
  `"inf"`); no-op on `d`; rejected on a string.
- **`0`**: sets the fill to `'0'` unless a fill is written, under every align, and makes the align
  `=` only when none is written. `{42:08}` and `{42:>08}` → `"00000042"`; `{42:<08}` → `"42000000"`;
  `{42:^08}` → `"00042000"`; `{-42:08}` → `"-0000042"` but `{-42:>08}` → `"00000-42"`;
  `{42:*>08}` → `"******42"` (a written fill wins).
- **width**: minimum field width, **capped at 4096**; a larger literal width (`{x:>9999999999}`) is a
  parse error. It may be a nested `{expr}` evaluated at runtime (CPython `f"{s:<{w}}"`, Go `%-*s`,
  Rust `{:<w$}`): with `w := 6`, `"|{s:<{w}}|"` is `|ab    |`, `"|{s:>{w}}|"` is `|    ab|`,
  `"|{s:*^{w}}|"` is `|**ab**|`, `"{42:0{w}}"` is `000042`, `"{s:<{w + 2}}"` is `ab      `.
- **grouping**: `,` or `_` every three integer digits (`1,234,567`, `1_234_567`); `_` groups
  `x`/`X`/`b`/`o` in fours (`{0xfffff:_x}` → `f_ffff`). `,` is rejected with those four types; both are
  rejected on a string. With zero-pad the field never starts with a separator (`{1000:08,}` →
  `"0,001,000"`, nine chars for width eight, as CPython).
- **precision** `.N`: float decimals; on a string it truncates to N chars. Capped at 4096. It may be
  a nested field: with `p := 2`, `"{3.14159:.{p}f}"` is `3.14`, `"{3.14159:{w}.{p}f}"` (`w := 6`) is
  `  3.14`, `"{'hello':.{p}}"` is `he`.
- **nested field** `{expr}`: legal only as the width or the precision (as Go and Rust). Elsewhere:
  `format spec: a nested field is allowed only as the width or the precision` (CPython re-parses the
  spec at runtime, Chezzi checks its shape at `check` time).
  - It must be an `int`: a static `str`/`float`/`bool` is `format spec: a nested width field must be
    an int, found str`; an unknown type faults with the same text at runtime.
  - It cannot carry a spec (`{w:3}`) or be empty (`{}`).
  - Order: the value, then the width, then the precision (`"{f():<{g()}}"` runs `f` first). A name
    read only in a nested field is still captured by a closure.
  - A value over 4096 faults before any buffer is sized (`format spec: width exceeds maximum 4096`,
    `format spec: precision exceeds maximum 4096`). A negative value faults
    (`format spec: width must not be negative, got -1`). Both are recoverable.
- **type**:
  - `d` int
  - `f`/`F` fixed float (default precision 6, never scientific: `{1e16:f}` →
    `"10000000000000000.000000"`)
  - `x`/`X` hex, `b` binary, `o` octal
  - `e`/`E` scientific (default precision 6, exponent signed and at least 2 digits: `1.234568e+05`)
  - `g`/`G` general (fixed when the exponent is in `-4..precision`, else scientific; trailing zeros
    stripped unless `#`; default precision 6, minimum 1)
  - `%` percent (×100 then `%`)
  - `inf`/`-inf`/`nan` print `INF`/`-INF`/`NAN` under `E`/`F`/`G` and lower case under
    `e`/`f`/`g`/`%` (with the `NaN` casing exception below). A float type char promotes an int.

**Defaults and checks.**
- A bare `{expr}` (or `{expr:}`) prints a whole float with `.0` (`5.0`).
- `.N` with no type char is CPython's general format: `{123.456:.3}` → `"1.23e+02"`, `{100.0:.6}` →
  `"100.0"`. It differs from `g`: the crossover is one exponent lower, and an integral fixed result
  keeps its `.0`.
- An unknown type char or trailing junk is a parse error.
- A type/value mismatch (`{name:d}` on a string, `{x:.2f}` on a string, `{x:d}` on a float, `{x:.3d}`
  on an int, zero-pad on a non-number) is a compile error when the value's static type is a concrete
  scalar (Python raises `ValueError` at runtime). A concrete non-scalar (`List`/`Map`/`Set`/tuple,
  `T?`/`T!E`, a struct, an enum, `bytes`, a fn value, a native struct like `Shared` or `Channel`) is
  checked against the string rules: `xs := [1]; "{xs:d}"`, `n: int? = None; "{n:+}"`, `{p:d}` on a
  struct and `{e:d}` on an enum are compile errors; a width/fill spec (`{o:>12}` on a `float?`) passes.
- The same check runs at runtime, with the same wording, for a value the checker cannot pin (a generic
  `fn show[T](v: T): "{v:.2f}"` given a `str`, a protocol existential). A program that passes `check`
  never faults with `format spec:` on a concrete-typed hole.

**Plain float printing matches CPython `repr()`/`str()`, except `NaN` casing:** Chezzi prints `NaN`
where CPython prints `nan`, on every path (`print`, `str()`, interpolation, container element,
`{x:.2f}`, `{x:>8}`). `inf`/`-inf` agree. Test with `math.is_nan`, not a string.

- A bare float uses scientific notation when the decimal exponent is `< -4` or `>= 16`, else fixed:
  `1e16` → `1e+16`, `1e15` → `1000000000000000.0`, `0.00001` → `1e-05`, `1.5e300` → `1.5e+300`,
  `-2.5e-8` → `-2.5e-08`. Whole floats in the fixed range keep `.0` (`1.0`, `1000.0`).
- The mantissa is the shortest that round-trips; the exponent is signed and at least 2 digits.
- An exact tie between two shortest candidates breaks to even, as CPython:
  `print(771.5462036132812)` is `771.5462036132812` (the exact value is `771.54620361328125`).
- `json.stringify` uses the same formatter (`1.5e+300` is valid JSON and round-trips).

### Built-in methods on core types

```chezzi
s.len()          s.upper()        s.lower()
s.trim()         s.strip()        s.split(",")
s.starts_with("ab")  s.ends_with("yz")  s.contains("b")
",".join(parts)  # join: separator.join(List[str])
s.chars()        # → List[str] of 1-char strings; also `for c in s:` iterates them
s.replace("a","b")  s.repeat(3)   s.reversed()     s.pad_left(4,"0")  s.pad_right(4," ")
s.index_of("x")  s.count("x")     s.strip_prefix("p")  s.strip_suffix("s")
s.split_lines()  # → List[str] split on a newline (LF, CRLF, or lone CR)
s.to_int()       s.to_float()     # → int? / float? (a value / None — None on bad input)
s.parse_int()    s.parse_float()  # → int!str / float!str (the value / !msg on bad input)
"a" + "b"        # concatenation
```

`ends_with`/`replace`/`repeat`/`pad_left`/`index_of`/`count`/`strip_prefix`/`strip_suffix`/`split_lines`
call the matching `std.string` free fns (no import needed). A character is a 1-char `str` (there is no
`char` type): index with `s[i]`, iterate with `for c in s:` or `s.chars()`, convert with `ord`/`chr`.

**List methods:** `xs.push(x)` `xs.pop()` `xs.len()` `xs.reverse()` `xs.contains(v)` `xs.index_of(v)`
`xs.sum()` (numeric; empty `-> 0`, or `0.0` for a `List[float]`), `xs.sort()` (ascending, in place),
`xs.concat(ys)→list` (new list), `xs.extend(ys)` (append in place, → None), `xs.copy()→list`
(shallow), `xs.map(f)` `xs.filter(p)` `xs.fold(init, f)`, `xs.sort_by(fn(a, b) -> int)` (negative means
`a` first; stable, in place), `xs.sort_by_key(fn(x) -> K)` (`K` Comparable: int/float/bool/str or a type
with `compare`; stable, in place).

**Map methods:** `m.get(k)→V?` `m.contains(k)` `m.keys()` `m.values()` `m.remove(k)` `m.len()`,
`m.items()→List[(K, V)]` (insertion order, so `Map(m.items()) == m`), `m.copy()→map` (shallow),
`m.merge(n)→map` (new map, `n` wins on a clash), `m.update(n)` (in place, → None). `m[k]` reads
(errors on a missing key); `m[k] = v` inserts or updates. A Map iterates `(key, value)` pairs
(`for k, v in m`, `[kv for kv in m]`, `List(m)`, `Iterable[(K, V)]`); `k in m` tests a key.

**Sets:** `{a, b, c}` is a set literal (deduped, insertion-ordered). `{}` is the empty *map*; the
empty set is `Set()`; `Set(list)` builds one. Elements are any `Hashable` type (int/str/bool, a struct
or enum with `hash(self) -> int` and `eq`, or a tuple of those). Methods: `s.insert(x)→bool` `s.remove(x)→bool` `s.contains(x)`
`s.len()` `s.union(t)` `s.intersection(t)` `s.difference(t)` `s.copy()→set` (shallow). Iterate with
`for x in s`. `==` ignores order.

### Inference

**Signatures are written; bodies are inferred from the line they are on.** A value's type is known on
the statement that creates it. Inference never reads a later statement (a `push`, assignment, call or
`return`).

| You write | Verdict |
|---|---|
| `xs := []`, `m := {}`, `s := Set()`, `z := None`, `h := [None]`, `t := ([], 1)`, `c := Cell(None)`, `v := Box.new()`, `e := Bx.Empty`, `ys := id([])` | error on that line |
| `xs: List[int] = []`, `xs := List[int]()`, `z: int? = None`, `Box[int].new()`, `Bx[int].Empty`, `f([])` into a typed parameter, `return []` in a typed fn, a typed field or element slot | accepted: the same statement supplies the type |
| `xs := [1, 2]`, `[None, 7]` (`List[int?]`), `if c: None else: 7`, `z := ?5` (`int?`), lambdas, `id(5)` | accepted: same-line inference |
| `print(None)`, `print([])`, `[] == []`, `for x in []:`, `[].len()` | accepted: the open value is consumed, never stored |

The error names the binding and both fixes:
``cannot infer the element type of `xs`: write `xs: List[int] = ...` or `xs := List[int]()` `` (the
type is an example). Other forms: ``cannot infer the key and value types of `m` ``,
``cannot infer the type of `z` from `None` ``, ``cannot infer the success type of `r` ``
(`r: int! = recover: ...`), ``cannot infer the type arguments of `c` ``. One error per open binding;
later uses report nothing more.

- A generic fn value read without type arguments is an error where it is bound (`g := ident`; write
  `g := ident[int]` or `g: fn(int) -> int = ident`).
- An unannotated closure parameter typed by an open argument is an error naming the parameter
  (`fold(xs, [], fn(acc, x): acc + [x])`; bind the result to a typed name, or pass `List[int]()`).
- A free closure with an open body keeps its own message (`g := fn(): []` is
  `cannot infer return type of '<closure>'; add a -> annotation`).
- An annotation fixes a collection's element type, so a conflicting `push` is a mismatch
  (`argument 1 of 'push': expected int, found str`). For mixed elements, annotate with a protocol:
  `shapes: List[Shape] = []`.
