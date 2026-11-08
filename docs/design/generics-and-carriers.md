# Design — generics on Rust's model, and `T?` / `T!E` without Rust's names

**Status: accepted, implemented.** R1-R2 in TICKET-222, R3/R3b in TICKET-229, R4 in TICKET-226, R5 in
TICKET-225 (narrowed by TICKET-238), D1 in TICKET-231, D2-D3 in TICKET-227, D4-D6 in TICKET-228. This file
keeps the rules and the decision log; the user-facing text is in the reference chapters.

**Why.** Sweeps #5 and #6 found 2 P0 generic/path bugs each. Each came from one fact
decided in several places. This design fixes the architecture once and settles the surface of
optional and error values.

**Lineage.** Generics and tagged-union enums: Rust. Errors as values: Rust/Go/Zig, never Python
exceptions. Scripting feel and `None`: Python. The hidden-carrier surface: Zig (`?T`, `E!T`,
`orelse`, `catch`).

---

## Part 1 — Generics and paths

**R1. One resolution, one writer.** `resolve_path` (rule table `classify_path`,
`src/checker/resolve.rs`) gives every path node (`f`, `lib.f`, `a.b.f`, `T.m`, `Bx[int].make`,
`E.A`) exactly one resolution (item + kind), memoised per node. A module fn is `Fn`, a type method
`MethodFn`, a payload variant `VariantFn`. The call side, the value side and the compiler only read
it, by a `match`. Only reject-only checks stay beside the match (a miss's message, the type-parameter
shadow, a type-only native name). There is no position rule: a module binds each name once, so no
`fn P` sits beside `struct P` (owner 2026-10-16, TICKET-263).

**R2. One bracket node, one type grammar.** `[]` stays for type arguments (as in Go). `head[X]` is one
node, `ExprKind::Index { obj, index, types }`, that carries both readings; `X` is parsed by the real
type grammar. R1 picks: type application when the head resolves to a generic item or type, index
when the head is a value.

**R3. The carriers are ordinary enums inside.** The optional and error carriers are prelude enums,
with no special case in the checker or compiler: one model for patterns, exhaustiveness and
lowering. `Ty::enum_ty` / `Ty::as_enum` are the one map between the enum key and the carrier type;
`variants_of` is the one variant reader. Their names are not part of the surface (D6).

**R3b. Variant import.** `import Red, Green from Color` makes variants bare names in the importing
module (Rust's `use Enum::Variant`). A bare variant that is not imported is rejected:
`'Green' is a variant of enum 'Color'; write it qualified`. `Checker::imported_variants` is the only
table that makes a bare name a variant. `None` is the one bare carrier name.

**R4. A fn value is (item, type args).** One `FnKey = (item, type args)` for every kind (plain fn,
method path, variant, `T.m`, native, extern, `json.decode[T]`), so equality follows from the key.
One compiler memo, `Compiler::synth_fn_proto`, keyed by `SynthFn`, builds every synthesized fn; a
synthesized fn reads no home global. One classifier, `Vm::callable` (an exhaustive `match`), is read
by call, `prepare_worker`, `Executor.submit` and the entrypoint.

**R5. Type variables, solved per statement.** A generic value read without a pin gets a type
variable. Joins (`if`/`match`/`??`/list/map/set/`==`) and call arguments on the SAME statement pin
it. It is rejected if still unpinned when its statement ends; no later statement pins (TICKET-238).
An integer or float literal is an **untyped constant** (Go's model) until it meets a slot; a width
slot (`int8`, ...) checks representability inside the one assignability relation. One constant
evaluator covers all pure ops.

**Rejected alternatives:**

- Monomorphization: Chezzi keeps type erasure + witness passing.
- Lifetimes and the `::<>` syntax: not adopted from Rust.
- A separate resolver pre-pass for R1: it needs a second scope model; Rust also resolves
  type-relative paths during type checking.

---

## Part 2 — Optional and error values: same model, hidden names

No code writes `Option`, `Result`, `Some`, `Ok` or `Err`. The table is the whole surface.

| need | spelling | ancestor |
|---|---|---|
| optional type | `int?` | Zig `?i32`, Swift `Int?` |
| error type | `int!E`, `int!` (= `int!Error`) | Zig `E!i32` |
| nothing, or an error | `None!E`, `None!` | Zig `E!void` |
| absent | `None` | Python `None` |
| present / success | the plain value (`x: int? = 5`); explicit `?x` (one level deeper) | Zig, Swift implicit wrap |
| an error value | `!e` | Zig `error.X` |
| return an error | `return !e` | Zig `return error.X` |
| pass it up | `f()?` | Rust `?`, Zig `try` |
| default | `f() ?? 0` | Zig `orelse` / `catch 0` |
| chain | `u?.name` | Swift, Kotlin |
| handle and leave | `v := f() else e:` + block | Swift `guard`, Rust `let-else` |
| both arms | `match` with `?v` / `!e` / `None` | Swift `case let v?` |

Error messages print the sugar (`int?`, `str!IoErr`).

### D1. One "nothing" word: `None`

`nil` is deleted. `None` as a **type** means "returns nothing" (`fn log(msg: str) -> None`); a
`-> None` fn produces no value. `None` as a **value** is "absent", legal only where a `T?` is
expected.

| | Python | Chezzi |
|---|---|---|
| `x := log("hi")` | `x = None`, silently | error: `log returns nothing; it cannot be used as a value` |
| `None` as a value | anywhere | only where a `T?` is expected |
| `None?` as a type | — | rejected |

### D2. Errors are values: keep `Error`, hide `Err`; `!e` builds one

- `Error` is the protocol every error type satisfies (`fn message(self) -> str`), like Go's `error`.
  `int!` means `int!Error`.
- Prefix `!` builds an error value from the whole operand: `!IoErr("disk")`, `!wrap(e)`. The operand
  must satisfy `Error` (`!5`: `int does not satisfy Error`).
- `return !e` returns an error. No `fail` keyword: the `!` tells success from failure in `str!str`.
- "Nothing, or an error" is `None!E` (Zig `E!void`), never `!E` (Zig's `!void` drops the error set).
- **Signatures are written, bodies are inferred.** A `T!E` type is never inferred from `return`s; a
  `!e` takes its type from an annotation or an expected type. A named fn's return type is written. A
  block body with no `->` returns `None`. An inline expression body (`fn dbl(x: int): x * 2`) takes
  its expression's type, as a lambda does (Kotlin's rule). A generator writes `-> Iterator[T]`.

### D3. Implicit wrap at every typed slot

A plain `T` value wraps into `T?` / `T!E` wherever the expected type is known: a binding, an
argument, a field, a collection element, a return, a yield. A carrier is never re-wrapped. No chain
with int→float (there is no widening). It declines where the expected type mentions an unpinned type
parameter.

`?x` builds a present/success value; the expected type picks the carrier. `x: int?? = None` is the
**outer** `None`; `x: int?? = ?None` is the inner one.

**No expected type on the value's own statement:**

| expression | rule |
|---|---|
| `y := ?5` | defaults to `int?` |
| `w := !"disk"` | error on its own line: annotate it, `w: int!str = !"disk"` |
| `z := None` | error on its own line: write `z: int? = None`; a literal `None` beside a value on the same line still joins (`[None, 7]` → `List[int?]`) |

An un-hinted `?x` binds only to a carrier. Every `?x` is judged before every `!e`, so `[!"e", ?1]`
and `[?1, !"e"]` are both `int!str`. An optional or a result never meets a bound through its payload
(`int?` does not satisfy `Num`).

### D4. `else` = handle and leave

`v := f() else e: <block>` binds `v` on success. On failure the block runs and **must leave**:
`return x`, `return !e`, `break`, `continue`, `panic(...)`, `os.exit(n)`, or an `if`/`match` whose
every branch leaves. Otherwise: `else block must leave (return, break, continue, panic)`. `else`
never produces a value; values come only from `??` and `match`.

```
data := load(p) else e:
    log(e.message())
    return None
for s in lines:
    n := to_int(s) else: continue          # T?: no e
```

| position | rule |
|---|---|
| module top level | no `return`; use `?` to end the run (rc=1), or `panic` / `os.exit` / `break` / `continue` |
| closure | `return` leaves the closure |
| `spawn:` block, `defer:` block | rejected |
| generator | `return` ends it; `return !e` only if it yields `T!E` |
| `else e:` on a `T?` | error: write `else:` |
| `else:` on a `T!E` | allowed |
| a `None!E` call | still must leave; "log and continue" is a `match` |
| `int?!E` | `else` handles the outer error; `v` is `int?` |

| job | tool |
|---|---|
| pass the error up | `?` |
| default, error ignored | `??` |
| log / wrap / skip, then continue unnested | `else` |
| default computed from the error; two long flows | `match` |

### D5. Patterns: compare the outer tag, bind only inside

| pattern | compares | binds |
|---|---|---|
| literal `1`, `"hi"` | the value | — |
| `_` | nothing (default arm) | — |
| bare lowercase name `other` | nothing (default arm) | the whole value |
| `Color.Red`, `None` | the tag | — |
| `Color.Rgb(r, g, b)`, `Point(x, y)` | the tag | inside |
| `?v` | present / success | `v` |
| `!e` | error | `e` |
| bare variant `Green` | rejected unless imported: "write it qualified as `Color.Green`" | — |
| bare constant `LIMIT` | rejected: "`LIMIT` is a constant; to compare write `x if x == LIMIT`, to bind use a new name" | — |

`?x` / `!e` build values and `?v` / `!e` match them, so each symbol means one thing both ways. Nesting:
`?(?v)` for `int??`, `?(a, b)` for a tuple payload, `!IoErr(code)` to destructure. Exhaustiveness:
`?v` + `None` covers `T?`; `?v` + `!e` covers `T!E`.

### D6. `Option`, `Result`, `Some`, `Ok`, `Err` are removed from the surface

| removed | replacement |
|---|---|
| `Option[T]`, `Result[T, E]`, `Result[nil, E]` | `T?`, `T!E`, `None!E` |
| `Some(x)`, `Ok(x)` | plain `x`, or `?x` |
| `Ok()` | falling off the end / bare `return` in a `None!E` fn |
| `Err(e)`, `return Err(e)` | `!e`, `return !e` |
| patterns `Some(v)`, `Ok(v)`, `Err(e)` | `?v`, `?v`, `!e` |

**Rejected alternatives:**

- `if v := x:`: brings back the nesting `else` removes.
- A force-unwrap operator: dropped.
- A `fail` keyword: `return !e` says it.
- `!E` for "nothing or an error": Zig's `!void` means the opposite (error set dropped).
- Inferring a return type from `return`s: Go and Rust require the written type.
- Pinning an open `None` from a later statement (TICKET-234): replaced by the per-statement rule
  (TICKET-238).

## Decisions log

| date | decision |
|---|---|
| 2026-10-06 | generics follow Rust's architecture (Part 1); keep `[]` for type args |
| 2026-10-07 | R1 is one `resolve_path` decider inside the checker (not a pre-pass), the only writer |
| 2026-10-07 | hide `Option`/`Result` behind `T?`/`T!E`; same enum model underneath |
| 2026-10-07 | one "nothing" word: `None`; `nil` removed |
| 2026-10-07 | errors are values: `Error` stays, `Err` removed; prefix `!e` builds an error, `return !e` returns it; no `fail` keyword |
| 2026-10-07 | `None` as a type is an annotation only; "nothing or an error" is `None!E` |
| 2026-10-07 | prefix `?x` builds a present/success value; `?None` is the inner absent of `int??` |
| 2026-10-07 | the long names are removed from the surface (D6) |
| 2026-10-07 | `else` must leave; values only from `??` / `match` |
| 2026-10-07 | patterns `?v` / `!e` / `None`; a bare constant in a pattern is rejected; a bare name is the default arm |
| 2026-10-09 | signatures are written, bodies inferred (TICKET-228) |
| 2026-10-10 | TICKET-238: inference never reads a later statement; `z := None`, `xs := []`, `g := ident`, `w := !e` are errors on their own line |
| 2026-10-11 | TICKET-248: an un-hinted `?x` binds only to a carrier; `?x` judged before `!e`; a carrier never meets a bound through its payload |
