# Chezzi — Language Gaps

Known limitations discovered by writing a real program (`examples/stats.chz` — merge sort + binary
search + stats) and probing the language against what an everyday app needs. Each entry lists the
**probe** that surfaced it, **what it blocks**, and a **fix sketch** where the path is clear.

> Method: small `.chz` snippets run through both engines (`chezzi run` / `--interp`). "Verified"
> means observed, not inferred from the cheat-sheet (`docs/syntax.md` is aspirational — several
> documented features below are not built yet).

Legend: 🔴 blocks real apps · 🟡 notable friction · 🟢 works (recorded so we don't re-flag it).

Last updated: 2026-06-06. Baseline: post-M6c (native stdlib seam).

> **Status: all flagged gaps (#1–#9) are now ✅ FIXED.** Both engines (tree-walk `interp` + bytecode
> `vm`) stay in lockstep, verified by the parity + conformance suites (569 tests green). Each gap
> landed TDD with golden `examples/*.chz` run under both engines. The one explicit deferral is
> `sort_by` (HOF comparator) — expressible via `fold` meanwhile.

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

### 3. ~~No higher-order function parameters — `f: fn(int) -> int`~~ ✅ FIXED
```chezzi
fn apply(f: fn(int) -> int, v: int) -> int:   # now parses & type-checks
    return f(v)
```
**Fixed:** added `Type::Func { params, ret }` AST node; `parse_type` parses a `fn(T, …) -> R` form
in type position (shared `parse_type_postfix` for `?`/`!`); `resolve_type` lowers it to the existing
`Ty::Func`. Calling already worked via `infer_call`. See `examples/hof.chz`.
**Blocks:** writing `map`/`filter`/`fold`, comparators, callbacks, strategy objects. Closures *exist*
as values (`inc := fn(x: int) -> int: x + 1` works and is callable), but a function parameter can't
be **typed** to receive one — and parameter types are required. So user-defined higher-order
functions are effectively impossible, and the `|>` pipe examples in the cheat-sheet
(`filter(fn(x): ...)`) can't actually be written.
**Fix sketch:** parse a `fn(T, ...) -> R` form in type position → the checker already has
`Ty::Func { params, ret }`, so the type lattice is ready; mostly a parser + `Type` AST-node + a
`Type → Ty` lowering addition.

### 4. ~~Lists have only `push` / `len`~~ ✅ FIXED
```chezzi
xs.map(f)    xs.filter(p)    xs.fold(init, f)            # higher-order (need gap #3, now built)
xs.pop()     xs.reverse()    xs.sort()                   # pop→Option, sort ascending in place
xs.contains(x)  xs.index_of(x)  xs.sum()                 # all built on both engines
```
**Fixed (phases A + B1 + B2):** added to the three lockstep tables (`interp/builtins.rs`
`list_method` + `vm` `core_method` + checker `list_method_sig`): `pop()→T?`, `reverse()`,
`contains(x)`, `index_of(x)→int`, `sum()` (numeric), `sort()` (ascending, int/float/str, in place),
and the HOF methods `map`/`filter`/`fold` (typed off the closure's `Ty::Func` in `infer_method_call`;
VM runs the closure per element via re-entrant `invoke_value`, keeping source+result rooted on the
operand stack across calls — proven by `gc_stress` parity tests). `pop` makes a list a usable stack.
`sort`-with-comparator (`sort_by`) deferred — expressible via `fold` meanwhile.
See `examples/list_methods.chz`, `examples/list_hof.chz`.

### 5. ~~No map / dictionary type~~ ✅ FIXED
```chezzi
m := {"a": 1, "b": 2}   # insertion-ordered map literal; {} is empty
m["c"] = 3              # keyed insert/update; m["a"] read errors on a missing key
m.get("z")  m.has("a")  m.keys()  m.values()  m.remove("a")  m.len()
```
**Fixed:** new `{`/`}` tokens (no block ambiguity — blocks are indent-based); `Ty::Map(K,V)`,
`Value::Map`/`Obj::Map` as an insertion-ordered `Vec<(K,V)>` (deterministic; `Value` isn't `Hash`);
keys restricted to `int/str/bool`. `m[k]` read/`m[k]=v` write reuse the index ops — the compile-time
`Op::AsInt` was removed and int-validation moved into the runtime `get_index`/`set_index` (list/str
keep the exact `expected int` error; map does key lookup). `Heap::children` traces keys **and** values
(gc-stress tested). `m[k]` missing → runtime error; `m.get(k)` → `Option[V]`. See `examples/map.chz`.

---

## 🟡 Notable friction

### 6. ~~`match` matches enum variants only — no literals, no wildcard~~ ✅ FIXED
```chezzi
match n:
    0: return "zero"     # int/str/bool literal arms now parse & type-check
    _: return "other"    # wildcard catch-all
```
**Fixed:** `Pattern` gained `Literal(LitPattern{Int,Str,Bool})` + `Wildcard` (wildcard reuses the
`_` identifier — no new token). Checker `MatchKind { Variants, Literal(Ty), Skip }`: literal arms
require the scrutinee be `int/str/bool` and each literal's type match; open literal domains need a
`_` arm for exhaustiveness; a wildcard makes any match exhaustive; mixing literal and variant arms is
rejected; `float` scrutinees still rejected. Both forms (statement + expression). Compiler lowers
literal matches with no `EnsureEnum` and **no new opcode** (`Eq` + `JumpIfFalse`); variant matches
keep `MatchArm`/`MatchNoArm`. See `examples/match_value.chz`.

### 7. ~~No `break` / `continue`~~ ✅ FIXED
```chezzi
for i in 0..5:
    if i == 3: break       # exits the loop
    if i == 1: continue    # skips to the next iteration
```
**Fixed:** new `break`/`continue` keywords + `StmtKind`; checker `loop_depth` rejects them outside a
loop; interp `Flow::Break`/`Continue` (loops intercept, `continue` falls through to the increment);
compiler `LoopCtx { continue_jumps, break_jumps }` patches `break`→loop-exit and `continue`→the
**increment** (range `i+=1` / list index advance) — never the bare condition, so `continue` can't
spin. No new opcode (reuses `Op::Jump`). See `examples/loops.chz`.

### 8. ~~No tuples / multiple return values~~ ✅ FIXED
```chezzi
t := (1, 2)                    # tuple literal (2+ elements; `(e,)` rejected, `(e)` is grouping)
fn pair() -> (int, int):       # tuple return type → multi-return
    return (3, 4)
a, b := pair()                 # destructuring let
x := t.0                       # field-style element access (.0, .1, …)
```
**Fixed:** `Type::Tuple`/`Ty::Tuple`/`ExprKind::Tuple`; `StmtKind::Let` carries `names: Vec<String>`
(single binding = one name, destructure = many). `Value::Tuple`/`Obj::Tuple` (immutable, GC-traced),
`Op::NewTuple`; `.N` access reuses `ExprKind::Field`/`Op::GetField` (postfix `.` now accepts an int).
Destructure compiles to a hidden local + per-element `GetField`. Out of scope: 1-tuples, unit `()`,
nested-pattern destructure, `a,b = …` reassignment, runtime `t[i]`. See `examples/pair.chz`.

### 9. ~~`+=` / `-=` allow implicit int→float widening~~ ✅ FIXED
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
**Fixed:** `check_assign_value`'s `PlusEq | MinusEq` arm now rejects `int <op> float` into a concrete
`int` slot (`widens` guard), mirroring the strict `Eq`/`compatible` path. Applies to all targets
(bare vars + index + field). Widening the other way (`int` into a `float` slot) stays allowed.

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

## Resolution order (all done)

1. ~~**#1 + #2 (index / field assignment)**~~ ✅ **DONE (post-M6)** — one assignment-target rule
   across checker + both engines; unlocked mutable arrays and objects.
2. ~~**#9 (strict compound assignment)**~~ ✅ — checker-only `widens` guard.
3. ~~**#3 (HOF params)**~~ ✅ — `Type::Func` AST + parser + `resolve_type` lowering.
4. ~~**#4 (list methods)**~~ ✅ — `pop`/`reverse`/`contains`/`index_of`/`sum`/`sort` + HOF
   `map`/`filter`/`fold` (re-entrant `invoke_value`, GC-rooted, gc-stress tested).
5. ~~**#6 (literal/wildcard `match`)**~~ ✅ — `Pattern::Literal`/`Wildcard`, no new opcode.
6. ~~**#7 (break/continue)**~~ ✅ — `Flow`/compiler `LoopCtx`; for-`continue` lands on the increment.
7. ~~**#5 (map type)**~~ ✅ — `{}` literals, `Obj::Map` insertion-ordered, index ops + methods, GC.
8. ~~**#8 (tuples + multi-return + destructuring)**~~ ✅ — `(a,b)`, `(int,int)`, `a,b := …`, `.0`.
