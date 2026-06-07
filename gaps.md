# Chezzi — Language Gaps

Known limitations discovered by writing a real program (`examples/stats.chz` — merge sort + binary
search + stats) and probing the language against what an everyday app needs. Each entry lists the
**probe** that surfaced it, **what it blocks**, and a **fix sketch** where the path is clear.

> Method: small `.chz` snippets run through both engines (`chezzi run` / `--interp`). "Verified"
> means observed, not inferred from the cheat-sheet (`docs/syntax.md` is aspirational — several
> documented features below are not built yet).

Legend: 🔴 blocks real apps · 🟡 notable friction · 🟢 works (recorded so we don't re-flag it).

Last updated: 2026-06-06. Baseline: post-M6c (native stdlib seam).

> **Forward-looking brainstorm** (defer, a non-Go concurrency model, missing scripting features,
> VM/GC optimizations) lives in **[`docs/future.md`](docs/future.md)** — speculative, NOT scheduled.
> Promote items here into `gaps.md` once they're committed work.

> **Status: round-1 gaps (#1–#9) are all ✅ FIXED.** Both engines (tree-walk `interp` + bytecode
> `vm`) stay in lockstep, verified by the parity + conformance suites (569 tests green). Each gap
> landed TDD with golden `examples/*.chz` run under both engines.
>
> **Round 2 (#10–#15): ✅ ALL FIXED.** A second probing pass — real DSA + apps (`examples/bst.chz`,
> `linked_list.chz`, `knapsack.chz`, `calc.chz`, `word_freq.chz`) — surfaced six new gaps. Each was
> observed on both engines (exact errors quoted below), not inferred. All six now land TDD, both
> engines in lockstep, with golden `examples/*.chz` (sort_by, cipher, knapsack, word_freq,
> match_nested, bits) committed and run under both engines. 646 tests green (parity + conformance).

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

### 10. ~~No character access — no `ord` / `chr`~~ ✅ FIXED — `ord(s)→int`, `chr(n)→str` builtins
```chezzi
print(ord("a"))   # type error (line 2, col 11): unknown name 'ord'
print(chr(65))    # type error (line 2, col 11): unknown name 'chr'
s := "abc"
c := s[0]         # c == "a"  (a 1-char str, NOT a codepoint)
print(int(c))     # runtime error: int(): cannot parse 'a' as an integer
```
**Probe:** `examples/calc.chz` — a recursive-descent evaluator that *works*, but only because the
input is pre-tokenised with spaces (`"3 + 4 * 2"` → `.split(" ")`). A Caesar cipher / ROT13, a JSON
or CSV scanner, base conversion, run-length encoding, any "classify this character" loop — all dead.
**Blocks:** real lexers/tokenisers, ciphers, char-frequency, parsing `"3+4*2"` without spaces. You
can *read* a character (`s[i]` → 1-char str) and compare it (`c == "+"`), but you cannot map it to a
number or shift it: there is no `'a'..'z'`, no `ord`/`chr`, no `c.is_digit()`, no digit value.
**Fixed:** two builtins — `ord(s: str) -> int` (first codepoint, errors on empty) and
`chr(n: int) -> str` (errors on an invalid codepoint) — registered in the same lockstep tables as
`len`/`range` (interp `builtins.rs` `is_builtin`/`call` + compiler `is_builtin` + vm `do_builtin` +
checker `infer_named_call`). Enables ciphers, scanners, digit classification (`ord(c) - ord("0")`).
A real `char` type / `s.chars()` stays deferred (bigger type-system change). See `examples/cipher.chz`.

### 11. ~~No `sort_by` / comparator~~ ✅ FIXED — `xs.sort_by(fn(T, T) -> int)`, stable, in place
```chezzi
xs.sort_by(fn(a: int, b: int) -> int: b - a)
# type error (line 3, col 5): type list[int] has no method 'sort_by'
```
**Probe:** `examples/word_freq.chz` — counting words into `map[str,int]` is easy; printing the
**top-N by count** is not. With no comparator you cannot sort `(word, count)` pairs, so the program
falls back to a repeated linear argmax (O(n²)) with a `used` set. A Dijkstra / Prim shortest-path
hits the same wall (no way to keep a frontier ordered by distance, no priority queue), as does
"sort these structs by a field" or "sort strings by length".
**Blocks:** top-N ranking, leaderboards, priority queues, Dijkstra/Prim/Huffman, any non-natural
ordering or sort-by-key. (Already noted as the lone round-1 deferral; round 2 confirms it is the
single most-felt missing method — promote from "deferred" to a real 🔴.)
**Fixed:** `xs.sort_by(fn(T, T) -> int)` (negative = a before b) added to the HOF path in all three
engines (checker `infer_list_hof`, interp `eval_list_sort_by`, vm `list_sort_by`). Because the
comparator is fallible and re-enters the engine, a **stable merge sort** drives it rather than
`slice::sort_by`; the VM permutes plain `usize` indices while the source list stays GC-rooted on the
operand stack (gc-stress tested). `sort_by_key` deferred (sugar). See `examples/sort_by.chz`,
`examples/word_freq.chz` (top-N by count).

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

### 12. ~~`std.math` `abs` / `min` / `max` are float-only~~ ✅ FIXED — int+float polymorphic
```chezzi
import std.math
print(math.max(3, 5))   # type error: argument 1 of 'max': expected float, found int
print(math.abs(-5))     # type error: argument 1 of 'abs': expected float, found int
```
**Probe:** `examples/knapsack.chz` — the DP recurrence wants `max(without, with_it)` over an int
table, but `std.math.max` only takes floats, so every `max`/`min` is spelled out as a 4-line
`if a > b: a else: b`. `abs` on an int (gcd, distance, "how far off") needs the same hand-rolling or
a `float(...)`/`int(...)` round-trip that risks precision and reads badly.
**Blocks:** clean int DSA — DP, gcd, Manhattan/abs distance, clamping. Not *blocking* (workaround is
trivial), but it is friction in the most common numeric code.
**Fixed:** `abs`/`min`/`max` are now numeric-polymorphic (int args → int, float args → float; a
mixed int/float call is rejected, consistent with no implicit widening). The native seam grew a
`Host::arg_is_int` and the fns branch to `NativeRet::Int`/`Float`; the checker special-cases the
three (`ModuleSig::numeric_poly` + `infer_numeric_poly`). Other `std.math` fns stay float-only.
Full generics / an ordering trait for user types stay deferred (future milestone). See
`examples/knapsack.chz` (DP table uses `math.max`).

### 13. ~~No bitwise operators~~ ✅ FIXED — `&` `|` `^` `<<` `>>` (int-only)
```chezzi
print(5 ^ 3)   # resolve error (line 2): lex error: unexpected character '^'
```
**Probe:** "find the single number" (XOR-fold a list where every value but one appears twice) — the
canonical O(n)/O(1) trick is unwritable. So are bitmask DP, subset enumeration (`1 << n`), bitset
sieves, hashing, parity/popcount, and packing flags.
**Blocks:** bit-manipulation DSA and any low-level integer work. The lexer doesn't even tokenise the
symbols (`^`, `<<`, `>>` fail at lex time; `&`/`|` would too).
**Fixed:** new lexer tokens (`Amp`/`Caret`/`BitOr`/`Shl`/`Shr`; bare `|` now lexes, `|>` still the
pipe) + `BinaryOp` variants + int-only checker rules + interp/vm ops, plus `docs/grammar.bnf` (drift
-checked by conformance). Precedence follows Python (comparison looser than `|`<`^`<`&`<shifts<add).
A shift amount outside `0..64` is a runtime error in both engines (no Rust panic). See
`examples/bits.chz` (XOR-fold single-number + bitmask).

### 14. ~~Maps aren't iterable in a `for` loop~~ ✅ FIXED — `for k in m` and `for k, v in m`
```chezzi
m := {"a": 1, "b": 2}
for k in m:        # type error: cannot iterate over map[str, int]
    print(k)
for k in m.keys(): # required form
    print(k)
```
**Probe:** `examples/word_freq.chz` iterates `counts.keys()`. Minor, but every map walk is wordier
than range/list iteration, and there is no `for k, v in m:` key+value form — you re-`m[k]` (or
`m.get(k)`) inside the loop for the value.
**Blocks:** nothing (`.keys()` is a clean workaround), pure ergonomics.
**Fixed:** `StmtKind::For` now carries `vars: Vec<String>`; the parser reads a comma-separated list,
the checker (`for_bindings`) binds the **key** for one var and **key+value** for two (two-var form
requires a map). The VM normalises the iterand with `ListClone` (list → clone, map → keys) and looks
up the value via the existing `GetIndex`; no `continue`/`break` retargeting changes. See
`examples/word_freq.chz`.

### 15. ~~`match` has no nested / tuple patterns~~ ✅ FIXED — nested + tuple patterns
```chezzi
t := (1, 2)
match t:
    (a, b): print(a + b)   # parse error (line 3, col 9): expected identifier, found '('
```
**Probe:** `examples/bst.chz` / `linked_list.chz` — every step down a tree/list is a full
`match root: None: ... Some(n): ...` block; you cannot write `Some((x, y))`, `Cons(h, Some(t))`, or
match a tuple's shape directly. Tuple destructuring works in `let` (`a, b := pair()`) but not in a
`match` arm.
**Blocks:** nothing outright (nest `match`es or destructure after binding), but recursive
data-structure code is markedly more verbose than the Python/Rust feel the language targets.
**Fixed:** `Pattern` generalised — variant `bindings` became `Vec<Pattern>`, plus `Pattern::Tuple`
and `Pattern::Ident` (a sub-position binding name). Parser recurses (`parse_subpattern`/
`parse_tuple_pattern`); checker recurses (`bind_subpattern`, new `MatchKind::Tuple`); interp uses a
recursive `try_bind`; the compiler lowers via a recursive `emit_pattern` reusing `MatchArm`
(variant), `GetField` (tuple element), and `Eq`+`JumpIfFalse` (literal) — no new opcodes. Nested
nullary variants (`Cons(h, None)`) stay unsupported (clear checker error); **match guards** and
**range patterns** stay deferred (future). See `examples/match_nested.chz`.

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
- **Recursive / self-referential structs** — `struct Node: val: int; left: Node?; right: Node?` and
  `struct Cell: val: int; next: Cell?` build, walk, and GC fine (the checker's two-pass name
  collection resolves the self-reference). BST insert/in-order/height + linked-list reverse run
  identically on both engines. See `examples/bst.chz`, `examples/linked_list.chz`.
- **Mutable `self` across method calls** — a method doing `self.pos += 1` persists for the caller
  (struct is one heap object by reference); recursive-descent parser cursor relies on it.
  See `examples/calc.chz`.
- **Nested-list DP** — `list[list[int]]` built with `push`, filled with two-level
  `dp[i][w] = ...` index assignment. See `examples/knapsack.chz`.
- **Empty map literal infers `K,V` from later use** — `m := {}` then `m["a"] = 1` type-checks
  (no annotation needed).
- **Space-tokenised recursive-descent parsing** with mutually-recursive struct methods
  (`expr → term → factor → expr`). See `examples/calc.chz`.

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

## Resolution order (round 2 — all done)

1. ~~**#11 (`sort_by`)**~~ ✅ — stable merge sort over a re-entrant comparator, GC-rooted on the VM.
2. ~~**#10 (`ord` / `chr`)**~~ ✅ — two builtins in the `len`/`range` lockstep tables.
3. ~~**#12 (int `min`/`max`/`abs`)**~~ ✅ — numeric-polymorphic native fns + checker `numeric_poly`.
4. ~~**#14 (map iteration)**~~ ✅ — `For.vars: Vec<String>`; `for k` / `for k, v` over a map.
5. ~~**#15 (nested match patterns)**~~ ✅ — recursive `Pattern` + `MatchKind::Tuple`; no new opcodes.
6. ~~**#13 (bitwise ops)**~~ ✅ — lexer→parser→checker→both engines + `grammar.bnf` (conformance).

## Deferred to future milestones

- ~~**Generics / operator overloading** (extends #12)~~ ✅ **M7 (G1 + G2)** — generic functions
  (`fn max[T: Comparable]`) **and** generic structs (`Pair[A, B]`, `Stack[T]`) + Go-style
  structural **protocols** (type-erased; all work in the checker, runtime barely changed). Prebuilt
  `Comparable` wires `< <= > >=` to a user `compare` method (the sole operator overload; `==`/`!=`
  stay structural). See `examples/generics.chz`, `examples/generic_structs.chz`, `docs/syntax.md`
  §7b. **M7-G3** then unified the stdlib onto it: `min`/`max`/`clamp` are generic `[T: Comparable]`
  functions in pure-Chezzi `std.cmp` (native numeric `std.math.min`/`max` + the `numeric_poly` hack
  removed; `abs` stays native), and `list.sort()` widened to any Comparable element. See
  `examples/stdlib_cmp.chz`. (Since shipped: explicit call-site type args `max[int](…)`, generic
  enums, multi-bound `T: A + B`, a numeric protocol via `Add`/`Sub`/`Mul`, `Hashable`/`Stringable`.)
## Roadmap to a complete v1 (statically-typed scripting language)

The language **core** is feature-complete: scalars, `list`/`map`/`tuple`, structs (generic), sum
types (`enum` with payloads), `Result`/`Option` + `?`, generics + structural protocols, pattern
matching with exhaustiveness, closures/HOF, struct methods, modules, GC, two backends, string
interpolation, pipe. What remains to make it a language you'd reach for to write real scripts is
**~80% standard-library breadth, ~20% type-system depth**, ordered below by leverage.

### Tier 1 — blocks everyday scripting (mostly stdlib) — ✅ **DONE (M8 + M9)**
- ~~**`std.json`** — parse/serialize.~~ ✅ **M8.** A pure-Chezzi `Json` enum (`parse`/`stringify`/
  `as_*`/`get`/`at`) **plus** type-directed `json.decode[T](s)` into a struct / typed map / list /
  scalar. Sidestepped the `Display`/`Hashable` dependency the original note feared — a dedicated
  `Json` enum makes `stringify` a plain `match` and keeps keys `str`. See `examples/json_dynamic.chz`,
  `examples/json_decode.chz`.
- **More stdlib:** ✅ `std.time` (`now`/`monotonic`/`sleep_ms`/`format`), ✅ `std.fs`
  (`list_dir`/`exists`/`is_file`/`is_dir`/`size`/`glob`), ✅ `std.process` (`cmd(s) -> Result[str]`).
  ✅ **M9 — `std.regex`** (the `regex` crate; `is_match`/`find`/`find_all`/`replace_all`/`split`,
  `Match` struct) and ✅ **`std.request`** (blocking HTTP/HTTPS via `ureq`+rustls; `get`/`post` →
  `Result[Response]`). These took the project's first runtime deps (the seam grew
  `NativeRet::Struct`/`Map` so native fns can return structured values). See `examples/sys.chz`,
  `examples/regex_demo.chz`, `examples/request_demo.chz`.
- ~~**A real `char` type / `s.chars()`**~~ ✅ **M8 — Python-style, no `char` type.** Added
  `s.chars() -> list[str]` and made strings iterable (`for c in s:`); a character stays a 1-char
  `str` (like Python). See `examples/string_iter.chz`.
- ~~**`set` type**~~ ✅ **M8.** `{a, b, c}` literals (deduped, insertion-ordered), `set()`/`set(list)`,
  `add`/`remove`/`has`/`len`/`union`/`intersection`/`difference`, iteration, order-independent
  equality; elements are hashable scalars. See `examples/set.chz`.

### Tier 2 — type-system depth
- ~~**Generic enums** — `enum Tree[T]`, `enum LinkedList[T]`.~~ ✅ **DONE.** Enums now carry type
  parameters exactly like generic structs (M7-G2): `Ty::Enum(String, Vec<Ty>)`, parser reads
  `parse_type_params` after the name, the checker enters the params over variant payloads, infers
  type args from the constructor (`unify`), substitutes them into variant payloads at `match`
  (`enum_param_map` + `subst`), and enforces bounds (`enum Box[T: Comparable]`). **Type-erased** —
  zero compiler/VM change; identical runtime per instantiation. `Result`/`Option` stay hardcoded
  (`Ty::Result`/`Ty::Option`) and coexist. See `examples/generic_enum.chz` (`Tree[T]` at int+str,
  `Either[A, B]`), `docs/syntax.md` §8.
- ~~**`Hashable` protocol** → let structs be `map` keys (maps key only int/str/bool now).~~ ✅
  **DONE (M10-G2 bound + map-model rework).** `map`/`set` were *association lists* (`Vec<(K,V)>`,
  linear scan, no hashing); now they are **real insertion-ordered hash tables** — a `Vec` of
  `(cached_u64_hash, k[, v])` plus a side `HashMap<u64, Vec<usize>>` (hash→positions) for O(1)-avg
  lookup; the cached hash makes index rebuild-after-remove pure (no re-hashing). The key restriction
  is **lifted**: any `Hashable` type is a key/element — int/str/bool intrinsically, or a struct via
  its `hash(self) -> int`, dispatched at runtime and confirmed by structural `==`. The struct-key
  `hash()` re-enters the engine (GC-rooted on the VM operand stack like `sort_by`; Rc-safe on the
  interp). Numeric keys hash by canonical f64 bits (`3`==`3.0`, ±0.0 normalised); float keys stay
  rejected (NaN). Insertion order preserved; both engines byte-identical (parity + gc-stress).
  Contract: structurally-equal structs must return equal `hash()` (user-owned, like Rust Hash/Eq).
  See `examples/hashmap_keys.chz`, `docs/syntax.md` §6.
- ~~**`Display`/`Show` protocol** → custom `str(point)` / `print(point)`.~~ ✅ **M10-G1 — shipped as
  `Stringable`.** Prebuilt protocol `Stringable` with `str(self) -> str`; a struct that defines it
  overrides its default repr in `print`, the `str()` builtin, and `{…}` interpolation (nested too).
  Both engines via a protocol-aware `stringify` (the `&self` `display` stays for error/debug text);
  enums keep the built-in repr (no enum methods). Naming: chose `Stringable` over `Display`/`Show`
  to match the `-able` convention and the `str()` builtin. See `examples/stringable.chz`.
- ~~**A numeric protocol** → `+ - *` on user types; multi-bound `T: A + B`; **type aliases**.~~ ✅
  **M10-G3.** Per-operator prebuilt protocols `Add`/`Sub`/`Mul` (method `add`/`sub`/`mul(self,
  other: Self) -> Self`) overload `+`/`-`/`*` on same-typed structs (int/float satisfy intrinsically;
  `/`/`%` never overload). **Multi-bound** `T: Add + Mul` (`TypeParam.bound` → `bounds: Vec`).
  **Type aliases** `type UserId = int` — transparent (structural), resolve in `resolve_type` with a
  cycle guard; reserved/dup names rejected. Both engines dispatch via `run_proto`/`call` (mirrors
  `compare`). See `examples/operators.chz`, `examples/type_alias.chz`. Not done: unifying
  `abs`/`min`/`max` onto the numeric protocol (optional follow-up).

### Tier 3 — runtime robustness + ergonomics
- ✅ **Panic recovery (M11).** `recover:` block → `Result[T, Error]` catches any runtime fault
  (index-OOB, div-by-zero, overflow, missing key) occurring transitively beneath it. try-block
  semantics: a `?` inside short-circuits to the boundary, so one `recover:` handles both panics and
  propagated errors. Errors are now Go-style: `Result[T, E]` (`T!` = `Result[T, Error]`, `T!E`
  explicit), `Error` protocol (`message(self) -> str`) with `str` conforming intrinsically.
- **Iterator protocol** — `for` only iterates built-in containers; a user struct can't be iterable,
  and there are no lazy/generator sequences.
- **Match guards** (`pattern if cond:`) and **range patterns** (`1..10:`) (extend #15): guards are
  the general mechanism (subsume range / less-than / greater-than).
- **Default / named / variadic args**; **`sort_by_key`** (sugar on #11).
- **Integers:** `i64` only — no overflow policy, no `byte`/bignum.

### Tier 4 — ecosystem (toolchain, not the language)
Formatter, LSP, package manager / registry (spec defers this), REPL, debugger, built-in test
runner, doc comments + docgen.

### Known fragilities (tech debt)
- ~~**Parser `MAX_DEPTH` (128) sits at the test-thread stack edge.**~~ ✅ **FIXED** — lowered
  `MAX_DEPTH` 128 → 64. Each recursive-descent frame is ~16 KB, so the old 128 deep frames ≈ 2 MiB
  sat right at a Rust *test* thread's default stack: the `deep_nesting_errors_not_crash` test
  overflowed the host stack before the depth guard could fire (worked around M10-G3 by running it on
  a 64 MiB thread). 64 levels ≈ 1 MiB now leaves real headroom — the guard fires cleanly on the bare
  test thread, so the 64 MiB workaround thread was removed (test runs inline). 64 still far exceeds
  any realistic source nesting; full suite + conformance green. `src/parser/mod.rs`.
- ~~**Duplicate generic type parameter `[T, T]` is silently accepted**~~ ✅ **FIXED.** `parse_type_params`
  (the one chokepoint for `fn`/`struct`/`enum` decls) now rejects a repeated name with a
  `duplicate type parameter '<name>'` parse error before it can reach the last-write-wins checker map.
  Distinct names (`[T, U]`) still parse. `src/parser/mod.rs` (`parse_type_params`); test
  `duplicate_type_param_rejected`.
- ~~**Nested `set` equality diverges across engines (latent parity gap).**~~ ✅ **FIXED.** The interp's
  `SetData::eq` (reached via `Value`'s derived `==` for a set nested in a struct/list) was
  order-*sensitive*; it is now order-*independent* (same-size + every element a member, compared with
  `values_equal`), mirroring the VM's `values_equal` Set arm. So `Wrapper(set([1,2])) ==
  Wrapper(set([2,1]))` is `true` on both engines. `src/interp/value.rs` (`SetData` `PartialEq`);
  golden `examples/set_eq.chz` + parity test `nested_set_equality_parity`.
- ~~**No explicit call-site type arguments** — `max[int](…)` / `Pair[int,str](…)`.~~ ✅ **FIXED.**
  `ExprKind::Call` gained a `type_args: Vec<Type>`; the parser speculatively steals `name[Types](args)`
  when the callee is a bare name (mirrors `.decode[T]()`; numeric/non-type subscripts like `fns[0](x)`
  stay index+call), and the checker seeds the substitution map from the explicit args (validating
  count, with inference filling the rest). **Type-erased** — the runtime ignores `type_args`. Works for
  generic fns, struct constructors, and enum-variant constructors. `src/ast/mod.rs`, `src/parser/mod.rs`
  (`try_parse_type_arg_call`), `src/checker/mod.rs` (`seed_targs`/`name_is_generic`); golden
  `examples/explicit_type_args.chz`. Accepted tradeoff: `arr[identVar](x)` (index-then-call with a name
  subscript) now parses as a type-arg call and the checker errors — use a temp binding.
- ~~**`?` inside a closure isn't checked against the closure's return type**~~ ✅ **FIXED.** Was
  type-unsound: `infer_try` read `self.current_ret` (the enclosing *function*'s return), but
  `infer_closure` never set `current_ret` for the closure body, so a `?` in a closure validated
  against the enclosing function (often `nil`/`main`, allowed) instead of the closure's own return,
  leaking an `Err` into a slot typed otherwise (e.g. `["2","x"].map(fn(s: str) -> int: parse(s)?*2)`
  → `[4, Err(...)]` in a `list[int]`). Fixed by setting `current_ret` to the closure's resolved
  return (or `Unknown` when unannotated) around its body in `infer_closure` (mirrors `check_fn_body`):
  `?` in a non-`Result`/`Option` closure is now rejected at type-check; a `Result`-returning closure's
  `?` is validated like a named fn's. `src/checker/mod.rs` (`infer_closure`).

**Recommended next:** Tier 1, Tier 2, and Tier 3 panic recovery (M11) are shipped, and the
"known fragilities" tech debt is now cleared (dup type-param, nested-set parity, explicit call-site
type args, closure-`?` soundness). The highest-leverage remaining Tier 3 work is the **iterator
protocol** (user structs iterable in `for`; lazy/generator sequences), then **match guards**
(`pattern if cond:`) + **range patterns**.
