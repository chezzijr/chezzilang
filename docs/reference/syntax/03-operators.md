## 4. Operators & precedence

Highest → lowest. Same row = same precedence, left-associative unless noted.

| Level | Operators | Notes |
|-------|-----------|-------|
| 1 | `f(x)` `a.b` `a[i]` `a?.b` | call, field access, index, optional chaining |
| 2 | `?` | error propagation (postfix, §9) |
| 3 | `-` (unary) | |
| 4 | `??` | null-coalescing, RIGHT-associative, `T?` or `T!E` (see below) |
| 5 | `*` `/` `%` | `*` also list repeat: `[0] * 3` (and `3 * [0]`, commutative) |
| 6 | `+` `-` | `+` also list concat: `[1,2] + [3,4]`; `-` also set difference: `a - b` |
| 7 | `<<` `>>` | bitwise shift (int-only) |
| 8 | `&` | bitwise and (int) / set intersection (`Set[T]`) |
| 9 | `^` | bitwise xor (int) / set symmetric-difference (`Set[T]`) |
| 10 | `\|` | bitwise or (int) / set union (`Set[T]`) |
| 11 | `..` `..=` | range value (`Range`); does not chain; either bound may be omitted: `..b`, `a..`, `..` |
| 12 | `<` `<=` `>` `>=` `==` `!=` `in` | ALL SEVEN comparisons — one level, and they CHAIN (see below); `in` = membership, yields `bool` |
| 13 | `not` (prefix) | looser than the comparisons, tighter than `and` (Python's grammar) |
| 14 | `and` | |
| 15 | `or` | |
| 16 | `\|>` | pipe (§11), left-assoc |

The relative order follows Python (comparison looser than `..` < `|` < `^` < `&` < shifts).
`0..1 << 2` is `0..4`; `x in 0..3` and `0..3 == 0..3` need no parentheses.

**Shifts and overflow.**

- A shift amount outside `0..64` is a runtime error.
- A `<<` that drops a significant bit is a recoverable `integer overflow in Shl` (for example
  `n := 1; n << 63`), as for `+ - * /`. Round-trip-safe shifts, including `-1 << 63 == INT_MIN`,
  succeed. `>>` never overflows.
- An **all-constant** int expression under `+ - * / % & | ^ << >>` and unary `-` that overflows `i64`
  (`print(9223372036854775807 + 1)`, `print(1 << 63)`) is a **compile error** (as in Go). With a
  non-constant operand (`m := 9223372036854775807; m + 1`) it is the runtime fault. A constant zero
  divisor and a constant shift amount outside `0..64` are runtime faults too.

**`??` and `not`.** `??` binds tighter than every binary operator, so `m.get("a") ?? 0 + 1` is
`(m.get("a") ?? 0) + 1`, yielding `2`. `not` binds looser than the comparisons, so `not x in xs` is
`not (x in xs)`, while `1 + not y` is a parse error (write `1 + (not y)`).

**Comparisons chain, like Python.** `0 <= i < n` means `0 <= i and i < n`.

- Each interior operand runs once (`1 < f() < 3` calls `f` once).
- The chain stops at the first false link (in `1 < f() < 0 < g()`, `g` never runs once `f() < 0` is false).
- Parentheses stop a chain: `(a < b) == c` is two ordinary comparisons.
- A link whose operands cannot be compared is a type error. `1 < 2 == true` chains as
  `1 < 2 and 2 == true`, and the second link is `cannot compare int and bool for equality`
  (CPython answers `False`).

**Collection operators.** `+ *` and `& ^ |` also work on collections, with the same behaviour as the
method forms (a mismatched element type is a type error):

- `List[T] + List[T]` → concat (= `.concat`); element types must match. `[] + [1]` infers `List[int]`.
- `List[T] * int` / `int * List[T]` → repeat; `n <= 0` → `[]`. A giant `n` raises a recoverable
  `list repeat capacity overflow`. An empty source repeats to `[]` in constant time for any `n`.
- `Set[T] | Set[T]` → union (= `.union`), `&` → intersection (= `.intersection`), `-` → difference
  (= `.difference`), `^` → symmetric-difference (no method form). The result keeps insertion order.

Every compound-assign form is `x = x OP v`: an operator returns a new value and never mutates an
operand. `xs += ys` builds a new list and rebinds `xs`; an alias taken before keeps its old value
(CPython's `list.__iadd__` and `set.__ior__` mutate in place instead). To change a collection in
place, call a method: `xs.push(v)`, `xs.extend(ys)`. A user type's operator hook can still mutate
`self` (a convention, not a check). A loop variable and a `const` binding reject every compound form,
and `int += float` is rejected as `x = 1.5` is.

    xs := [1]
    ys := xs
    xs += [2]
    print(ys)        # [1]: `+=` built a new list
    s := {1}
    t := s
    s |= {2}
    print(t)         # {1}

Bitwise `& ^ | << >>` on **int** operands is unchanged; `<< >>` are int-only, and a float operand is
a type error.

**Membership `in`.** `x in xs` is a `bool`: element-of for a `list`/`set`, **key**-of for a `map`
(`k in m` tests keys, not values), and substring-of for a `str`. The element/key/`str` type must match
the left operand.

```chezzi
3 in [1, 2, 3]      # true   — list element
20 in {10, 20}      # true   — set element
"a" in {"a": 1}     # true   — map KEY
"ell" in "hello"    # true   — substring
```
