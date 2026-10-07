# Design — generics on Rust's model, and `T?` / `T!E` without Rust's names (draft, 2026-10-07)

Status: **draft, owner decisions recorded below; nothing filed yet.** Motivation: sweeps #5 and #6
(`docs/root-causes-w20.md`, `docs/root-causes-w21.md`) kept finding generic/path bugs (2 P0 each wave).
Each came from a fact decided in several places. This doc fixes the architecture once, and at the same
time settles the surface syntax of optional and error values.

Lineage: generics and tagged-union enums come from **Rust**; error handling is **errors as values**
(Rust/Go/Zig), never Python exceptions; scripting feel and `None` from **Python**; the hidden-carrier
surface follows **Zig** (`?T`, `E!T`, `orelse`, `catch`).

---

## Part 1 — Generics and paths: Rust's architecture

### The problem in one table

| bug (wave) | cause |
|---|---|
| CK1 `(math.abs[int])(-6)` check-OK then internal fault (w21 P0) | two places record a resolution for one node (call fallback + value path) |
| CK3 `json.decode[int] == json.decode[int]` is false (w21 P0) | fn-value identity built per kind; decode has no key |
| K1 `P.mk == P.mk` false (w20 P0) | same: identity per read |
| S1 `json.decode[Map[str, int]]` read as an index | `f[X]` parsed by two grammars (type vs expression) |
| S2 `Result[int, str].Ok(5)`, `f := Some` unknown | `Option`/`Result` special-cased, not ordinary enums |
| CK5 `(o ?? g)(5)` rejected | generic value decided at the read, from the hint only |
| FF1 `y: int8 = id(300)` accepted | constant typed before the width hint is seen |

### Rust's model, adopted step by step

**R1. One resolution, one writer.** One decider inside the checker, written once: every path node
(`f`, `lib.f`, `a.b.f`, `T.m`, `Bx[int].make`, `E.A`) gets exactly one resolution (item + kind)
from `resolve_path` (rule table `classify_path`, `src/checker/resolve.rs`), memoised per node.
**Done (TICKET-222, 2026-10-07):** one kind per path in every position (a module fn is `Fn`, a type
method `MethodFn`, a payload variant `VariantFn`; `Resolution::Static` is deleted). The call side
(`infer_call_dispatch`), the value side (`infer_ident`, `infer_field`, the type-applied fn value)
and the compiler only read it: each binds `resolve_path`'s answer and takes its branch from a
`match` on it. What stays beside the match is reject-only: a miss's message, the type-parameter
shadow, a type-only native name. One rule reads the position (`PathPos`): inside `fn P` of a
module declaring `struct P`, the callee `P(..)` is the raw constructor and a value read of `P` is
the fn, as Rust's struct expression `P { x }` and value `P` split by namespace
(owner decision 2026-10-07; a separate pre-pass would need a second scope model, and Rust itself
resolves type-relative paths like `Vec::<i32>::new` / `T::method` during type checking). Deletes: the call-side "defensive fallback"
(`checker/expr.rs:649-718`), the per-shape call arms, and every second writer of the resolutions table.
Makes W7-49 ("two decisions for one position") impossible by construction.

**R2. One bracket node, one type grammar.** Chezzi keeps `[]` for type arguments (owner decision; Go
does the same). The parser stops guessing: `head[X]` becomes one neutral node that carries both
readings, the index expression and `X` parsed by the real type grammar (`Map[str, int]`, `(int, str)`,
`fn(int) -> int`, `lib.Bx[int]`). R1 picks: type application when the head resolves to a generic item or
type, index when the head is a value. Deletes `ast::index_as_type` and its three direct callers.
**Done (TICKET-222, 2026-10-07):** `ExprKind::Index { obj, index, types }`; `TypeApply` and the
parser's comma rule are deleted.

**R3. The carriers are ordinary enums inside.** The optional and error carriers are declared in the
prelude like any user enum and get no special case in the checker or compiler: one model for patterns,
exhaustiveness and lowering. Deletes: the inline `variants_of` arms, the prelude drift assert,
`builtin_ok`, and the extra `resolve_type` arms. Their names are **not** part of the surface (Part 2,
D6): user code never spells `Option`, `Result`, `Some`, `Ok` or `Err`. This internal half of R3 is what
fixes S2's class (special-cased built-ins disagreeing with the path code); S2's surface forms
(`Result[int, str].Ok(5)`, `f := Some`) disappear with the names.

**R3b. Variant import (Rust's `use Enum::Variant`).** `import Red, Green from Color` makes variants bare
names in the importing module, for user enums. A bare variant is still rejected unless imported, as
today (`'Green' is a variant of enum 'Color'; write it qualified`). `None` is the one bare carrier name
(D1); the prelude provides it.

**R4. A fn value is (item, type args).** Rust's fn item value is its `DefId` + substitutions, and
equality follows from that. Chezzi:
- One `FnKey = (item, type args)` for every kind: plain fn, method path, variant, `T.m`, native, extern,
  `json.decode[T]`.
- One compiler memo for synthesized fns, replacing `variant_fns`, `param_method_fns` and the per-site
  decode proto. A synthesized fn reads no home global.
- One classifier `Vm::callable(value)` read by call, spawn, `Executor.submit` and the entrypoint,
  replacing each consumer's hand-listed kinds.

**Done (TICKET-226, 2026-10-07):** one compiler memo `Compiler::synth_fn_proto` keyed by `SynthFn`
(variant ctor, `T.m`, `json.decode[T]` by target shape); the decode value is homed in std.json by
`Op::MakeFuncIn` and reads std.json's own `parse` slot. `FnKey::Builtin` makes `Vm::fn_value` the
allocator of every bare fn kind. `Vm::callable` (an exhaustive `match`, no `_` arm) is read by call,
`lower_task`, `Executor.submit` and the entrypoint; a non-closure spawn callee crosses as
`Lowered::Value`. Grid: `tests/chz/spec/fn_value_grid_test.chz`.

**R5. Type variables, solved across the body.** **Status: done (TICKET-225, 2026-10-07).** A generic value read without a pin gets a type variable,
not an immediate reject. Joins (`if`/`match`/`??`/list/map/set/`==`), call arguments and later uses pin
it; it is rejected only if still unpinned at the end. This is the general form of today's call-argument
deferral, which becomes one case of it.
- An integer or float literal is an **untyped constant** (Go's model) until it meets a slot. A width slot
  (`int8` ...) checks representability inside the one assignability relation, so the hand-placed
  `check_const_fits` list goes away. A constant argument seeds `T` from the expected type first.
- One constant evaluator (all pure ops) replaces `const_int_scan`, `const_float` and the peephole table's
  separate rules.

**Not adopted from Rust:** monomorphization (Chezzi keeps type erasure + witness passing), lifetimes,
the `::<>` syntax.

### Grid tests that gate Part 1

1. Path grid: type-arg shape {`int`, `List[int]`, `Map[str, int]`, `(int, str)`, `fn(int) -> int`,
   `lib.T`, `lib.Bx[int]`, `int?`} x head {local generic, imported, `lib.f`, `a.b.f`, `json.decode`,
   `math.abs`, `Bx[..].static`, `Bx[..].inst`, `E[..].Payload`, `E[..].Nullary`, `T.m`, prelude variants,
   imported variants} x position {call, let, typed let, HOF arg, `(h[X])(..)`, `Head[X].m(..)`, `==`,
   spawn target, `spawn:` capture, default, guard}.
2. Fn-value grid: kind {plain, method path, variant, `T.m`, `json.decode[T]`, closure, native, builtin,
   extern} x consumer {call, spawn callee, spawn arg, submit, submit_result, channel round trip, `==`
   (same site, two sites, two modules), repr, snapshot}, plus a completeness test that fails when a new
   callable kind lacks a consumer arm.
3. Join grid: join {if, elif, match, `??`, list, map, set, `==`} x order x sibling {concrete fn,
   another unpinned generic (must reject)}.
4. Constant grid: every slot from the assignability funnel x constant {literal, folded expression
   (`1 << 8`, `3e38 + 3e38`)} x every width.

---

## Part 2 — Optional and error values: same model, hidden names

The carriers stay ordinary enums inside (R3). No code writes `Option`, `Result`, `Some`, `Ok` or `Err`;
those names are removed from the surface (D6). The table is the whole surface.

| need | spelling | today | ancestor |
|---|---|---|---|
| optional type | `int?` | same (also `Option[int]`, removed) | Zig `?i32`, Swift `Int?` |
| error type | `int!E`, `int!` (= `int!Error`) | same (also `Result[int, E]`, removed) | Zig `E!i32` |
| nothing, or an error | `None!E`, `None!` | `Result[nil, E]`, success `Ok()` | Zig `E!void` |
| absent | `None` | same | Python `None` |
| present / success | the plain value: `x: int? = 5`; explicit `?x` (one level deeper) | `Some(5)` / `Ok(5)`; auto-wrap only at a `return` | Zig, Swift implicit wrap |
| an error value | `!e` | `Err(e)` | Zig `error.X` |
| return an error | `return !e` | `return Err(e)` | Zig `return error.X` |
| pass it up | `f()?` | same | Rust `?`, Zig `try` |
| default | `f() ?? 0` | same | Zig `orelse` / `catch 0` |
| chain | `u?.name` | same | Swift, Kotlin |
| handle and leave | `v := f() else e:` + block | — | Swift `guard`, Rust `let-else` |
| both arms | `match` with `?v` / `!e` / `None` | `Some(v)` / `Ok(v)` / `Err(e)` | Swift `case let v?` |

Error messages print the sugar (`int?`, `str!IoErr`). Generic code needs no long name either:
`fn first[T](xs: List[T]) -> T?`, `Map[str, int!E]`.

### D1. One "nothing" word: `None` (owner decision 2026-10-07)

`nil` is deleted. `None` is used in two positions that never collide, as in Python:

| position | meaning | example |
|---|---|---|
| type | returns nothing (void) | `fn log(msg: str) -> None` |
| value | absent, only where a `T?` is expected | `x: int? = None` |

`None` as a type is an annotation only, **not** Python's "returns the value None": a `-> None` function
produces no value at all. The safety rule stays: a void result is not a value.

| | Python | Chezzi |
|---|---|---|
| `x := log("hi")` | `x = None`, silently | error: `log returns nothing; it cannot be used as a value` |
| `None` as a value | anywhere | only where a `T?` is expected |
| `None?` as a type | — | rejected (an optional nothing) |

### D2. Errors are values: keep `Error`, hide `Err`; `!e` builds one (owner decisions 2026-10-07)

- `Error` stays: the protocol every error type satisfies (`fn message(self) -> str`), like Go's `error`.
  `int!` means `int!Error`.
- `Err(e)` is gone. The prefix `!` builds an error value: `!IoErr("disk")`, `!e`. Its operand must
  satisfy `Error` (`!5` is an error: `int does not satisfy Error`). It applies to the whole operand:
  `!wrap(e)` = the error `wrap(e)`.
- Returning an error is `return !e`. There is **no `fail` keyword**: `return !e` says it, and the `!`
  is what tells success from failure in `str!str` (`return "x"` vs `return !"x"`).
- `!e` is an ordinary expression, so an error value can be built without leaving:
  `rs: List[int!IoErr] = [1, !IoErr("disk"), 3]`.

```
fn load(p: str) -> str!IoErr:
    if p == "":
        return !IoErr("empty")
    return "data"                # implicit success wrap
fn save(p: str) -> None!IoErr:   # nothing on success (Zig `IoErr!void`)
    if p == "":
        return !IoErr("empty")
    write(p)                     # falling off the end = success
save("a")?                       # statement: fine
x := save("a")?                  # error: save returns nothing on success
```

"Nothing, or an error" is `None!E` (and `None!` for any `Error`), the direct translation of Zig's
`E!void` with `None` as the void type (D1). Not `!E`: Zig's leading `!` (`!void`) drops the error set,
the opposite meaning.

### D3. Implicit wrap at every typed slot

A plain `T` value wraps into `T?` / `T!E` wherever the expected type is known: a binding, an argument, a
field, a collection element, a return, a yield. Today this happens only at a `return`. Rules kept from
the current success-coercion: a value that is already a carrier is never re-wrapped; no chaining with
the int→float rule (D3 of TICKET-138, still no widening); it declines where the expected type mentions an
unpinned type parameter.

**Explicit wrap `?x`.** The prefix `?` builds a present / success value; which carrier comes from the
expected type (`int?` → present, `int!E` → success). Implicit wrap makes it rare: it is needed only to
go one level deeper. Nested carriers: `x: int?? = None` is the **outer** `None` (Zig picks the same);
`x: int?? = ?None` is the inner one (Rust `Some(None)`). `None`'s own `T` is inferred from where it
lands, as in Rust, so `?None` works for any `T`.

**No expected type** (owner decision 2026-10-07). With R5, "no expected type" means nothing in the
whole function pins it, not just the line:

| expression | known | unknown | rule |
|---|---|---|---|
| `y := ?5` | the value type `int` | which carrier | default `int?` (complete; like Go's untyped `5` → `int`) |
| `w := !"disk"` | the error | the success type `T` | a later use may pin it (`return w` in an `int!` fn); never pinned → error: `cannot infer the success type; write w: int! = !"disk"` |

```
z := ?5
take_result(z)          # take_result(r: int!E) pins z → int!E
e := !"disk"
return e                # the fn returns int! → e: int!Error
```

### D4. `else` = handle and leave (owner decision 2026-10-07: design B)

`v := f() else e: <block>` binds `v` on success. On failure it runs the block, which **must leave**: the
code after it never runs without a valid `v`. It never produces a value; values come only from `??` and
`match`.

Leave statements: `return x` (x matches the function's return type; `return !e` returns an error),
`break` / `continue` (inside a loop), `panic(...)` / `os.exit(n)` (anywhere), or an `if`/`match` whose
every branch leaves. Anything else is a compile error: `else block must leave (return, break, continue,
panic)`.

```
data := load(p) else e:
    log(e.message())
    return None
rows := parse(data) else e: return !Wrap(e)
for s in lines:
    n := to_int(s) else: continue          # T?: no e
cfg := load("app.toml")?                    # top level: an unhandled error ends the run, rc=1
cfg := load("app.toml") else e:             # top level with your own handling
    print("bad config: {e.message()}")
    os.exit(2)
```

| position | rule |
|---|---|
| module top level | no `return` there (already an error): use `?` to end the run with the error, or `panic` / `os.exit` / `break` / `continue` in the guard |
| closure | `return` leaves the closure |
| `spawn:` block | rejected, as `?` already is |
| `defer:` block | rejected (a defer cannot leave) |
| generator | `return` ends the generator; `return !e` only if it yields `T!E` |
| `else e:` on a `T?` | error: absent has no `e`; write `else:` |
| `else:` on a `T!E` | allowed: ignore the error and leave |
| a call returning `None!E` (no value) | still must leave; "log and continue" is a `match` |
| `int?!E` | `else` handles the outer error; `v` is `int?` |

Jobs and their one tool:

| job | tool | flow |
|---|---|---|
| pass the error up | `?` | leaves |
| default, error ignored | `??` | continues with a value |
| log / wrap / skip, then continue with the value unnested | `else` | leaves on failure |
| default computed from the error; two long flows | `match` | both arms |

Dropped from the plan: `if v := x:` (it brings back the nesting `else` removes) and a force-unwrap
operator.

### D5. Patterns: one compare-vs-bind rule (owner decisions 2026-10-07)

Principle (Rust's): a pattern **compares the outer tag** and **binds only inside it**. A bare name is the
only pattern that binds without comparing, and it is the default arm.

| pattern | compares | binds |
|---|---|---|
| literal `1`, `"hi"` | the value | — |
| `_` | nothing (default arm) | — |
| bare lowercase name `other` | nothing (default arm) | the whole value |
| `Color.Red`, `None` | the tag | — |
| `Color.Rgb(r, g, b)`, `Point(x, y)` | the tag | inside |
| **`?v`** | present / success | `v` |
| **`!e`** | error | `e` |
| bare variant `Green` | rejected unless imported (R3b): "write it qualified as `Color.Green`" | — |
| **bare constant `LIMIT`** | **rejected** (today it silently binds, Python's footgun): "`LIMIT` is a constant; to compare write `x if x == LIMIT`, to bind use a new name" | — |

The symbol comes first, then the name: tag then binding, the same order as a constructor pattern
`Point(x, y)`. The same prefix symbols build values (D2, D3): `?x` / `!e` construct, `?v` / `!e`
match, so each symbol means one thing in both directions. Postfix `?` (propagate) and `?.` stay
expression operators; Chezzi's "not" is the `not` keyword, so prefix `?` / `!` are free. They mirror the types: `int?` → `?v`, `int!E` → `!e`. Nesting: `?(?v)` for `int??`,
`?(a, b)` for a tuple payload, `!IoErr(code)` to destructure an error struct.

```
match fetch(url):
    ?body: render(body)
    !e: retry_later(e)
match find(3):
    ?v: print(v)
    None: print("none")
match code:
    200: print("ok")
    other: print("unexpected {other}")
```

Exhaustiveness: `?v` + `None` covers `T?`; `?v` + `!e` covers `T!E`.

### D6. The names `Option`, `Result`, `Some`, `Ok`, `Err` are removed (owner decision 2026-10-07)

They exist only inside the compiler (R3). Every spelling has a replacement:

| removed | replacement |
|---|---|
| `Option[T]`, `Result[T, E]`, `Result[nil, E]` | `T?`, `T!E`, `None!E` |
| `Some(x)`, `Ok(x)` | plain `x` (implicit wrap), or `?x` |
| `Ok()` | falling off the end / bare `return` in a `None!E` fn |
| `Err(e)`, `return Err(e)` | `!e`, `return !e` |
| patterns `Some(v)`, `Ok(v)`, `Err(e)` | `?v`, `?v`, `!e` |

---

## Part 3 — Migration and order

1. **R1 + R2 + R3/R3b** (resolver pass, bracket node, prelude enums + variant import). This fixes CK1,
   S1, S2 and makes Part 2's hiding possible. Largest step; grid 1 gates it.
2. **R4** (fn value = item + type args, one memo, one callable classifier). Fixes CK3, A4. Grid 2.
3. **R5** (type variables, untyped constants, one constant evaluator). Fixes CK5, FF1. Grids 3, 4.
4. **Part 2 surface**, each its own ticket with a corpus migration (examples, tests/chz, std, docs, grammar.bnf,
   editor grammar):
   - D1 `None` replaces `nil`;
   - D2 `!e` error values, `return !e`, `None!E`;
   - D3 implicit wrap at every slot, `?x`, the no-expected-type rule;
   - D4 `else` guard;
   - D5 `?v` / `!e` patterns, bare-constant reject;
   - D6 remove the long names (last: after every corpus use is migrated).
5. Docs: rewrite the error-handling chapter of `docs/syntax.md` around the Part 2 table; `docs/spec.md`
   and `docs/grammar.bnf`; a migration note listing every old spelling and its new one.

Each step lands with its grid test and must keep `tests/chz` green at both worker counts and the benches
inside base's spread.

## Open questions

None at the moment. Resolved on 2026-10-07: "nothing, or an error" is `None!E`; the inner absent of
`int??` is `?None`; the long names are removed (D6).

## Decisions log

| date | decision |
|---|---|
| 2026-10-06 | generics follow Rust's architecture (Part 1); keep `[]` for type args |
| 2026-10-07 | R1 is one `resolve_path` decider inside the checker (not a pre-pass), the only writer |
| 2026-10-07 | hide `Option`/`Result` behind `T?`/`T!E`; same enum model underneath |
| 2026-10-07 | one "nothing" word: `None`; `nil` removed |
| 2026-10-07 | errors are values: `Error` protocol stays, `Err` removed; prefix `!e` builds an error, `return !e` returns it; no `fail` keyword |
| 2026-10-07 | `None` as a type is an annotation only (no value); "nothing or an error" is `None!E` (Zig `E!void`) |
| 2026-10-07 | prefix `?x` builds a present/success value; `?None` is the inner absent of `int??` |
| 2026-10-07 | no expected type anywhere: `?x` defaults to `T?`; `!e` needs `T` pinned by a later use, else an error |
| 2026-10-07 | the names `Option`, `Result`, `Some`, `Ok`, `Err` are removed from the surface (D6) |
| 2026-10-07 | design B: `else` must leave; values only from `??` / `match` |
| 2026-10-07 | match patterns `?v` / `!e` / `None`; a bare constant in a pattern is rejected |
| 2026-10-07 | a bare name in a match is the default arm and binds the whole value |
