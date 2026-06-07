# Fix: `?` inside a closure isn't checked against the closure's return type

## Context

The M11 recover review surfaced a pre-existing **type-soundness bug** (now recorded in `gaps.md`
tech-debt). `?` inside a closure is validated against the *enclosing function's* return type, not the
closure's own, because `infer_closure` never sets `self.current_ret` for the closure body. So a `?`
in a closure passes the check against `main`'s `nil` return (which `?` allows as "top-level"), then
early-returns an `Err` as the closure's value — landing an `Err` in a slot typed otherwise:

```
fn parse(s: str) -> int!:
    if s == "2": return Ok(2)
    return Err("bad: '{s}'")
doubled := ["2", "x"].map(fn(s: str) -> int: parse(s)? * 2)
print(doubled)        # [4, Err(bad: 'x')]  — an Err in a list[int]
```

Outcome: the checker should **reject** `?` in a closure whose declared return isn't `Result`/`Option`
(and validate the `Result` case properly), exactly as it does for named functions. Checker-only — no
runtime/VM change, so no parity impact. Verified no existing `.chz`/test uses `?` in a closure, so no
regression.

## Change (one site)

`src/checker/mod.rs` — `fn infer_closure` (~line 2043). Mirror `check_fn_body`'s `current_ret`
handling around the closure body:

- Before `let body_ty = self.infer(body);`, compute the closure's declared return and install it:
  ```rust
  let declared_ret = ret.map(|t| self.resolve_type(t, body.span)).unwrap_or(Ty::Unknown);
  let saved_ret = std::mem::replace(&mut self.current_ret, declared_ret);
  ```
- After the body is inferred (and `pop_scope`), restore: `self.current_ret = saved_ret;`
  (place it alongside the existing `self.recover_depth = saved_recover;` restore).

Effect via the existing `infer_try` (`self.current_ret` reader):
- declared `-> int` (or any non-Result/Option) + `?` → rejected (`'?' used in a function that returns
  int, not Result or Option`). Fixes the example.
- declared `-> int!` (Result) + `?` → allowed, yields the `Ok` type, E checked against the function's
  error type — same as named fns.
- inferred-return closure (no annotation) + `?` → `current_ret` = `Unknown` → rejected with the
  generic `'?' ... returns ?` message. Rare; acceptable (annotate the closure's return). Closures
  *without* `?` are unaffected (`current_ret` is set but never read).

The existing `recover_depth` reset in `infer_closure` already scopes `?`-in-recover correctly; this
just adds the function-return context. No `infer_try` change needed.

## Tests (TDD — add to `src/checker/tests.rs`)

- `closure_question_mark_on_nonresult_return_rejected` — the example shape:
  `["2"].map(fn(s: str) -> int: parse(s)? * 2)` → `rejects(.., "not Result or Option")`.
- `closure_question_mark_on_result_return_ok` — `fn(s: str) -> int!: parse(s)?` inside a Result-typed
  consumer → `ok(..)`.
- (optional) `closure_question_mark_inferred_return_rejected` — no annotation + `?` → rejected.

Write tests first, watch them fail, then apply the `infer_closure` change.

## Verification

- `cargo test` (checker + full suite) — all green; `cargo test conformance` green; `cargo clippy
  --all-targets` clean; `cargo build`.
- `cargo run -- run /tmp/closure_q.chz` (the repro) now reports a type error instead of printing
  `[4, Err(...)]`; confirm both engines reject (it fails at type-check before either runs).
- Update `gaps.md`: flip the "`?` inside a closure isn't checked…" tech-debt entry to ✅ FIXED with a
  one-line note. Commit single-line conventional (`fix(checker): …`).
