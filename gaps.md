# Chezzi — Language Gaps

Known limitations discovered by writing a real program (`examples/stats.chz` — merge sort + binary
search + stats) and probing the language against what an everyday app needs. Each entry lists the
**probe** that surfaced it, **what it blocks**, and a **fix sketch** where the path is clear.

> Method: small `.chz` snippets run through both engines (`chezzi run` / `--interp`). "Verified"
> means observed, not inferred from the cheat-sheet (`docs/syntax.md` is aspirational — several
> documented features below are not built yet).

Legend: 🔴 blocks real apps · 🟡 notable friction · 🟢 works (recorded so we don't re-flag it).

Last updated: 2026-06-06. Baseline: post-M6c (native stdlib seam).

---

## 🔴 Blocking gaps

### 1. ~~No index assignment — `xs[i] = v`~~ ✅ FIXED (post-M6)
```chezzi
xs := [0, 0, 0]
xs[1] = 5        # now mutates in place; `+=`/`-=` too
```
**Was blocking:** in-place arrays, DP tables, sieve, counting sort. **Fixed:** `check_assign`
accepts an `Index` LHS (list only — str index-assign rejected as immutable); interp does
`borrow_mut()[i] =`; VM gained `Op::SetIndex` (+ `Dup2` for compound) mutating the heap `Obj::List`.
See `examples/mutate.chz`.

### 2. ~~Struct fields are immutable after construction — `p.x = 5`~~ ✅ FIXED (post-M6)
```chezzi
p := P(1, 2)
p.x = 5          # now mutates in place; `+=`/`-=` too
```
**Was blocking:** stateful objects — accumulators, mutable state, builders. **Fixed:** `check_assign`
accepts a `Field` LHS (struct data fields only — methods/module members rejected); interp does
`fields.borrow_mut()`; VM gained `Op::SetField` (+ `Dup` for compound) mutating the heap `Obj::Struct`.

### 3. No higher-order function parameters — `f: fn(int) -> int`
```chezzi
fn apply(f: fn(int) -> int, v: int) -> int:   # parse error: expected identifier, found 'fn'
    return f(v)
```
**Blocks:** writing `map`/`filter`/`fold`, comparators, callbacks, strategy objects. Closures *exist*
as values (`inc := fn(x: int) -> int: x + 1` works and is callable), but a function parameter can't
be **typed** to receive one — and parameter types are required. So user-defined higher-order
functions are effectively impossible, and the `|>` pipe examples in the cheat-sheet
(`filter(fn(x): ...)`) can't actually be written.
**Fix sketch:** parse a `fn(T, ...) -> R` form in type position → the checker already has
`Ty::Func { params, ret }`, so the type lattice is ready; mostly a parser + `Type` AST-node + a
`Type → Ty` lowering addition.

### 4. Lists have only `push` / `len`
```chezzi
xs.map(...)  xs.filter(...)  xs.contains(x)  xs.index_of(x)
xs.pop()     xs.reverse()    xs.sort()       xs.sum()
# all: type error — type list[int] has no method '<name>'
```
**Blocks:** every list operation is a hand-rolled loop. No `pop` means a list can't serve as a
**stack** — kills iterative DFS, RPN/shunting-yard, backtracking with an explicit stack. No `sort`
means re-implementing it each time.
**Fix sketch:** the core-method dispatch already exists (M6a lockstep: `interp/builtins.rs`
`list_method` + `vm` `core_method` + checker `list_method_sig`). Add `pop`/`reverse`/`contains`
there directly. `map`/`filter`/`sort`-with-comparator depend on gap #3 (HOF params).

### 5. No map / dictionary type
```chezzi
m := {"a": 1}    # documented in the cheat-sheet, but `map[K,V]` is not built
```
**Blocks:** frequency counts, memoization, adjacency-by-key, dedup, any keyed lookup. Worked around
only by parallel lists + linear scan (O(n) lookups).
**Status:** the spec lists `std.map` as "later". No `Value::Map` in either engine yet.

---

## 🟡 Notable friction

### 6. `match` matches enum variants only — no literals, no wildcard
```chezzi
match n:
    0: return "zero"     # parse error: expected identifier, found integer 0
    _: return "other"    # type error: '_' is not a variant
```
**Impact:** can't `match` on int/str/bool values, and can't write a catch-all arm — every `match`
must enumerate all variants (exhaustiveness is otherwise a nice property). Forces `if`/`else if`
chains for value dispatch.

### 7. No `break` / `continue`
```chezzi
for i in 0..5:
    if i == 3: break       # type error: unknown name 'break'
    if i == 1: continue    # type error: unknown name 'continue'
```
**Impact:** loops exit only via their condition or an enclosing `return`. Early-exit search loops
must be restructured (flag variable, or factor into a function and `return`).

### 8. No tuples / multiple return values
```chezzi
t := (1, 2)              # parse error: expected ')', found ','
fn pair() -> (int, int): ...   # unsupported
```
**Impact:** a function returns exactly one value. Returning a pair needs a `struct` or a 2-element
list. No destructuring (`a, b := ...`).

### 9. `+=` / `-=` allow implicit int→float widening
```chezzi
x: int = 5
x += 1.5        # accepted; x becomes 2.5 (a float in an int-typed slot)
xs := [1, 2, 3]
xs[0] += 1.5    # accepted; xs becomes [2.5, 2, 3] — a float in a list[int]
```
**Impact:** the spec says "no implicit int→float", and plain `=` enforces it (`xs[0] = 1.5` is
rejected). But `check_assign_value`'s compound arm gates on `is_numeric && is_numeric`, so `+=`/`-=`
silently widen. Pre-existing for bare variables; the index/field-assignment work (gaps #1/#2) newly
exposes it to `list[int]`/struct fields, where it can quietly poison an int array in a counting-sort
/ DP loop.
**Fix sketch:** in `check_assign_value` (`src/checker/mod.rs`), the `PlusEq | MinusEq` arm should
require the numeric result type to stay compatible with `target_ty` (reject `int <op> float` when the
target is a concrete `int`), mirroring the strict `Eq`/`compatible` path. Applies to **all** targets
(bare vars + index + field) for consistency — own change, own tests.

---

## 🟢 Verified working (so we don't re-flag)

- **Struct equality** `P(1,2) == P(1,2)` → structural compare (`true`/`false`).
- **String indexing** `s[i]` → a 1-char `str`; `s.len()`, `s.upper/lower/trim/split/join/contains/starts_with`.
- **List-of-structs** `[P(1,1), P(2,2)]`, field access `ps[1].y`.
- **Nested-list read** `g[i][j]`; **by-reference sharing** — a list passed to a function and
  `.push`ed is mutated for the caller.
- **`if` / `match` as expressions**, incl. inside string interpolation `"{if a>b: a else: b}"`.
- **`Result` / `Option` + `?`**, exhaustive-match checking, deep recursion, integer overflow → error
  (not wrap), int division truncation, `%` on negatives.
- **`std.math` / `std.io` / `std.os`** native modules and **`std.str`** (Chezzi), on both engines.

---

## Suggested priority

1. ~~**#1 + #2 (index / field assignment)**~~ ✅ **DONE (post-M6)** — one assignment-target rule
   across checker + both engines; unlocked mutable arrays and objects.
2. **#3 (HOF params)** — the `Ty::Func` machinery already exists; mostly parser work. Unblocks #4's
   `map`/`filter`/`sort` and the documented pipe idioms.
3. **#4 `pop` (+ `reverse`/`contains`)** — trivial additions to the existing core-method tables;
   `pop` alone unblocks stack-based algorithms.
4. **#6 literal/wildcard `match`** — removes the most common "why won't this compile" for newcomers.
