# M11 — Panic recovery + Go-style `Result[T, E]`

## Context

`gaps.md` Tier 3 names **Panic recovery** as the single biggest gap to "a language you'd
reach for to write real scripts." Today a runtime fault (index-OOB, div-by-zero, overflow,
missing map key, max-call-depth) bubbles out as a `RuntimeError` and **kills the process**;
only *explicit* errors modeled with `Result`/`?` are handlable. There is no boundary that
contains an unexpected fault.

We add that boundary, and — per the user — fix the error model to be **Go-style** first.
Research across Go/Rust/Zig/Swift/Erlang/Koka/Gleam/Roc/Lua/Python converged on:
1. two channels (explicit `Result`/`?` vs. panics) kept separate;
2. the recovery boundary catches *everything beneath it transitively* — no per-call wrapping,
   no pre-tagging risky code;
3. the boundary converts the panic into the **existing** error type, so it composes with
   `?`/`match` rather than introducing new control-flow vocabulary.

Go's `error` is an **interface** `{ Error() string }`, not a string — so "do it like Go" =
keep an `Error` protocol, with `str` conforming to it (the message *is* the string).

### Locked decisions
- **Error model (Go-style):** `protocol Error: fn message(self) -> str`. `str` conforms
  (`"x".message()` → `"x"`). `T!` = `Result[T, Error]`; `T!E` = `Result[T, E]`; `T?` stays
  `Option[T]`. No error-builder needed — `Err("msg")` works because `str` is an `Error`.
- **Boundary:** `recover:` block expression → `Result[T, Error]` (= `T!`). Catches **all**
  runtime faults transitively beneath it, converting the panic message to `Err(<str>)`;
  returns `Ok(<block value>)` otherwise. Not try/except, no closures, no unbound-var trap.
- **`?` vs recover:** `recover` catches *panics only*, never a `?` propagation. (`?` inside a
  `recover:` block early-returns the *enclosing function*, as today — `recover` is panic-only.)
- **Phasing:** Phase A (error model) lands fully + commits, then Phase B (boundary). TDD throughout.
- **Both engines** (tree-walk interp + bytecode VM) — parity is a hard invariant.

---

## Phase A — Go-style `Result[T, E]` + `Error` protocol  (checker-heavy; runtime type-erased)

Runtime stays type-erased: a `Result` is `Value::Enum{ty:"Result", variant, payload}` in both
engines and does not track `E`. So Phase A is almost entirely the **checker + type syntax**;
the only runtime change is adding a `message` method to `str`.

**A1. `Ty` changes** — `src/checker/ty.rs`
- `Result(Box<Ty>)` → `Result(Box<Ty>, Box<Ty>)` (T, E). Update `Display` (`T!E`, or `T!` when
  E == `Protocol("Error")`).
- Add `Protocol(String)` — an existential protocol-as-value-type (e.g. the type of an `Error`).
- Add/adjust helper `Ty::result(t)` → `Result(t, Protocol("Error"))`; add `Ty::result_e(t,e)`.
- Fix every `Ty::Result(_)` match site (finite): `match_kind` (`mod.rs:1205`), `infer_try`
  (`1884`), `infer_decode`/`Ty::result` (`1919`), `compatible`, `subst`, `unify`, `Display`,
  `top_level_error`. Grep `Ty::Result` and `Ty::result(` to enumerate.

**A2. Prebuilt `Error` protocol** — `prebuilt_protocols()` `src/checker/mod.rs:2811`
- Insert `"Error" => ProtocolInfo { methods: [("message", FnSig::plain([Unknown], Str))] }`,
  mirroring the `Stringable` entry.

**A3. `str` conforms to `Error`; `.message()` at runtime**
- Checker `satisfies` (`mod.rs:2681`): `Ty::Str` (and the existing built-in scalars where it
  makes sense — scope to `str` only for now) satisfies `"Error"`.
- Existential `Protocol("Error")` also implies `Stringable` for `print`/interpolation — when a
  value is statically `Protocol(P)`, allow `print(e)` / `"{e}"` (dispatch to `message()` at
  runtime) and the `P` methods only.
- Runtime: add `"message"` arm to `str_method` (`src/interp/builtins.rs:47`) returning the
  string itself; VM mirrors in its str-method dispatch. (Struct errors already dispatch
  `message()` structurally on both engines — no change.)

**A4. Type syntax `T!E` + protocol-as-type** — `src/parser/mod.rs` + `resolve_type`
- `parse_type_postfix` (`parser/mod.rs:853`): after eating `Bang`, optionally parse a base type
  for `E` (only when the next token starts a type — ident / `(` ); `T!` → `Generic("Result",[ty])`,
  `T!E` → `Generic("Result",[ty,E])`. Keep left-to-right stacking working.
- `resolve_type` (`mod.rs:704`): `Type::Generic("Result",[t])` → `Result(t, Protocol("Error"))`;
  `("Result",[t,e])` → `Result(t,e)`. In `Type::Named`, when the name is a known protocol
  (`self.protocols.contains_key(n)`) → `Ty::Protocol(n)`.

**A5. Existential semantics** — `compatible`, field/method access
- `compatible(C, Protocol(P))` ⇔ `C` satisfies `P` (reuse `satisfies`); `compatible(Protocol(P),
  Protocol(P))` ⇔ same P; `Unknown` compatible with all (as today).
- Method/field access on a `Protocol(P)` receiver yields P's method signatures only.
- `subst`/`unify`: pass `Protocol` through unchanged.

**A6. `Ok`/`Err` constructor typing** — `infer_call` `src/checker/mod.rs:2122`
- `Ok(x)` → `Result(typeof x, Unknown-E)`; `Err(x)` → `Result(Unknown-T, typeof x)`. The
  unknown side unifies against the expected/declared `Result[T,E]` at the use site (return type,
  `let` annotation, match). `Err("..")`: `typeof x == Str`, compatible with `Protocol("Error")`.

**A7. `match` payload + `?` typing**
- `match_kind` for `Result` (`mod.rs:1205`, `1209`, `1260`): `Ok` payload binds `T`, `Err` binds
  `E` (was `Unknown`).
- `infer_try` (`mod.rs:1884`): yields `T`; the inner `Result`'s `E` must be compatible with the
  enclosing function's return `E` (propagation requires matching error types — like Rust).

**A8. Result-producing internals** — `json_decode`, `top_level_error`
- `infer_decode` → `Result[T, Error]`. `top_level_error` unchanged in behavior (reports the
  `Err` payload via its message). VM/interp `alloc_enum("Result","Err",..)` paths unchanged
  (type-erased).

**A9. Migrate error *consumption* sites** (the only breakage)
- Sites treating an `Err` payload as raw text break under `e: Error`. Migrate
  `"prefix: " + e` → `"prefix: {e.message()}"` (or `+ e.message()`), `e.trim()` →
  `e.message().trim()`. Known: `examples/sys.chz`, `examples/json_dynamic.chz`,
  `examples/regex_demo.chz`, and `std/*.chz` (`grep -rn 'Err(' examples std`). **Producers**
  (`Err("..")`) stay as-is — `str` conforms.

**A10. Docs/grammar** — `docs/syntax.md`, `docs/grammar.bnf`
- Document `Error` protocol, `str` conformance, `T!E` shorthand, `Result[T,E]`. Update the
  `!` type-postfix rule in the BNF (drives `cargo test conformance`).

### Phase A tests (TDD — write first, in `#[cfg(test)]` of `checker`, then make green)
- `Result[int, str]` and `int!str` parse + resolve equal; `int!` resolves to `Result[int,Error]`.
- `str` satisfies `Error`; `"x".message() == "x"` (interp **and** VM via `run`/`assert_parity`).
- custom `struct DbErr: code:int; fn message(self)->str` usable as `T!DbErr`; `Err(DbErr(503))`.
- `match` binds `Err(e)` with `e: Error`; `e.message()` checks; `print(e)`/`"{e}"` works.
- `?` ok when E matches; type error when E mismatches.
- regression: existing `Err("..")` programs still check + run identically (parity).

---

## Phase B — `recover:` boundary  (engine-heavy)

**B1. Lexer** — `src/lexer/mod.rs:130` — add `recover` keyword.

**B2. AST** — `src/ast/mod.rs` (~`281`) — `ExprKind::Recover(Box<Block>)` (expression; the
block is a statement list whose trailing expression is its value, like a fn body).

**B3. Parser** — `recover:` + INDENT block → `Recover`. Reuse the existing block parser used for
fn bodies / `if`. Assignable: `r := recover: ...`.

**B4. Checker** — `infer` arm for `Recover(block)`: type the block like a function body (new
value scope), let `T` = trailing-expression type (or `nil`); result type = `Result[T, Error]`.
`?` inside still validated against the enclosing function (recover is panic-only).

**B5. Interp** — `src/interp/mod.rs`
- Eval `Recover`: `let depth = self.call_depth; let saved = self.propagating.take();` run the
  block via the fn-body value path; then:
  - `Ok(v)` → `Value::Enum Result/Ok [v]`.
  - `Err(e)` **and** `self.propagating.is_some()` → a `?` unwind: restore `propagating`, re-raise
    `Err(e)` (do **not** catch).
  - `Err(e)` otherwise → genuine panic: `self.call_depth = depth` (unwinding skipped the
    `-=1`s at `:695/:1492`), restore `saved` propagating, produce `Value::Enum Result/Err
    [Value::Str(e.message)]`.
- `Value::Str` payload is a valid `Error` (str conforms) — composes with `match`/`?`.

**B6. VM** — `src/vm/op.rs` + `src/vm/mod.rs`
- New ops in `op.rs:40`: `Op::PushHandler(target_ip)` / `Op::PopHandler`.
- VM state: `handlers: Vec<Handler{ frame_len, stack_len, call_depth, ip }>`.
- Codegen for `Recover(block)`: `PushHandler(H)`; emit block; on fall-through wrap top in
  `Result/Ok`, `PopHandler`, jump past `H`; at `H`: stack already holds the error str (pushed by
  the catch) → wrap in `Result/Err`.
- Catch point in `run_until` dispatch: today `self.step(op,span)?` bubbles. Wrap it — on
  `Err(rte)` with a live handler at/above this run level: truncate `frames`/`stack` to the
  handler snapshot, restore `call_depth`, push `Value::Str(rte.message)`, set `ip = handler.ip`,
  continue. Else propagate. (VM `?` never returns `Err` — it `do_return`s — so any `Err` here is
  a genuine panic; no propagation gate needed, unlike interp.)

**B7. `panic(msg)` builtin** (optional, cheap) — explicit raise that produces a catchable
`RuntimeError`. Defer if time-boxed; OOB/div0/overflow already raise catchable `RuntimeError`s.

### Phase B tests (TDD)
- `recover: xs[99]` → `Err`, message contains "out of bounds"; `recover: 1/0` → `Err` "division
  by zero"; overflow, missing key, runtime type error → all caught.
- `recover: 2+2` → `Ok(4)`.
- deep panic (fault 3 calls down) caught at the boundary.
- `recover` does **not** swallow a `?`: a `?`-Err inside still early-returns the enclosing fn.
- nested `recover`; recovered value drives `match` and `?`.
- **parity**: every recover program identical on interp vs VM (`assert_parity`).
- golden: `examples/recover.chz` + `.expected`, run on both engines.

---

## Verification (end-to-end)
- `cargo test` (lexer/parser/checker/interp/vm/parity) + `cargo test conformance` — all green.
- `cargo build` + `cargo clippy --all-targets` — clean.
- `cargo run -- run examples/recover.chz` — observe Ok + recovered-Err output.
- Confirm a previously process-killing script (`print(xs[99])` wrapped in `recover:`) now
  prints a recovered error and exits 0.
- Update `PROGRESS.md` (new **M11**, staged A→B) and `gaps.md` (mark Tier 3 *Panic recovery*
  done; note Result[T,E]/Error landed). Commit each phase with single-line conventional messages.

## Critical files
- Checker: `src/checker/ty.rs`, `src/checker/mod.rs` (`resolve_type:704`, `prebuilt_protocols:2811`,
  `satisfies:2681`, `match_kind:1205`, `infer_try:1884`, `infer_call Ok/Err:2122`).
- Parser: `src/parser/mod.rs` (`parse_type_postfix:853`, block parser).
- Lexer/AST: `src/lexer/mod.rs:130`, `src/ast/mod.rs:281`.
- Interp: `src/interp/mod.rs` (eval, fn-body value path, `call_depth` at `:695/:1492`),
  `src/interp/builtins.rs:47` (`str` `message`).
- VM: `src/vm/op.rs:40`, `src/vm/mod.rs` (`run_until:205`, `step:311`, codegen, str-method dispatch).
- Docs: `docs/syntax.md`, `docs/grammar.bnf`, `PROGRESS.md`, `gaps.md`.
- Migration: `examples/*.chz`, `std/*.chz` (error-consumption sites).
