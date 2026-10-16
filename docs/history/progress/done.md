# Progress: done list, newest to oldest (moved verbatim from `PROGRESS.md` lines 14449-15820)

## Done (newest → oldest)

One bullet per milestone/epic. Full landing detail (TDD notes, review-panel findings, test-count deltas,
branch names) is in the git log.

- **TICKET-218 (2026-10-06) — `native type`; a C width is range-checked at the C boundary.** `std/ffi.chz` is the one list of `std.ffi`'s exports: `native type ptr` and `native type int8 = int` … `float32 = float` (new std-only declaration form), read by `native::ffi::declared_types()`; `TYPE_NAMES` and `native_module_sig` are deleted, and module constants are harvested `NAME: const T = value` lets (`math.pi`; `native_consts` deleted). A width is a slot tag `Ty::Width(CType)` erased from every value type. A constant outside a width is a compile error at every value-into-slot site (`constant 300 does not fit int8 (-128..127)`, one helper `check_const_fits`); a runtime value faults where it crosses into C (extern param, struct field, `ffi.store_<w>`, callback return: `value 300 does not fit int8 (-128..127)`). Ranges come from `CType` (Rust's primitive bounds); uint64 passes its bit pattern at run time; float32 passes NaN/inf. Explicit wrap: `ffi.cast_<w>(v)`; range constants `ffi.INT8_MIN` … `ffi.FLT_MAX`. Grids: `checker::tests::ffi_width_constant_grid`, `tests/ffi_width_range_grid.rs`.
- **TICKET-181 (2026-09-29) — Blocking contexts: one table (wave 16 Family 2).** Every blocking op asks `block::mode(ctx, spec)` (`src/vm/block.rs`) how to block; the context is derived by `Vm::block_ctx()`. One demote bracket (`Vm::block_enter`) accounts a wait by `WaitSpec::will_return` (`inflight`) or registers a `Waiter` in the one registry `SchedCore::waiters`, which replaced `demoted_chans`, `demote_cancel_watch`, `demoted_groups`, `body_waits`, `guard_waits` and `blocked_native`. The four predicates (`is_counted_party`, `owns_os_thread`, `can_block_in_place`, `may_block_socket_in_place`) and `demote_enter`/`demote_exit`/`demote_socket_*` are gone. Fixed C1/E1-E4 (timed `wait:` in a callback), X1 (`Executor.shutdown()` in a fiber at T=1), X2 (socket read in a `main` callback/`defer:`), F1 (`request.get` in a fiber callback at T=1), a `wait:` send arm in a `main`/job callback, and a `timer.recv()` in a fiber callback. Grids: `block::tests::every_cell_matches_the_table`, `tests/blocking_context_grid.rs::grid_every_op_in_every_context` (210 cells × T=1/2/0 + seeds). Table and deliberate differences: `docs/reference/concurrency.md` "Blocking-context table".
- **TICKET-179 (2026-09-29) — D4 airlock: the mark means "the parent can observe it" (wave 16 Family 4).** One receiver-write table, `vm::is_mutating_native_kind`, read by the checker's `mutates_receiver`; `Map.merge` is not a write; `bytearray.copy()` exists. A fresh `spawn` operand (literal, comprehension, container `.copy()`) is decided once by the checker and crosses with its root unmarked. A crossing generator marks only the frame slots the sending task can still reach (`Vm::gen_frame_observable`). Layer A reports a write only when every path reaches it. Grids: `checker::tests::receiver_write_table_matches_observed_behavior`, `checker::tests::airlock_crossing_grid_compile_cells`, `tests/chz/spec/airlock_crossing_grid_test.chz`.
- **TICKET-178 (2026-09-28) — type declarations and `test fn` are top-level only.** One list, `Parser::item_keyword`, decides it: `parse_module` dispatches items through it and every block rejects them (`struct must be a top-level declaration`); the `depth > 1` denylist for `extern`/`native`/`import` is gone, and `docs/reference/grammar.bnf` has an `<item>` rule. Grid: `parser::tests::item_declarations_are_top_level_only_grid`.

- **TICKET-160 (2026-09-21) — `Map.items()` and a shallow `copy()` on `List`/`Map`/`Set`.** `m.items()` returns `List[(K, V)]` in `keys()` order, so `Map(m.items()) == m`; `copy()` is shallow like CPython's and snapshots struct keys like `merge`. Declared in `std/prelude.chz`, dispatched in `Vm::core_method` (`src/vm/call.rs`). `Map` still iterates by key.
- **TICKET-139 W14-2 (2026-09-19) — a keyword call through a fn value needs a binding of ONE known fn; anything else is a compile error.** Labels are surface-only, so `fs := [f, ren]; fs[1](a=1, b=2)` bound by `f`'s names and ran `ren` (`201`; CPython `102`). Decision (owner-approved 2026-09-18): the checker (`kw_certain`/`kw_written`/`kw_pending`, settled at `pop_scope`) accepts a keyword call only through an unannotated `:=` of a top-level fn or closure literal, or a nested `fn`'s name, never written afterwards; a param, list slot, returned value or reassigned binding is rejected with a message naming the positional workaround. CPython-style runtime rebinding was rejected as unsound (`fn(a: int, b: str)` vs `fn(b: int, a: str)` share one type). `kw_value_call_hof_param_labels` became `kw_value_call_through_a_param_is_rejected`; `examples/keyword_value.chz` lost its HOF-param and curried keyword calls. Pinned by `kw_value_call_through_*` and `tests/chz/spec/fn_value_named_arg_labels_test.chz`.
- **TICKET-139 W14-22 (2026-09-19) — a string-literal match pattern is decoded like the literal, and a hole is a parse error.** `parse_pattern_primary` (`src/parser/mod.rs`) now runs a `Token::Str` pattern through `interpolation::parse_interpolation`: `{{`/`}}` decode to braces (before, pattern `"{{x}}"` did not match the value `"{{x}}"`), an all-digit hole stays text (DEC-018), a real `{x}` hole is `a string pattern cannot interpolate`, and `"{"` reports `unterminated '{'`. `r"..."` is unchanged. Pinned by `holed_string_pattern_is_a_compile_error` and `tests/chz/spec/string_pattern_braces_test.chz`.
- **TICKET-139 W14-36 (2026-09-19) — a tuple scrutinee accepts a bare catch-all binding.** `MatchKind::Tuple` (`src/checker/pattern.rs`) now takes a payload-free unqualified non-variant name (DEC-107), as the struct arm already did; `rest:` binds the whole tuple, a variant name is still rejected, a guarded one closes nothing. Pinned by `tests/chz/spec/tuple_scrutinee_catch_all_test.chz` and `a_tuple_scrutinee_catch_all_binding_is_irrefutable`.
- **TICKET-139 W14-1 (2026-09-19) — a closure can capture a bare match catch-all binding.** `pattern_binds` (`src/compiler/mod.rs`) now collects a payload-free unqualified name, the capture-analysis twin of DEC-107's catch-all predicate; before, `match e: 1: ..; whole: (fn() -> int: whole)()` was check-OK then panicked `CellLoad on a non-handle value` (rc=101). Pinned by `tests/chz/spec/match_bare_binding_capture_test.chz`.
- **TICKET-119 (2026-09-15) — a deep module global reaches its depth-exceeded fault in linear, not
  quadratic, time (W13-9).** `to_snap_depth`'s speculative fast path (`try_wire_speculative`) re-walked
  the whole remaining subtree from every node before the slow arm descended one node and repeated, an
  O(cap²) cost bounded by `MAX_STRUCTURAL_DEPTH` regardless of the chain's real length — a 5000-deep and
  an 8000-deep module global both measured ~20.4 s. Fixed by skipping a speculative attempt already
  proven, by an exact-replay argument over the memo's mint/taint/invalidation state, to overflow the
  same way again (`WireMemo`'s new `doom`/`doom_ids`/`doom_invalid_upto`). Measured on release binaries:
  5000-link chain 20.40 s → 0.05 s, 8000-link 20.32 s → 0.05 s, 5000-link with a payload per level
  33.30 s → 0.06 s, 6000-deep nested closures 8.81 s → 0.04 s; the under-cap 4500-link control and two
  crossing-heavy programs (50-task fan-out over a 200k-int list, a spawn over 20 000 aliases) show no
  slowdown.
- **TICKET-120 (2026-09-12): a static method's default now fills through a module-qualified
  `mod.Type` head, closing W13-11.** `Walker::receiver_struct_ty` (`src/desugar/mod.rs`) gains a
  fifth arm for `Field { obj: Ident(alias), name }`, resolved through a new `Ctx::find_type_qualified`
  and gated on `alias` being a real import alias (never a local or a type parameter). A new
  `ModReg::types` set (struct + enum + newtype names) backs the lookup. Four shapes now resolve their
  default through the qualified head — the plain import, the `import … as` alias, an enum, and a
  generic struct — while five stay rejected unchanged: a defaultless argument, an instance method
  called statically, an unknown module member, over-arity, and a cross-module struct-name collision.
- **The airlock's cross-heap copy is now identity-preserving for data and closures, not just cells
  (2026-09-10, TICKET-100).** A DAG alias (`pair := [box, box]`) used to re-serialize as two
  independent copies on every crossing; it now back-references like a cycle does, so one source
  object stays one object per crossing (`b := a` means the same thing inside a task as outside one,
  matching CPython's `copy.deepcopy`). `WireMemo` gains a never-popped `nodes`/`gens_seen` pair
  alongside `path`/`cells` (`src/vm/sched.rs`); a generator reached twice in one crossing is now a
  clean fault (`a generator cannot be sent across tasks twice in one crossing`) instead of a silent
  duplicate, since a generator carries no wire id to back-reference. **Exception, recorded as W11-15
  (`docs/contributing/gaps.md`):** the three `RwShared` stores keep the old per-crossing splitting
  (`to_wire_crossable_split`), because their read views drain one stored wire through many
  independent rebuild maps and a cross-element back-ref would make a rebuild quadratic
  (`rwshared_view_over_shared_bindings_is_not_quadratic`). No regression measured on the spawn path
  (`docs/contributing/benchmarks.md`).
- **TICKET-050's seven deferred wall-clock tests are converted to counted measures, the Rust ratio
  ban now scans all of `src/`+`tests/`, both W8-8 serialization gates bound child CPU against child
  wall on one run instead of dividing two wall-clock samples, the wall-clock allowlist is down to 19,
  a sleep used to order two events is now ratcheted in both Rust and chz, and the W7-60 nested-join
  cancel test now skips instead of failing when the host starves its 2s window (2026-09-05,
  TICKET-059).**
- **A leading UTF-8 byte-order mark (`U+FEFF`) is now stripped instead of a lex error (2026-09-05,
  TICKET-058).** `Lexer::new`/`Lexer::new_file` strip at most one leading `U+FEFF`, matching CPython
  and Go; a second BOM or a mid-file BOM stays a lex error, and an interpolation fragment re-lexed via
  `Lexer::new_in` never strips. Every raw-source consumer that indexes by a lexer `(line, col)` —
  `--errors=json`'s `end_col`, the plain-text caret snippet, and the editor's `hover`/semantic-token
  columns — strips once at its own use site so the two never drift apart.
- **`json.parse`'s multi-character token errors now name where the token STARTED, not where the scan
  stopped (2026-09-04, TICKET-057).** Four sites in `std/json.chz`: `parse_lit` (a bad
  `true`/`false`/`null` now names the word's first letter), `parse_string` (an unterminated string
  now names its opening quote), and `parse_number`'s fraction and exponent branches (a malformed `.`/
  `e`/`E` now names that character; the exponent site captures a named position rather than
  `self.pos - 1`, since it may also consume a `+`/`-` sign). Measured against CPython 3.14.7 over a
  1086-document mutation sweep, position mismatches fall from 242 to 26 of the 918 documents both
  implementations reject; the 26 residual are CPython 3.14's own trailing-comma diagnostics, left
  diverging on purpose alongside an invalid string escape and `invalid number: leading zero`.
- **Three positions that hold a statically known expected type now push it into the position instead
  of inferring bottom-up and comparing after (2026-09-04, TICKET-054).** All three falsely rejected a
  program every ancestor accepts. (1) An `if`/`match`-EXPRESSION bound to an annotated protocol-typed
  slot (`x: Sh = if true: Sq(2) else: Tr(9)`) is now accepted: `Checker::unify_branch`
  (`src/checker/pattern.rs`) takes the branch position's `expected_hint` and, on a `compatible`
  mismatch, accepts the fold if the hint is `assignable` from both branches — read only through plain
  `assignable`, never `assignable_w` (DEC-034), so it licenses no int→float widen and an un-annotated
  mismatch still reports `branches have incompatible types: … and …`. (2) A closure's own `-> float`
  return now widens an untyped int body (`g := fn() -> float: 3`) exactly like a named fn's: the
  checker half swaps `assignable` for `assignable_w` with the `untyped_int_const` license
  (`Checker::infer_closure`), and the paired compiler half sets `FnComp::ret_is_float` in
  `compile_closure` from the closure's own declared `ret` and emits `Op::CoerceFloat` before the
  return coerce — a checker-only fix would have left a runtime `Int` under a static `float`. (3) A
  `float` parameter reached through a protocol-typed receiver or a bound generic type parameter now
  widens the same as the identical method on a concrete receiver: the `Ty::Protocol` and `Ty::Param`
  method-call arms (`src/checker/expr.rs`) now route through `check_args_subst` with the
  PRE-substitution declared params, the same call the concrete-struct arm already makes. Two programs
  outside the three repros change verdict as a result: `x: Any = if c: 1 else: "a"` is now accepted
  (`Any` is the empty top protocol, matching the existing `List[Any]` concat precedent), and a
  protocol method requiring `List[float]` now accepts a `List[int]` argument, carrying DEC-033's
  element license.
  **Review-fail follow-up (2026-09-04): the sink-3 scalar widen was keyed on the PROTOCOL's declared
  slot, not the actual runtime witness's.** A GENERIC-struct witness (`struct S[T]` with
  `fn m(self, x: T) -> T`, satisfying `fn m(self, x: float)` via `T=float`) declares its own param
  `T`, not `float`, so its own prologue never emits `Op::CoerceFloat` — `p: P = S(1.0)` then `p.m(1)`
  left a runtime `Int` under a static `float` (measured: `p.m(1) / 2` printed `0`, not `0.5`), while
  the concrete receiver correctly rejected the same call. Fixed by moving the coercion to the CALL
  SITE for any argument `check_args_subst` licenses: a new checker→compiler side table
  (`ArgFloatWidenTable`, keyed exactly like `RetCoerceTable` on the argument's own span) records the
  widen verdict per argument, and `Compiler::compile_args` emits `Op::CoerceFloat` right after the
  argument when the table says so — sound regardless of which witness the dynamic dispatch calls, and
  a harmless duplicate on a concrete receiver whose own prologue also coerces. `cargo test --lib` is
  `4471 passed; 0 failed; 2 ignored` (was 4462, +1 net for the review-fail regression test — 10 added,
  9 already counted); `chezzi test tests/chz` is `767 passed, 0 failed, 0 errored` (was 761, +6 in
  `tests/chz/spec/expected_type_sinks_test.chz`, which asserts RUNTIME values via `/ 2`).
- **A witness's unmet `eq` `where` bound is now refused at the erasure into a protocol slot, not
  laundered through it (2026-09-04, TICKET-053).** `a: Tagged = Box(Tag(1))` (`Box`'s `eq` requires
  `Tag: Comparable`, which `Tag` doesn't provide) was `ok: no type errors` then printed the
  unconditional `("Eq", "eq", "protocol")` grant's own answer — a hole reaching an `Option[Tagged]`
  payload, a `[T: Eq]` bound and a struct field too. `Checker::assignable`'s protocol arm
  (`src/checker/proto.rs`) now also checks a new where-bound-only mode of the existing `eq` walk
  (`eq_where_bounds_unsatisfied`, kept apart from the strict walk's memo since a where-only `None` is a
  weaker proof) and refuses the erasure, naming the bound (``Box[Tag]'s `eq` requires Tag:
  Comparable``) through `protocol_note`. Three `tests/chz/spec/eq_protocol_existential_test.chz` tests
  that pinned this as a RUNTIME fault no longer compile and moved to
  `checker::tests::protocol_erasure_eq_where_bound_boundaries` as compile-time rejections; `docs/contributing/gaps.md`
  W7-52 and `docs/reference/syntax.md` are corrected — the erasure gate is Rust's compile-time model, not Go's
  runtime one.
- **Five Rust `#[test]`s that asserted a wall-clock bound or a `time.sleep_ms` handshake now assert a
  counted probe or a channel handshake instead (2026-09-03, TICKET-050).** CPU contention reddened
  these under load though the code they guard was unchanged. `find_all_offset_conversion_is_linear`
  (`src/native/regex.rs`) now counts bytes scanned via a new `cp_count` probe (`CP_SCAN_BYTES`,
  `#[cfg(test)]`) instead of timing `find_all` — every byte→codepoint conversion must route through
  `cp_count` or the probe goes silent. Four `src/vm/tests.rs` executor/cancellation tests
  (`nested_executor_job_is_cancelled_by_an_outer_shutdown_now_mn`,
  `a_job_submitted_to_mains_executor_survives_another_executors_shutdown_now_mn`,
  `a_timer_armed_eager_wait_is_cancellable_by_shutdown_now`,
  `shutdown_now_cancels_a_job_parked_in_a_nested_executor_join`) now synchronise with a capacity-1
  `Channel` instead of a `time.sleep_ms` guess at when a job has reached its checkpoint. A new ratchet,
  `tests/no_wall_clock_ratio_gates.rs::no_new_rust_test_reads_a_wall_clock`, pins the 28 Rust tests
  still allowed to read `.elapsed()`/`Instant::now()` by NAME, so an 18th cannot be added silently and a
  converted test must drop off the list in the same commit. 12 more tests in the same family
  (`checker` recursion-depth timing, `over_memory_counts_jobs_queued_but_not_started`, and others) are
  left for a follow-up ticket — see TICKET-050's `## Decisions` for the full list and why each is
  deferred.
- **An annotated `let` now reports one diagnostic per mistake, not two (2026-09-03, TICKET-044).**
  `m: Map[float, str] = {}` printed `chezzi: 2 type errors`; now `chezzi: 1 type error`. Root cause:
  `check_stmt`'s single-name `let` arm (`src/checker/sig.rs`) resolved the type annotation TWICE — once
  to seed `expected_hint`, once to compute `declared` — and `resolve_type` reports diagnostics
  (`unknown type 'X'`, the Map-key/Set-element Hashable ban) as a side effect of resolving, so every
  diagnostic it emitted was doubled. Fixed by resolving the annotation once into a local and reading it
  at both sites. Covers every diagnostic `resolve_type` emits, not only Hashable: `x: Nope = 1` went
  from two `unknown type 'Nope'` errors to one.
- **A binary operator in a hinted position now hands the expected type to BOTH operands, not just the
  left (2026-09-02, TICKET-034).** `infer_binary` (`src/checker/pattern.rs`) re-installs the single
  `expected_hint` slot per operand, matching the hop `infer_if_else_chain` already makes per branch.
  Fixes eight positions (annotated `let`, `return`, a non-generic call/ctor argument, a `List`
  element, an `if` else-arm, a `match` arm, and a chain of concats) across nine operators (`+ - * /
  % | & ^` and comparison). Does not fix an argument of a GENERIC callee (`infer_generic_arg_tys`
  gets no hint at all) or `??` (`ExprKind::NullCoalesce`, not a `BinaryOp`) — both stay open. One
  visible consequence: a concat at a `List[Any]` element slot is now accepted and renders like its
  literal twin, matching CPython (`[1] + ["a"]` prints `[1, 'a']`).
- **The int→float ELEMENT widen now reaches a call argument, a method argument, a struct constructor
  argument and a `return`, not just an annotated `let` (2026-09-02, TICKET-033).** Before: only
  `xs: List[float] = [1, 2]` widened; `f([1, 2])`, `return [1, 2]` at `-> List[float]`, and
  `S([1, 2])` at a `List[float]` field all wrongly rejected `List[int]`, though their SCALAR twins
  (`f(1)`, `return 1`, `S(1)`) were already accepted. Fix: the checker's per-literal verdict
  (`ListWidenTable`'s value widened from `bool` to `ElemWiden { Decline, Default, Widen(hint) }`) now
  rides to the type-blind backend from every sink, not just the `let`'s syntactic annotation — the
  license is installed at `check_args_range_decl` (covers call/method/ctor/keyword args through one
  seam) and `check_return`, both computed from the RESOLVED sink type via `float_elem_hint_ty`. Two
  side effects, both deliberate: a whole-collection alias (`type LF = List[float]`) is now a type
  context too (the verdict travels on the table instead of requiring the backend to see through the
  alias), and `List[float]?`/nested/erased/default sinks stay declined — the license reads the sink
  type BEFORE any carrier unwrap. Gate: `cargo test` green (4397 lib tests, up from 4394 + the ticket's
  own previously-failing pinned test), `chezzi test tests/chz/spec/float_elem_widen_sinks_test.chz`
  (6 new tests, all RUN the widened value, not just check-clean), `cargo clippy -- -D warnings` clean.
  `docs/reference/spec.md`/`docs/reference/syntax.md` updated in the same commit.
- **`chezzi test` now prints the same fault trace, line:col and warning path as `run`/`check`
  (2026-08-27, TICKET-010, `docs/contributing/gaps.md` W8-37 closed).** Three drops, one root cause
  (`src/test_runner.rs` rendered all three diagnostics from `message` alone): a fault lost its
  callee frames and its col (`ERROR t (f37_test.chz:2) index 9 out of bounds (len 1)` →
  `ERROR t (f37_test.chz:2:12) index 9 out of bounds (len 1)` plus
  `  at boom (called at f37_test.chz:6:5)`); a checker type error in a test file lost `line:col`
  (`ERROR h37_test.chz` / bare message → `ERROR h37_test.chz` /
  `type error (h37_test.chz:2:14): cannot assign str to variable of type int`); a warning lost its
  path (`warning (line 5, col 5): …` → `warning (w_test.chz:5:5): …`, so a multi-file
  `chezzi test tests/chz` run now names which file warned). Routes through the renderers `run`/
  `check` already use: `CheckError::render(path)` for the checker paths, and a new
  `vm::format_frames` (extracted from `vm::format_trace`, byte-identical for `chezzi run`) plus
  `Vm::take_fault_trace` for the fault path. A fault's captured trace always carries one spurious
  frame — the invoke frame `Vm::invoke_test`/`invoke_suite_method`/`build_suite_instance` pushes
  with `Span::RUNTIME` — which the new `test_runner::fault_site` drops before rendering, so a plain
  failing `assert` renders no `  at ` line. `--errors=json` is unchanged (still `line` only, no
  `col`/`file`). Gate: `cargo test` green (4251 lib tests + all integration targets), clippy clean,
  `chezzi test tests/chz` at both the default and `CHEZZI_THREADS=2` worker counts — exit 0,
  `639 test(s): 639 passed, 0 failed, 0 errored`, zero `  at ` lines. New CLI gate
  `tests/test_diagnostics.rs` (5 cases) plus 3 `#[cfg(test)]` cases in `src/test_runner.rs`.
- **A method/field miss now suggests a near-miss "did you mean" (2026-08-27, TICKET-007,
  `docs/contributing/gaps.md` W8-17 partially closed).** `xs.lenght()` → `type List[int] has no method 'lenght'`
  now also carries `help: did you mean 'len'?`, on every method-miss and field-miss site (native
  handle, struct, newtype, enum, protocol, type-param bound). `CheckError` gained a `help: Option
  <String>` field (never module-attributed), scored by `src/checker/suggest.rs`'s restricted
  Damerau-Levenshtein distance discounted for length difference — derived by MEASURING rustc
  1.97.1's own suggestions (`lenght`→`len` needs the discount; plain Levenshtein's distance is 3
  against a limit of 2) rather than reading rustc's source. `Checker::method_names`/
  `field_names`/etc. sort every `HashMap`-backed candidate table before scoring, so a distance tie
  picks the same candidate on every run. `chezzi check`/`chezzi run` now also echo a three-line caret
  snippet of the source line (`lexer::render_snippet`, reusing `editor::word_end_col` for the caret
  width) before the `help:` line; the caret survives a piped/`/dev/stdin` entry because the entry
  source is threaded down rather than re-read (DEC-001). `--errors=json` gained a `"help"` key
  (after `"message"`, omitted when absent) and the LSP appends `help: <h>` to the diagnostic
  message. Out of scope (recorded in `docs/contributing/gaps.md` W8-17): error codes; a suggestion for an unknown
  variable, a module member, or an enum variant miss.
- **A bare generic fn passed to a GENERIC hof is now INFERRED, the way Go and Rust infer it
  (2026-08-15).** The leftover the entry below named: the deferral it built lived only on
  `infer_generic_method`, so when the fn-typed slot belonged to a **generic free fn** the callee's own
  `[U]` was never pinned first and the read was refused. Measured pre-change: `applyg(id, 5)`,
  `applyr(5, id)` and `twop(id, 5)` (for `applyg[U](f: fn(U) -> U, n: U)`, `applyr[U](n: U, f: fn(U) ->
  U)`, `twop[A, B](f: fn(A) -> B, a: A)`) all reported *'id' is generic and T is not determined here*,
  while **Go 1.26.5 and Rust 1.97 each print `5` for all three** — and the control proves the diagnosis:
  making the same HOF non-generic (`applyc(f: fn(int) -> int, n: int)`) already worked. This is a
  **missing inference**, not a rejection relaxed: the information is at the call site, in the other
  arguments. `infer_generic_call` now runs the SAME three pieces `infer_generic_method` does — the
  prepass wall silenced (`generic_fn_value_prepass`), a bare generic-fn argument the slot cannot pin
  YET deferred past pass-1 unification instead of binding the callee's `[U]` to its own leaked
  `Ty::Param` (first-binding-wins makes that permanent), and the verdict re-asked at the end by
  `report_undetermined_generic_fn_value_args`. **No second derivation**: `pin_generic_fn_value` /
  `try_pin_generic_fn_value_arg` / the one diagnostic are reused as-is; the only free-fn-specific
  choice is WHERE the deferred pass runs — after `seed_from_hint`, because this path has an annotation
  source the method path does not, and before `enforce_bounds` so a bound on a param it fills is still
  checked. So `xs: List[int] = mklist(id)` pins `U` from the annotation (Rust infers this; Go has no
  assignment-context inference at all and refuses it, as it refuses `xs: List[int] = empty()`). **What
  must stay refused, and does — Go and Rust agree on every one:** `nopin(id)` (nothing determines `U`;
  Go *cannot infer U*), `applyg(mk, 5)` (shapes cannot match), `twop(mk, 5)` (`B` reachable only through
  `mk`'s own undetermined `T`; Go *cannot infer B*), `applyg(konst, 5)` for a `konst[T, S]` whose `S` is
  in no position, and `take(id)` on `fn take[U](f: U) -> int`, whose slot is not a function type at all.
  A slot that pins CONCRETELY but WRONG (`applyg(to_text, 5)` → `fn(int) -> str` into `fn(int) -> int`)
  now gets the ordinary assignability diagnostic, which is what Go says there too. Because this
  **widens** what compiles, every newly accepted program was RUN and its value asserted on both engines
  (`tests/chz/spec/list_test.chz`), including the downstream uses that prove the type is real and not an
  unresolved placeholder — `applyg(id, 5) * 2 == 10`, `applyg(id, "s").upper() == "S"`, and
  `applyg(id, 5).upper()` still rejected. **One defect found by re-deriving the premise rather than by
  the suite, which stayed green through it:** the silencing licence is a `Checker` flag, so wrapping the
  whole argument prepass in it leaked into every NESTED read — `take2(Bx(ident), 5)` went from a clean
  reject to `ok: no type errors`, check-cleanly building a `Bx[fn(T) -> T]`, exactly the
  value-of-unresolved-type this widening had to avoid. The licence now covers the IMMEDIATE
  bare-identifier arguments only (the one shape `bare_generic_fn_value_arg` can re-pin), scoped inside
  the shared `infer_generic_arg_tys` — which also closes the same hole on the METHOD path, where
  `Holder(0).m(Bx(ident), 5)` had been wrongly accepted since the licence was introduced. 438-file
  `.chz` corpus sweep, pre vs post binary (both built from THIS worktree, so no stdlib-path noise):
  **zero** verdict changes. 4160 lib tests + 550 Chezzi tests identical on both engines.
  `docs/reference/syntax.md` §7b, `docs/reference/spec.md`.
- **The empty-receiver carve-out suppresses the REPORT, never the PIN — and stops hiding the rule's
  own subject (2026-08-15).** Two defects in the argument-position rule below, both found by
  adversarial review and reproduced against the `69aa9efc` binary. (1) **A regression.** The
  `!wp.iter().all(ty_fully_concrete)` gate lived in the SHARED derivation, so it disabled the pin as
  well as the report: `[].fold(0, add)` for `add[T: Add](a: T, b: T) -> T` printed `0` before the rule
  and afterwards reported *argument to 'fold' has type fn(T, T) -> T, expected fn(int, ?) -> int* —
  the pre-rule wording the rule exists to replace. `fold`'s slot is `fn(A, E) -> A`, and an empty
  receiver makes `E` = `?`, so the whole slot read non-concrete even though argument ZERO determines
  `T` outright; Go's `Fold([]int{}, 0, add)` returns `0`. The carve-out was only ever tested on `map`,
  whose slot has **no concrete parameter** to expose it — so the new test uses `fold`. `pin_generic_fn_value`
  now gates the pin on the RESULT alone (`ty_has_param` + `ty_fully_concrete` on `refined`), and each
  REPORTING caller applies the carve-out itself. (2) **A pre-existing escape**, the rule's own subject:
  `Bx(0).two(ident, ident)` on `fn two[U](self, f: fn(U) -> U, g: fn(U) -> U) -> List[U]` checked clean
  and printed `[]` on BOTH binaries — a `List[U]` with `U` determined by nothing (Go: *in call to two,
  cannot infer U*). Both fn args defer, neither pins, `recover_return_only_params` degrades `U` to `?`,
  and the carve-out — asked of the SUBSTITUTED slot — could not tell that `?` from an empty receiver's.
  The method-path report now asks it of the **unsubstituted** slot (`fn_slot_params_have_unknown`):
  `[].map(ident)`'s `fn(?) -> U` carries the receiver's sentinel *before* any binding, `two`'s
  `fn(U) -> U` does not. The hint path (`infer_ident` Scope A) keeps its own predicate
  (`fn_slot_params_concrete`) because a hint is already fully substituted and may hold a RIGID outer
  type param — `fn f[T](): g: fn(T) -> T = ident` is legal and stays accepted. 438-file `.chz` corpus
  sweep, pre vs post binary: **zero** verdict changes outside the file the new tests were added to
  (`std/*.chz` differ only because a binary built from another worktree fails its own stdlib-path gate
  — the `69aa9efc` binary shows the same). 4157 lib tests + 549 Chezzi tests identical on both engines.
- **…and at an ARGUMENT-position read too, deferred to the end of the call (2026-08-15).** The rule
  above only asked its question where an `expected_hint` was absent, so one function got two verdicts
  depending on where it was read. Measured on the pre-change binary: `g := pred` rejected, but
  `take_bool(pred)` printed `1`, `[1,2,3].filter(pred)` printed `[1, 2, 3]`, and `[1,2,3].map(mk)`
  printed `[[], [], []]` — a `List[List[T]]` whose `T` nothing ever determined. Go refuses all three
  (`./e.go:12:10: in call to takeBool, cannot infer T`). Now every one of them gets the SAME sentence
  the binding gives, at the argument's own span. **The ordering is the whole difficulty and the reason
  the check is DEFERRED**: `[1,2,3].fold(0, pick)` for `pick[T](a: T, b: T) -> T` is pinned by the
  accumulator, argument ZERO, while `pick` is argument one — a per-argument eager check refuses a
  program that runs today (`3`) and that Go accepts. So the verdict is re-asked once, after
  `recover_return_only_params`, when the call's substitution is as bound as it will ever get
  (`report_undetermined_generic_fn_value_args`); it is idempotent with the interleaved pin because
  `unify` is first-binding-wins, so the map only grows. **ONE derivation, not a second look-alike
  rule:** `pin_generic_fn_value` (`src/checker/mod.rs`) is now the single answer to *"does this
  position determine every type parameter?"*, returning `Pinned` / `Undetermined` / `Skip`, and
  `reject_undetermined_generic_fn_value` (`src/checker/pattern.rs`) is the single diagnostic — asked by
  `infer_ident`'s Scope A (expected-type hint), by the generic-method argument pin
  (`try_pin_generic_fn_value_arg`), and by the deferred check. `Skip` is what keeps the rule honest:
  a non-matching-arity or non-`fn` slot belongs to the arity/shape diagnostic, and a slot whose own
  PARAMETER positions are `Unknown` is the empty-collection sentinel, not an undetermined parameter —
  `[].map(id)` and `[].map(mk)` still print `[]`. A slot's `Unknown` RETURN does not excuse anything
  (`RwShared[List].for_each`'s `fn(int) -> ?` slot rejects a `sink[T](n: int)` whose `T` is nowhere).
  Two things the one derivation fixed for free: `q.for_each(gs)` for `gs[T](x: T)` was a FALSE reject
  (*argument 1 of 'for_each': expected fn(int) -> ?, found fn(T) -> nil*) because the old Scope A
  demanded a fully concrete hint including the return; and `b.app(ident, 5)` on
  `fn app[U](self, f: fn(U) -> U, x: U) -> U` — the fn slot BEFORE the argument that pins it, the order
  no builtin can spell — reported *argument to 'app' has type int, expected T*, because pass-1 `unify`
  bound `U` to the rigid callee's leaked `T` and first-binding-wins made it permanent. A bare
  generic-fn argument the slot cannot pin yet now DEFERS its pass-1 unification past the value
  arguments (same shape as Bug D's `mask_closure_ret`), so both argument orders work. Every builtin
  HOF enumerated and covered: `map`, `filter`, `fold`, `sort_by`, `sort_by_key`, `min_by`, `max_by`,
  `take_while`, `drop_while`, `count`, `position`, plus `RwShared`'s `for_each`/`for_each_entry`/
  `fold`/`fold_entries` and `Shared.update`/`RwShared.write`. Still NOT covered, unchanged and
  measured: a parameter/field default value (`fn run(f: fn(int) -> int = id)` — a concrete slot the
  checker does not thread into the hint). The generic FREE-fn HOF path (`applyg(id, 5)` for
  `fn applyg[U](f: fn(U) -> U, x: U) -> U`) rejected at the read on both binaries here — its pass-1
  leak is the same one fixed above but on `infer_generic_call`, and moving it there is a widening of
  what is ACCEPTED, so it got its own change with its own proof (the entry above, same day).
  438-file `.chz` corpus sweep,
  pre vs post binary: **zero** verdict changes outside the file the new tests were added to. 4156 lib
  tests + 549 Chezzi tests identical on both engines. `docs/reference/syntax.md` §7b, `docs/reference/spec.md`.
- **An UNINSTANTIATED generic fn value is refused at the binding, Go's rule (2026-08-15).** `g := id`
  for `fn id[T](x: T) -> T` was accepted, and the mistake surfaced at the eventual call as
  *argument 1 of 'closure': expected T, found int* — blaming a `closure` the user never wrote and
  naming a `T` there is no way to act on; a binding that was never called was accepted **silently**
  (`g := id` alone: `ok: no type errors`, and `g := mk` for `mk[T](n: int) -> List[T]` even ran,
  printing `[]`). Go refuses exactly this spelling at the READ (*cannot use generic function id
  without instantiation*); every other shape already agreed with Go, so this was the single
  divergence. Now: *'id' is generic and T is not determined here, so it cannot become a function
  value — instantiate it (`id[<T>]`), or give this position a concrete function type
  (`fn(<T>) -> <T>`), writing a real type in place of each `<…>`*. The `<…>` shape is rendered
  from the fn's OWN signature, the undetermined parameters are named, and the turbofish is offered
  only for a **single**-parameter generic — a fn-value turbofish carries exactly one type argument, so
  suggesting `pair[int]` for `pair[A, B]` would be advice that cannot work. Fires only where the
  parameters are undetermined: every pinned spelling is untouched (turbofish, annotation, HOF parameter,
  return position, struct-field slot, `.map`/`.fold`/`.filter` element slot), as is a non-generic or
  builtin fn value and any local/param that merely shadows the name. Two traps found while building
  it: (1) the `.map(conv)` prepass reads the fn bare with no slot type yet and is pinned *afterwards*
  by `try_pin_generic_fn_value_arg`, so the wall is held back there (`generic_fn_value_prepass`);
  (2) gating on `expected_hint.is_none()` alone let the rule **cancel itself** through an inferred
  return — `fn get(): return id` took the rule's own `Ty::Unknown` as the inferred return type, and
  the re-check then saw a `Some(Unknown)` hint, so `g := get(); g(1)` printed `1` check-clean;
  `Unknown` determines nothing and now counts as nothing. The witness wall
  (`reject_witness_fn_value`) still fires first — its advice differs, since a turbofish does not help
  there. 438-file `.chz` corpus sweep, pre vs post binary: **one** verdict change, the
  `g := tagged` probe in `tests/chz/spec/static_witness_test.chz` (now `tagged[Node]`, and it asserts
  the call). `docs/reference/syntax.md` §7b, `docs/reference/spec.md`.
- **Adversarial review of the rule above, applied (2026-08-15).** Two prosecutors ran the change
  against the pre-change binary over ~160 probes plus a 104-case context fuzz. Four defects, all fixed
  in the follow-up commit. (1) **The message was FACTUALLY FALSE in three positions.** A parameter or
  field DEFAULT value has a fully concrete slot (`fn run(f: fn(int) -> int = id)`) that Chezzi simply
  does not thread into `expected_hint`, so "nothing here determines T" was untrue — and the accurate
  pre-change diagnostic (*"default value for parameter 'f': expected fn(int) -> int, found
  fn(T) -> T"*) had been replaced by it. Worse, `xs: List[fn(int) -> int] = [id]` was told to *"annotate
  the binding with a concrete function type"* — which the user had already done, at the only
  annotatable point. And `print(id)` / a `yield` have no binding to annotate at all. The message now
  says what is true of THIS read — `'id' is generic and T is not determined here … instantiate it
  (`id[<T>]`), or give this position a concrete function type (`fn(<T>) -> <T>`)` — naming the
  POSITION, never claiming no information exists anywhere. (2) **The suppression flag was set in the
  shared `infer_generic_arg_tys`, silencing the wall at all SEVEN of its callers** when only ONE
  (`infer_generic_method`) re-pins the argument afterwards. So a generic CTOR argument slipped through
  to the very message this rule exists to replace: `struct Bx[T]: f: T` + `b := Bx(ident)` still said
  *"argument 1 of 'f': expected T, found int"*. The flag moved to that one call site; the other six
  now reject at the read. (3) **The guard's `Ty::Unknown` drew a false second error**: a ONE-element
  `xs := [id]` matched `is_unrefined_empty_coll` (a test on the TYPE) and the finalize added *"cannot
  infer element type of empty collection; add a type annotation"* — both halves false, and the
  suggested annotation does not help. Pre-existing (the witness wall reached the identical pair via
  `xs := [reset]` on the pre-change binary), but this rule made it far more reachable; the registration
  now asks the EXPRESSION whether the literal is empty. (4) The rendered signature dropped optional
  arity, advertising a stricter value than a plain fn read gives; `with_min` restored. Charges that did
  NOT stick, after genuine effort: no soundness break, no check-clean-then-runtime fault, no flag leak,
  no sibling of the inferred-return bypass, and 0 corpus diffs. Re-swept after the fixes: **0** verdict
  changes over 438 files.
- **A lex error points at the offending CHARACTER, on both axes (M24-7, 2026-08-14).** `LexError`
  carried a line but no column, and the resolver seam hardcoded `col: 1`, so a lex error was the one
  diagnostic in the compiler that could not point at a character — the LSP squiggle landed on the
  start of the line. Measured `chezzi check`, before → after: `z := "ab\u{12zz}cd"` →
  `lex error (line 3): …` → `lex error (line 3, col 14): …` (the first `z` of `12zz`); `y := 0x` →
  col 6 (the `0`); `y := "a\qb"` → col 9 (the `q`); `y := "abc` unterminated on line 2 of a 2-line
  file → `(line 3, col 1)` → `(line 2, col 6)`, the opening quote — which fixes the **line** too, as
  the position was being read off the cursor after it ran to EOF. **The rule** (measured against
  rustc + CPython, table in `docs/contributing/gaps.md` M24-7 and in the test's doc comment): point at the
  offending character; an unterminated delimiter points at its opener; a malformed number points at
  the literal's start — **except a misplaced `'_'` separator, which points at the `_` itself**
  (CPython measured: `y = 1__0` → offset 6; rustc has no opinion, it accepts `1__0`) — and an escape
  error points at the escape char, not the `\`. A `\`+newline line continuation points at the `\`
  (neither ancestor rejects it, so the choice is Chezzi's), and an error about a `\u{…}` escape *as a
  whole* points at its `{` — 1-2 columns left of rustc, which anchors those at the `\` and
  caret-**spans** the escape; a `LexError` carries a point, not a span. Mechanism: `col` on
  `LexError`, `Display` is `lex error (line {}, col {}): {}` (matching `Span`'s), and
  `src/resolver/mod.rs` reads `e.col` — the LSP, `--errors=json` and the plain-text renderer all ride
  along unedited. `Lexer::error` stays the sole constructor path, now `error()` → `error_at(pos)` →
  `error_span(span)`; `error_at` asks `span_at`, so a column inside a re-lexed interpolation fragment
  composes through the M24-6 `PosMap` for free. All 35 construction sites audited. `Span` untouched
  (still 12 bytes). **The interpolation seam needed one more edit to actually deliver it:**
  `interpolation::parse_expr_str` built its `InterpError` from the *string literal's* span, so
  `s := "a\tb{1 + @}c"` reported *type error (line 2, col 6): lex error (line 2, col 16): unexpected
  character '@'* — the inner text named the `@`, the outer position (the one the LSP squiggles) named
  the opening quote. The `InterpError` now carries the `LexError`'s own line/col, the checker reports
  `e.span`, and the doubled position is gone: *type error (line 2, col 16): lex error: unexpected
  character '@'*. Tests: `lexer::tests::lex_error_points_at_the_offending_character` (18-case table,
  every column hand-counted from the source rather than read off the implementation, and covering the
  three families this change *chose* rather than measured — the `'_'` sites, the three line
  continuations, the whole-escape `\u{…}` sites), `lex_error_display_carries_the_column`, the extended
  `checker::tests::a_lex_error_inside_a_fragment_reports_its_real_line` (pins the OUTER `e.span` at
  col 16, the fragment column only the `PosMap` knows), and
  `check_errors_json::lex_error_carries_a_real_column` (CLI-level JSON + plain text).
  **Closed the same seam's other three-and-a-half discards (2026-08-14).** The lex arm was one of
  four error paths through `interpolation::parse_interpolation`, and the other four all still
  reported the string literal's opening quote. Every one now names a real character, each measured
  against CPython 3.14. A fragment **parse** error: `s := "a\tb{1 + }c"` → *(line 2, col 6)* →
  **(line 2, col 15)**, one past the last token consumed — the identical rule the top-level parser
  uses for that message (`y := 1 + ` → col 10); `tokenize_frag` had made `ParseError.span` correct
  all along and only this seam threw it away, so the fix is `span: e.span` (CPython points inside the
  fragment too, at the `+`, offset 14). **Unterminated `{`**: `s := "a\tb{1 + c"` → col 6 → **col
  17**, the closing delimiter — exactly CPython's offset 17. **Unmatched `}`**: `s := "a\tb}c"` →
  col 6 → **col 11**, the `}` itself — exactly CPython's offset 11. **Format-spec error**:
  `s := "a\tb{x:>99999999}c"` → col 6 → **col 14**, the spec's first char (`fmtspec::parse` reports
  no offset of its own, and a format spec is a runtime concern in CPython, so there is no ancestor to
  copy — the rule is simply "inside the fragment, as precise as the information allows"). Checker
  errors on a fragment *expression* were already right (`"a\tb{nope}c"` → col 12) — the AST they run
  on carries `tokenize_frag`'s spans — and `desugar`'s two `parse_interpolation` callers discard the
  error entirely, so the whole seam is closed. Tests:
  `checker::tests::a_parse_error_inside_a_fragment_reports_the_fragments_own_position` and
  `literal_shape_errors_point_at_their_own_char`, both with a real `\t` escape before the offending
  char so a column counted off the post-escape content instead of composed through the `PosMap`
  fails them.

- **The last two stutters/drifts in that family, closed 2026-08-15.** (1) The **primary** lex-error
  seam still doubled its position: `resolver::parse` built the message from `LexError::to_string()`,
  so a module-graph lex error rendered *resolve error (line 1, col 8): in module 'dtest.badmod': lex
  error (line 1, col 8): unexpected character '@'* — once from the outer renderer, once from the
  inner `Display`. It now uses `e.message` under the `lex error: ` prefix the interpolation seam
  already used; one rendering rule for both paths (`check_errors_json::lex_error_carries_a_real_column`
  asserts the whole line). (2) The **unterminated `{`** column could name a position the file does
  not have. It is `map.at(<content length>)`, which used to EXTRAPOLATE from the last checkpoint —
  correct only while the literal's final content char shares the delimiter's line, so a literal
  ending in a real newline reported a column past the end of the PREVIOUS line: `x := """a` /
  `{b` / `"""` said *(line 2, col 4)* of a two-character line, and blank lines before the
  terminator drifted the caret onto one of them (fuzzing: 37/5000 interpolation inputs produced an
  out-of-range column, all from this arm). The lexer now checkpoints one-past-the-end with the
  closing delimiter's own span (one extra `PosMap::note` after the scan loop in `string()` and
  `triple_string()`), so `at(len)` is a real position by construction — those two cases now report
  *(line 3, col 1)* and *(line 6, col 1)*, the delimiter itself, and single-line literals are
  unchanged. Test: `checker::tests::unterminated_fragment_points_at_the_real_closing_delimiter`,
  which also asserts the reported column exists on the reported line.

- **`examples/panic.chz` type-checks — the last allow-listed example is gone, and its recorded reason
  was wrong (2026-08-06).** `chezzi check examples/panic.chz` failed with `line 29, col 22: expression
  returns no value (nil) and cannot be used as a value`, while `checker::tests::all_shipped_examples_typecheck`
  exempted it as "a top-level `panic(...)` demo whose result is used in value position". **That was not
  the cause**: `x := if true: 1 else: panic("unreachable")` checks clean on its own. The real one is
  `recover:` over a VOID call yielding `Result[nil, Error]`, so `Ok(v)` + `"ok: {v}"` interpolated nil.
  Both arms now bind `Ok(_)`; `panic.expected` is byte-unchanged (the `Ok` arm is never taken — both
  recoveries yield `Err`). The `known_check_failures` allow-list and its filter are **deleted**: every
  `examples/*.chz` now type-checks, verified by the test and by a CLI sweep of all of them.
  Two reports investigated in the same pass turned out to be **not bugs**, and their misleading comments
  were corrected instead: (1) `channel.expected` / `implicit_nursery.expected` are not serial-only — the
  measured M:N divergence (21/30, 30/30) is `chezzi run`'s **streamed** sink, where cross-task order is
  documented-nondeterministic on both engines; the `.expected` files are consumed only by lib tests on
  the **buffered** sink (`HostConfig::stream = false`), which is deterministic on serial and M:N alike,
  which is why the goldens' exact M:N asserts do not flake. (2) `log_demo.expected` is not stale — it
  pins the STDERR stream by design and `golden_log_demo_via_run_file` asserts the two stdout lines
  separately, plus the cross-stream discrimination. Both examples now say so in their header comments.

- **Refactor — a native's behavioural properties now ride its REGISTRY ENTRY, so forgetting one is a
  compile error (2026-08-05, `docs/design/roadmap.md` §3c option B + the interception fold).** A native's
  properties used to live in string matches far from where it is registered: a 40-name `is_blocking`
  list (with a `strip_prefix('_')` patch bolted on so the W7-8 rename would not silently un-classify
  every `std.fs` syscall), three `"sleep_ms"` arms in `vm/call.rs`, a `name == "connect" || "listen"`
  check, and a `fn_addr_eq` identity test — `sleep_ms` named in 4 files. **A new blocking native that
  forgot the list failed SILENTLY**: no error, no red test, it just pinned an M:N core worker for the
  syscall (the D5 starvation the classification exists to prevent).
  Now `Kind { Inline, Blocking, TimedWait, InterceptIo, InterceptNet }` is a field of every `MEMBERS`
  tuple (192 entries, 14 tables), copied onto `Obj::Native` at bind time and carried through
  `WireValue`/`SnapValue`/`Callee::Native` into `invoke_native(func, name, kind, args, span)`. **No
  native's behaviour is decided by a string comparison anywhere in the VM**, there is no name→kind
  lookup (the kind rides the value — `invoke_native` has exactly one call site), and `is_blocking` +
  the bare-name-uniqueness guard are **deleted**. Bare-name keying was in fact unsound: `std.io::_append`
  (an intercepted opener) and `std.fs::_append` (a syscall) collide, and were kept apart only by check
  ORDER plus a test exemption list.
  Pure refactor — byte-identical behaviour, re-measured on the release binary: `--timeout=200` still
  aborts a `sleep_ms(3000)` at top level, in a nursery, and in an eager `Executor` in ~200 ms each at
  `CHEZZI_THREADS=1/2/4/8`; 4×`process.cmd("sleep 0.3")` still overlap on one core worker (305 ms
  offloaded vs 1209 ms on `--serial`); `io.create`/`stdout`/`buffered` + `net.listen`/`connect` still
  allocate their handles on both engines. Guarantee demonstrated by dropping one entry's kind →
  `expected a tuple with 3 elements`. Suite 3820 green.
  **Found by the conversion, not fixed there:** `fs.stat`/`fs.walk` were never in the old list, so they
  pinned a worker — the predicted silent omission, already in the tree. Preserved as `Kind::Inline`
  (behaviour-identical) with a `BUG PRESERVED` comment + a test pinning the state, filed as `gaps.md`
  **W7-19** and fixed the next day (below).

- **Fix — `fs.stat`/`fs.walk` no longer PIN an M:N core worker (2026-08-05, `gaps.md` W7-19).** They
  were the only two of `std.fs`'s seventeen members outside the blocking set, so their syscalls ran
  inline on a core worker — `walk` for an entire tree walk — the D5 starvation the set exists to
  prevent. Both ancestors hand the worker off here (Go's runtime releases the P on a blocking syscall;
  CPython drops the GIL around `os.stat`/`os.walk`). Now `Kind::Blocking`, after the off-heap-safety
  proof the gap asked for: both take their path through `Host::arg_bytes`, and both returns are
  primitive `NativeRet`s already crossed by members that offload today (`_list_dir` returns the same
  `Ok(List([Bytes…]))`, `process.run`/`run_args` the same `Ok(Struct{…})`). Measured at
  `CHEZZI_THREADS=1` on a 121k-entry tree, paired against a binary built from the same commit with only
  these two entries reverted: a sibling fiber's worst scheduling gap **136–139 ms → 38–41 ms**, and 4
  concurrent `fs.walk`s **814–825 ms → 449–469 ms** (1.8× — they overlap on the dirty pool instead of
  serializing). The ~39 ms residual is *result lowering*, not the syscall: building the 121k-path list
  as heap objects needs the `Vm`, so it stays on the core worker (it falls to 9–10 ms on an 18k-entry
  tree). `every_syscall_module_member_is_blocking` is now exception-free and is the fence that pins
  the classification; a new `tests/chz` case runs `fs.stat` inside a nursery to cover the offloaded
  `Struct` round-trip (a correctness fence — it passes under either `Kind`, which the review made the
  comment say out loud; `fs.walk` from a fiber was already covered). Suite 3821 green.

- **Fix — an intrinsic protocol method on a built-in is now CALLABLE (2026-07-25, bug-hunt wave-6 W6-3,
  P0).** `fn total[T: Add](xs: List[T], zero: T) -> T` with `acc.add(x)` passed `chezzi check` and then
  faulted `type int has no method 'add'` on BOTH engines — the idiomatic Rust/Go generic shape, broken.
  The checker grants built-ins ~12 protocol conformances *intrinsically* (no user method), but the VM
  hand-intercepted only 2 of them (`compare`, `str`); everything else — `add`/`sub`/`mul`/`div`/`mod`/
  `neg`/`hash` on `int`/`float`, `hash` on `bool`/`str`/`bytes`/a zero-field struct, the arith set +
  `compare` on a numeric `newtype`, `index`/`set_index`/`slice` on `list`/`map`/`str`/`bytes`/`bytearray`
  — fell through to `has no method`. Also reachable without generics via a protocol-typed value
  (`x: Hashable = 5`). Parity-blind (byte-identical on both engines), and the repo's **dominant defect
  class**: a fix applied to SOME arms of an N-way set.
  Fix: one `Vm::intrinsic_proto_method` (`src/vm/call.rs`) that **delegates** to the exact primitive each
  operator form already uses — `arith` (which covers the numeric-newtype grant for free via
  `newtype_arith`), a newly extracted `Vm::neg_value` (`Op::Neg`'s body, now single-sourced in
  `src/vm/arith.rs`), `hash_value` (literally the Map/Set key hash, so `x.hash()` can never disagree with
  `m[x]`/`s.contains(x)`), `compare`, and `get_index`/`set_index`/`get_slice` (with the `Option[int]` → raw
  `Nil`/`Int` unwrap `Slice`'s signature needs). Nothing reimplemented ⇒ `a.add(b)` ≡ `a + b` and
  `c.index(k)` ≡ `c[k]` by construction, fault text included. Wired at 5 **miss** sites (inline scalar,
  the merged built-in-container dispatch, struct, newtype, and the catch-all where a boxed `BigInt`
  lands), so a user method always wins and an ordinary struct method call pays **zero** added cost
  (`do_method_call` stays a Tier-1 perf target); benches re-measured within run-to-run noise.
  **The ratchet, worth more than the fix** — keyed on **(protocol × receiver KIND)**, because that is the
  axis W6-3 actually failed on (`compare`/`str` WERE paired; their interceptions were just type-gated
  narrower than the grant set, which a protocol-keyed table cannot express). Three layers:
  (1) `satisfies_args_d`'s success type is `checker::proto::Grant`, a token with a private field, so a new
  early-out written the way every pre-existing one was — a bare `return Ok(())` — **does not compile**; the
  author must pick `grant_intrinsic` (registers the grant) or `Grant::no_intrinsic_method`;
  (2) `grant_intrinsic(protocol, ty)` `debug_assert`s that `(protocol, intrinsic_recv_kind(ty))` has a row
  in `INTRINSIC_PROTO_METHODS` (51 paired rows) or `INTRINSIC_UNPAIRED` (0 carve-out rows — W6-3b emptied it; registering one is still the only legal way to ship an unpaired grant);
  (3) `vm::tests::intrinsic_grants_all_have_vm_arms` sweeps the whole **(protocol × kind) cross product**
  (165 cells): the accepted-cell set must equal the registered rows, then every paired row's generated
  call probe RUNS on both engines. Verified RED both ways: a bare `Ok(())` grant fails to compile, and
  widening `Comparable` to `bytes` (which the earlier protocol-keyed ratchet passed) now fails the suite.
  Three carve-outs FILED rather than silently shipped: `Iterator`→`next` on a raw collection (**W6-3b**,
  stateful, no cursor position — **since FIXED, 2026-07-26: the grant was narrowed to real cursors and
  `INTRINSIC_UNPAIRED` is now empty; see the top entry**), `compare` on a **NaN** operand (**W6-3c** —
  first shipped as an explicit recoverable fault instead of W6-3's own `has no method` symptom; **now
  FIXED (2026-07-26)**, it answers `sort()`'s total order — see the wave-6 tail entry below), and a
  numeric `newtype` that
  DEFINES `add`/`compare` (**W6-3d** — the method form gets the user method, the operator form the
  underlying's native op; reqs "≡ the operator" and "never shadow a user method" genuinely conflict there).
  Tests: `tests/chz/spec/intrinsic_proto_methods_test.chz` (20 `test fn`, operator-equivalence AND
  fault-message equality via `recover:`, user-method-wins controls, plus the W6-3d divergence pin, the
  NaN total-order pin and the Iterator-is-a-cursor pin),
  serial==M:N. Docs: `docs/contributing/gaps.md` (W6-3 FIXED + new W6-3b/c/d), `docs/reference/spec.md`, `docs/reference/syntax.md`.
- **Diagnostic — recursive *local* fn crossing the airlock (2026-07-18, bug-hunt).** A nested (local)
  recursive `fn` sent across a task boundary used to fault with the misleading `maximum structural depth
  (10000) exceeded (cyclic data structure?)` — there is no cyclic *data*, just the compiler letrec's
  self-cell making the closure's capture graph self-referential (`Closure h -> Cell -> h`), tripping the
  generic depth guard. The two closure-serialization arms (`to_wire_depth` / `to_snap_depth`,
  `src/vm/sched.rs`) now scan the crossing closure's capture graph for its own handle (new
  `graph_reaches_handle`, sibling of the Task-2b ref-capture scan) and raise a clear, **recoverable**,
  byte-identical-on-both-engines error: `a recursive local fn cannot be sent across a task boundary — hoist
  it to module scope (a module-global recursive fn is sendable)`. Fires at every airlock arm (`spawn:`
  block, `spawn f()` callee, `spawn f(g)` arg, `Channel[fn].send`). **Diagnostic only — actual
  recursive-local-fn sendability stays DEFERRED** (a risky VM change past the JIT freeze); the fix is to
  hoist the fn to module scope (crosses as a plain `Func`, no capture, sendable). Genuine cyclic *data*
  still reports the depth message (regression-locked). +2 tests (`airlock_recursive_local_fn_clear_diagnostic_both_engines`,
  `airlock_module_global_recursive_fn_control_sends`). Docs: `docs/reference/concurrency.md` §7, `docs/contributing/gaps.md`.
- **QoL — int-const if/match-EXPRESSION branch widens to float (2026-07-17).** A bug-hunt papercut:
  `x := if c: 1 else: 2.5` was a compile error while the equivalent list literal `[1, 2.5]` coerced fine —
  an inconsistency in where the untyped-int-constant→float peephole applied. Extended the SAME
  `literal_numeric_mix` mechanism (float-const sibling → widen the int-const siblings) to if/match-expression
  tail branches, on BOTH sides under one predicate: checker `branch_widen` (in `infer_if_else`/`infer_match`)
  widens the branch type, compiler `compile_if_expr`/`compile_match_expr` emit `Op::CoerceFloat` on the
  int-const branch — identical `untyped_int_const` guard, zero drift. Sound (proven no int-under-float,
  both engines: `if_match_expr_int_float_widen_parity`). A TYPED int branch still rejects (the V1 hole
  boundary), and multi-`return` inference is UNTOUCHED (still conflicts — a separate join, not a widening
  sink). Consistent with `[1, 2.5]`; docs in `syntax.md` (if-expression) + `spec.md` (widening contexts).
- **P0 fix — a cancelled task's `defer` silently did not run on M:N (2026-07-14).** Pre-existing
  scheduler race. A cancel trip and its `cancel_drain` (which requeues the scope's PARKED fibers so they
  unwind) sit two core-lock acquisitions apart; an idle worker's `take_runnable` landing in that gap saw
  `running == 0 && runnable == 0 && parked_n > 0 && done < total`, called the teardown a **deadlock**, and
  `flag_deadlock` dropped the still-parked sibling **without `unwind_deferred`** — its `defer`s never ran.
  Invisible: `reduce_task_slots` ranks `Exit > Fault > Deadlocked`, so the real sibling fault surfaced and
  the lost `defer` was the only symptom (`defer` is the language's ONLY cleanup mechanism — an unclosed
  file, an unreleased lock, silently). Fixed with a **cancel-teardown veto** in `MnSched::is_deadlocked`
  (`SchedCore::any_incomplete_scope_cancelled`): a scope with `cancel` set and `done < total` is
  mid-teardown, not deadlocked. Put at the predicate, not at the one seam that reported it, because
  **three** seams trip a cancel and drain in a later lock acquisition. The veto alone was not enough —
  the two abort seams (`abort_enlisted_scope`, `abort_eager_nursery`) *cleared their own* deadlock veto
  (`awaiting_builder` / `body_open`) before arming this one, so they now trip the cancel FIRST
  (`MnSched::trip_scope_cancel`, a store **under the core lock** — a bare `Relaxed` store outside it had
  no synchronizes-with edge to the worker evaluating the predicate). Two more holes in the same
  invariant, both fixed here: the **panic-fault** path (a worker-VM panic → `Vm::panic_outcome`) never
  tripped the cancel at all, so the requeued siblings re-parked and the scope quiesced *uncancelled*;
  and the netpoller's `register` gated the park on the **outermost** nursery's flag, so a fiber of a
  cancelled INNER scope could park on an already-swept poller (which would have made the veto permanent
  → deadlock detection disabled sched-wide) — `poll_park_offload` now hands it the per-scope cancel, and
  the dead sched-level `MnSched::cancel` field is deleted. Transient by construction (park/park_wait/
  poller-register all refuse to park a cancelled fiber, and every trip is followed by a notifying
  `cancel_drain`), so it can never become a hang; genuine deadlock detection is untouched.
  `parallel_defer_runs_on_cancelled_sibling`: **14/200 failures → 0/200** under CPU contention (35/200 on a busier box);
  `parallel_defer_runs_when_enlisted_nursery_escapes` covers the escaped-enlisted-nursery seam; the
  invariants are pinned by `mnsched_cancelled_scope_with_parked_fibers_is_not_deadlock`,
  `panic_fault_trips_the_scope_cancel` and `poll_park_rejects_cancelled_inner_scope`.
  See `docs/contributing/gaps.md` **N4**. Two related holes found while verifying it, both pre-existing, both left
  open with their own entries: **N5** (a GENUINE deadlock also tears parked fibers down without
  `unwind_deferred`, so it skips `defer`s too — closed 2026-08-06 as **not a bug**: Go's deadlock
  `fatal error` skips its `defer`s too, and CPython just hangs) and **N6** (`--serial` does **not** run a PARKED task's `defer` on a sibling fault — it
  abandons the parked children at `run_child(i)?` — a real **serial ≠ M:N divergence**, uncovered by the
  parity suite, where the *oracle* is the wrong engine; fixing it moves serial's fault-path output
  ordering, so it is its own task).

- **R1 — the `bytes` native seam + binary IO/sockets; B1 now FIXED (2026-07-14).** `bytes`/`bytearray`
  shipped complete below the native seam (lexer/checker/VM/GC/airlock), but the seam itself was
  `str`-only, so **no native fn could accept or return them**. Widened in three places
  (`src/native/mod.rs`): `NativeRet::Bytes(Vec<u8>)` (lowered in `Vm::lower_native` to `Obj::Bytes` —
  the immutable form; a caller wanting mutation writes `bytearray(b)`), a defaulted-to-error
  `Host::arg_bytes` (implemented on `VmHost`; `bytes`-ONLY — a `bytearray` is not assignable to a
  `bytes` sink (7b29552: a mutable buffer aliased as immutable `bytes` is the hole that rule closes),
  so a caller converts with `bytes(ba)`, an explicit copy exactly like CPython's `bytes(ba)`),
  and `NativeArg::Bytes` + `OffloadHost::arg_bytes` so a **blocking** bytes native still offloads to
  the dirty pool instead of pinning a core worker (D5). No new type, no heap obj, no GC/airlock work.
  `value_to_native_ret` deliberately gets no bytes arm (it writes C's return register; the checker
  restricts callback returns to C scalars). Consumers wired: `io.read_bytes(path) -> Result[bytes]` /
  `io.write_bytes(path, data) -> Result[nil]` (binary whole-file IO — `read_file` decodes UTF-8 and so
  hard-failed on any binary file; it now says `use io.read_bytes for binary files`; the 64 MB
  `MAX_READ_FILE_BYTES` cap applies read-side only, as for `read_file`), `crypto.sha256_bytes(b)`
  (hash a file / a socket payload), `encoding.base64_encode_bytes` / `base64_decode_bytes` (the
  arbitrary-binary base64 round-trip), and `Socket.read_bytes(n[, timeout_ms]) -> Result[bytes]` /
  `Socket.write_bytes(b[, timeout_ms]) -> Result[int]` (`src/vm/netio.rs` — **B1's honest fix**: binary
  sockets work; `read_bytes` returns AT MOST `n` bytes and DRAINS the carry first, so the bytes the str
  `read`'s sticky `Err("invalid utf-8 …")` refused to deliver are recoverable instead of forcing a
  `close()`; the str `read` keeps its documented decode contract, unchanged). **Not done (out of
  scope):** `std.request` binary bodies (ureq `into_string()` needs its own reader plumbing → "download
  a file" stays blocked on *request*, not on R1), gzip/zlib, `NativeRet::ByteArray`, and any
  `Writer`/file-handle type (that is R2). Docs: `docs/reference/stdlib.md`, `docs/contributing/gaps.md` (R1 DONE, B1 FIXED,
  dependent bullets re-graded), `std/{io,net,crypto,encoding}.chz`.

- **`Socket.read` no longer CORRUPTS data — B1 MITIGATED (2026-07-14).** `src/vm/netio.rs` decoded every
  socket chunk with `String::from_utf8_lossy` at TWO sites (the fast path + the in-callback demote
  poller), so the `str`-only seam (`read -> Result[str]`, `std/net.chz`) silently produced U+FFFD: any
  **binary** payload became garbage, and — worse — **valid UTF-8 text** was mangled whenever a multibyte
  codepoint straddled a `read(n)` boundary, i.e. the ordinary read-in-a-loop idiom (`read(1)` over
  `"héllo"` → `h\u{fffd}\u{fffd}llo`). The runtime lied about the *data*. Both sites now route through
  ONE guard, `Vm::decode_carry`, which splits the two cases `Utf8Error` already distinguishes:
  **(a) truncated tail** (`error_len() == None`) — the ≤3-byte incomplete codepoint is RETAINED on the
  `Arc`'d `SocketCore` (`carry`, so it survives the would-block park's `ip`-rewind re-execution) and
  prepended to the next read, so byte-at-a-time reads of valid text reassemble **exactly**; **(b)
  genuinely invalid bytes** (`error_len() == Some(_)`) — a recoverable
  `Err("invalid utf-8 on the socket: …")` that is **non-destructive and sticky** — the valid text before
  the bad byte is delivered as a normal `Ok`, the undecodable bytes STAY carried, and every later read
  re-errs identically, so a log-and-continue caller cannot silently shred the stream (an `Err` that eats
  the chunk it already took off the fd is just silent data loss wearing an `Err`); it must `close()`. An
  incomplete codepoint left when the peer closes is `Err("invalid utf-8 at eof: …")`, never a silent drop.
  **Contract changes:** `n` bounds the NEW bytes off the fd, so a `read(n)` may return up to `n + 3`
  bytes; a chunk holding no complete codepoint re-reads rather than returning `Ok("")` (that is the EOF
  sentinel — returning it mid-`é` would silently truncate every `while chunk != "":` loop), so **a read
  blocks until it has at least one whole character** — the Go `bufio.Reader.ReadRune` / Python
  text-mode-socket contract, escapable via `timeout_ms` or the peer's close. `timeout_ms` bounds the WHOLE
  call on EVERY path and the carry is kept across the timeout `Err` (the bytes are still owed). `close()`
  stays `-> nil` — a leftover tail at close is dropped, the EOF error surfaces on the READ that sees the
  close. Review-caught corners of the retry loop, all fixed here: **(i) `read(0)`** (a caller-computed
  `read(want - have)` that lands on 0) is a **no-op `Ok("")`** — it never touches the fd, because a
  zero-length `Read::read` answers `Ok(0)` unconditionally and would spin the retry loop forever against a
  pending carry — but it is taken AFTER the closed-socket check, since the stream lock's `None` arm is the
  only closed-fd detector on the read path and `Ok("")` there is indistinguishable from EOF; **(ii) the fd
  read and the carry update are ONE critical section** (the `carry` lock is taken OUTER, `stream` inner) —
  split, two fibers sharing a socket (the `spawn handle(conn)` idiom) could decode out of wire order and
  error valid text as "invalid utf-8"; **(iii) the read's deadline is LATCHED on the fiber**
  (`Vm::poll_deadline`, swapped in `FiberCtx` like `poll_timed_out`) — a park rewinds `ip` and re-executes
  the op, so recomputing `now + timeout_ms` per park let a byte-per-(timeout-ε) dribbler keep a
  `read(n, ms)` alive forever — **and it is now threaded into `Vm::demote_block_socket`** (`sched.rs`),
  which took no deadline at all: an in-callback read (`native_reentry > 0` — a `list.map`, a
  `Shared.update`) waited on fd readiness forever, and a demoted op is accounted `inflight`, which VETOES
  the deadlock predicate — so it hung with no fault and no `Err("timeout")`; **`write`/`write_bytes` and
  `accept` now latch the same way (N2, 2026-07-15)** — extracted to `Vm::socket_write`/`Vm::listener_accept`
  and routed through `poll_deadline` + the shared `Vm::drop_poll_latch` clear, so they no longer recompute
  `now + timeout_ms` on a re-park (a robustness/consistency fix — `write` is architecturally single-park so
  the re-arm was only reachable on a spurious wake); **(iv)** a `read` that TOOK bytes but completed no
  codepoint returns `Err("incomplete utf-8: …")`, not `Err("timeout")` (which is documented as *nothing
  arrived*) — the bytes are off the wire and retained, and saying "deadline expired" about a read that
  consumed data is the same lie-about-your-data class B1 exists to kill. **This classification is now
  consistent across ALL timeout paths (N3a, 2026-07-15)** — poll-once, the netpoller park, and the
  in-callback demote — via a fiber-latched `Vm::poll_partial` (the twin of `poll_deadline`) consulted at
  both timeout sites through the shared `Vm::sock_incomplete_err`. Also: the decode's hot path (no carry, valid chunk) decodes the fd buffer BORROWED
  (`Cow`), so it costs one `String` alloc — exactly what `from_utf8_lossy(..).into_owned()` cost, no
  regression on the IO path. **Known v1 limit (unchanged): binary sockets are UNSUPPORTED** — they need
  `Socket.read_bytes`/`write_bytes`, which need the `bytes` native seam (`docs/contributing/gaps.md` R1, the honest
  fix). B1 is downgraded from *silent corruption* to *a clear, recoverable limitation*, not closed. Pinned
  by nine M:N tests (`net_read_reassembles_split_codepoint_over_parallel`,
  `net_read_invalid_utf8_errs_not_replacement_chars`, `net_read_incomplete_tail_at_eof_errs`,
  `net_read_zero_with_pending_carry_returns_empty_not_spin`, `net_read_zero_on_closed_socket_errs`,
  `net_read_shared_socket_two_fibers_decode_in_wire_order`,
  `net_read_poll_once_mid_codepoint_errs_incomplete_not_timeout`,
  `net_read_timeout_bounds_whole_call_across_codepoint_parks` (N3a: asserts `incomplete utf-8`),
  `net_read_timeout_bounds_the_in_callback_demote_path` (N3a: asserts `incomplete utf-8`),
  `net_read_partial_timeout_then_clean_timeout_is_not_incomplete` (N3a stale-latch clear guard),
  `net_write_timeout_when_buffer_full` (N2)); the serial engine's net path (immediate
  "requires the --parallel engine" `Err`) is untouched, so no parity divergence.
- **stdin is SHARED by every task (Go/Python) — the false EOF is dead (2026-07-14).** stdin belonged to
  the ENTRY task: every other task was handed `Stdin::Empty`, so `io.read_line()` / `io.input()` inside a
  `spawn:`/nursery task or an `Executor.submit` task returned `None` — a **false EOF**, while the entry
  task still had unread lines queued. That rule existed only to protect the byte-identical serial==M:N
  parity oracle (a shared consumable stream makes which-task-gets-which-line scheduling-dependent) — the
  same fake determinism the interactive-CLI milestone just removed from stdout. **The oracle bends; the
  language does not.** Now: ONE stdin source, shared by every task, exactly like Go's `os.Stdin` and
  Python's `sys.stdin` — any task may read it; a line goes to **exactly one** task (never duplicated,
  never dropped); **which** task gets a given line is **nondeterministic** on both engines (want
  deterministic distribution? entry task reads and fans out over a `Channel[str]` — the same "order it
  yourself" answer as concurrent `print`); `None` means genuinely exhausted (a real EOF still EOFs).
  Killed at **every** task-entry seam, because an invariant enforced at one seam is not enforced (that is
  how the Executor divergence survived five hunt waves): `FiberCtx::stdin` + its `swap_ctx` park DELETED
  (the `spawn:`/nursery fibers now read the one `Vm::host.stdin`), `spawn_worker` shares the handle
  instead of `Stdin::Empty` (the single M:N funnel — nursery + Executor pool drain), and the cooperative
  `Executor` inline drain's stdin park (65c2e42) is **reverted** — it made both engines agree on the
  WRONG semantics, and consistent-and-lying is still lying. Sharing is by **handle, not copy**:
  `Stdin::Lines` became an `Arc<Mutex<VecDeque<String>>>` (a by-value clone would hand every worker its
  own copy of every line — delivering each line N times, worse than the bug being fixed) and `Stdin::Real`
  stays a unit variant over the process-global, internally-locked `std::io::stdin()` (one `read_line` =
  one whole line, atomic across the M:N worker threads; never a per-worker `BufReader`, which would steal
  bytes into a private buffer and drop lines). `Stdin::Empty` survives only as a legitimate host config
  (an embedder with genuinely no stdin) — hence the golden/parity suite stays byte-identical green.
  Pinned by `parity_{spawned,executor}_tasks_share_stdin_exactly_once` (both task-entry families, both
  engines: 3 lines, 2 tasks + entry ⇒ the stdout line MULTISET is exactly the 3 lines, each once, no
  `eof` — asserted as a multiset, never an exact stdout, because the assignment is nondeterministic BY
  DESIGN and an `assert_eq!` here would be a flake built on purpose), `worker_shares_the_one_stdin_source`
  (a line the worker consumes is GONE for the parent — proves shared, not cloned) and the real-binary
  `task_reads_piped_stdin_{mn,serial}` (the `Stdin::Real` path, piped stdin, both engines). **New v1
  limit:** `read_line`/`input` are deliberately outside `is_blocking` (off-heap `OffloadHost::read_line`
  is `unreachable!`), so a task blocked in a read now **pins an M:N core worker** (K blocked readers ⇒ K
  pinned workers) — previously impossible, since tasks got an instant EOF. Accepted + documented
  (`docs/contributing/gaps.md`); offloading stdin reads is its own milestone.

- **Interactive CLI — `chezzi run` STREAMS stdout; the buffered sink stays for tests (2026-07-13).**
  `src/main.rs` used to capture the whole run into a `String` and `print!` it once, after the VM
  returned: a prompt never appeared before its `read_line`, a hung/killed program printed NOTHING, a
  long-running program was silent until exit, and a spawned task's log was invisible until its nursery
  joined (for a server: never). Now the stdout/stderr sink is selected by `HostConfig::stream` (default
  `false`): **every lib helper, golden and parity test keeps the BUFFERED sink unchanged** (per-task
  buffers + task-order flush → byte-identical serial-VM == M:N-VM; zero test edits), while `chezzi run`
  sets `stream = true` and each `print` becomes ONE `write_all` on the real stdout (line-atomic across
  tasks). In stream mode the per-task buffers just stay empty, so the scheduler is untouched.
  New `std.io` surface: **`flush()`** and **`input(prompt)`** (= print prompt, flush, read line →
  `Option[str]`). The write itself happens on a **background writer thread per stream** (`vm::stream`),
  which `write_all`s + `flush`es each message: a fiber must never sit in `write(2)` — an inline blocking
  write on a core worker means one stalled reader starves every other task (the D5 `is_blocking`
  invariant) — and the streamed handles are **unbuffered**, so a `print(x, end="")` progress marker
  appears (and survives a kill) with no `io.flush()`. Nothing in the VM ever *waits* on a writer thread
  (`flush`/`read_line`/`input` only queue), so a stalled consumer cannot pin a worker either; `flush()`
  is consequently a no-op that exists because it is the portable idiom. A writer thread **never calls
  `std::process::exit`** (this is library code; two threads racing libc `exit(3)` is UB, and a thread
  that kills the process discards the run's real outcome): it records, and the VM raises an ORDINARY
  runtime fault at its next `print` (`stdout closed (broken pipe)`). Policy — a closed stdout reader
  (`| head -1`) fails the run non-zero with a trace on the still-live stderr (Python raises
  `BrokenPipeError` identically), and an endless printer stops instead of spinning on a dead pipe. A
  program's **LAST** print into a just-closed pipe has no *next* print site to fault at, so the in-VM
  `stream_halt` never fired — the exit status used to be a nanosecond RACE (exit 0 if the VM outran the
  writer's EPIPE, dropping the bytes and reporting SUCCESS; 1 if the writer won). **Fixed (N1, 2026-07-15):**
  `cmd_run` re-checks `vm::out_dead_reason()` AFTER `flush_stream()` (which blocks on the writer ack, so
  `OUT_DEAD` is final) and fails deterministically — outranked by a non-broken-pipe `stream_error` and by
  `os.exit`, skipped when the VM already faulted (no double-report). Pinned by
  `last_print_into_closed_pipe_is_deterministically_nonzero_{mn,serial}` (+ `fully_drained_output_stays_success_*`
  for the no-regression clean case). The
  dead pipe must NOT borrow the `os.exit` channel to halt: that channel outranks a fault everywhere
  (`run_file_with_entry` discards the `Err` when `pending_exit` is set; `classify_mn_outcome` ranks
  `Exit` above `Fault`), so the first cut of this milestone turned a *faulting* run under `| head -1`
  into a silent **exit 0 with no trace** — a crashing program reporting SUCCESS to CI. Caught by the
  review panel, fixed before merge, pinned by `fault_under_broken_pipe_is_not_success_{mn,serial}`; any other stdout errno prints
  `chezzi run: cannot write stdout: …` and exits non-zero (a `> /dev/full` run can no longer report
  SUCCESS with no output); a **stderr** write failure is swallowed (diagnostic channel — a dead `2>`
  reader must not kill a healthy program). A spawned task's stdin was made `Empty` on **both** engines
  here (`swap_ctx` parks the entry task's stdin), closing a serial-vs-M:N divergence `read_line`/`input`
  inside a `spawn:` would otherwise have — **SUPERSEDED 2026-07-14** (see the shared-stdin entry above:
  the `Empty`-for-tasks rule was a false EOF; stdin is now one source shared by every task).
  The task-order flush is now documented as a **test-harness**
  property, not a user guarantee: cross-task print order is nondeterministic on both engines
  (Python/Go/Rust-identical); join and print the results yourself if you want order. Perf:
  `benches/run.chz` unmoved (all within noise); an ad-hoc 200k-line print loop goes **0.048 s →
  0.101 s** (~2.1×: one write syscall per line + the queue handoff — a `BufWriter` would hide the
  syscall but break "a killed program retains its output"). New real-binary suite `tests/interactive.rs`
  (34 tests, both engines: prompt-before-stdin, killed-program output, partial line visible with no
  flush, spawned-task print before join, order-insensitive concurrent lines, `from std.io import input`,
  broken-pipe clean exit, fault-after-broken-pipe still reported, last-print-into-closed-pipe
  deterministically non-zero (N1) + fully-drained clean run stays success, unwritable stdout reported,
  unwritable stderr does not kill the run, both pipes closed terminates, stalled reader does not starve
  — with or without `io.flush()`).

- **`regex.Match.start`/`.end` are now CODEPOINT offsets (observable stdlib behavior change).** They
  were the `regex` crate's raw **byte** offsets while Chezzi slicing/indexing is codepoint-based and
  there is no byte-indexed slice — so on non-ASCII input `s[m.start:m.end]` silently produced the
  wrong substring (`"héllo"` + `l+` → `"lo"`, want `"ll"`), with no fault. Converted at the single
  Match-construction seam (`src/native/regex.rs` `match_from_caps`), matching Python's `re`, which the
  module otherwise mirrors. New invariant, asserted in the tests: `subject[m.start:m.end] == m.text`.
  ASCII programs are bit-identical (byte == codepoint); a non-ASCII program doing arithmetic on
  `.start`/`.end` changes result — it was wrong before. Closes pre-JIT wave-5 audited residual #2.
  The conversion is **linear in the subject across a whole `find_all`** (one ascending byte→codepoint
  `CpCursor` over `captures_iter`'s monotonically increasing spans) — a from-zero prefix rescan per
  match would have been O(n·m), turning a document scan into a hang; pinned by a timing test
  (200k matches / 400k chars: 2.9s quadratic vs ~0.1s linear, debug).
- ✅ **A module bind that shadows a same-named USER type now NAMES the collision** (2026-07-13) — wave-5 audited residual 4, **downgraded from a soundness gap to a message fix**. `import lib.Point` + `struct Point` puts the module in the VALUE namespace, where it beats the ctor in expression position — but unlike the reserved-name case (which *silently destroyed* the builtin, hence the `is_reserved_module_bind` gate), a shadowed USER ctor is a **hard type error at the call**, so no program can run wrong, and `import lib.Point as pt` is the cure — i.e. ordinary Python-style shadowing, plus a diagnostic Python does not give. The only real defect was that the diagnostic was `module Point is not callable`, which never said where the ctor went. The not-callable arm (`src/checker/expr.rs`) gained a `Ty::Module` case gated on the bare-key type tables (`self.structs`/`self.enums`, so it is module-prefix-key correct): `module bind 'Point' shadows the same-named type 'Point' — alias the import: \`import ... as point\``. Checker-only, no compiler/VM change → parity- and perf-neutral. +1 test RED-first (`module_bind_shadowing_user_type_names_the_collision`): struct ctor + enum ctor collide, the alias escape hatch reaches BOTH the module and the ctor, and a module bind with no same-named type keeps the generic `is not callable` error (no over-rejection). A real **module namespace** (module names legal only in field position) remains the principled fix and is NOT planned — it is a resolver change that buys only the loss of an alias keystroke. Docs: `docs/contributing/gaps.md` (residual 4 downgraded).
- ✅ **Module/import seam: 4 fixes — `std.str` renamed to `std.string`, a reserved module bind is rejected, a nested `import` is a parse error, a from-imported global can't be rebound, and a qualified generic fn's turbofish/hint reach it** (2026-07-13, `auto-task/module-import-seam`) — front-end only (parser + checker + one std file rename); compiler/VM untouched, so both engines stay consistent by construction. **(1) A module bind silently shadowed a builtin ctor.** `import std.str` + `print(str(5))` → `type error: module str is not callable` — importing the documented std module DESTROYED the global `str()` ctor (the whole-module bind puts the last path segment into the VALUE namespace, where it beats the reserved builtin in expression position). Fixed in two parts: **(A)** the std string module is RENAMED `std.str` → `std.string` (`std/string.chz`; `str` was the ONLY std module name colliding with a reserved name — the scalar TYPE `str` and the `str()` ctor are UNCHANGED; matches Python/C++); **(B)** a module bind — aliased OR the un-aliased last path segment — whose name is reserved is now REJECTED (`module name 'int' is reserved (builtin) — alias it: import lib.int as ints` / `import alias 'Ok' is reserved (builtin)`). The gate is `is_reserved_module_bind` = the existing `is_reserved_alias_target` (reserved callables + reserved type names) + `nil` (carved out of the ALIAS gate because an alias binds a VALUE and a value still works as a value — but a MODULE is not a value, so `import lib.nil` would silently retype the `nil` literal) + the extracted `is_builtin_variant` (`Ok`/`Err`/`Some`/`None`, which `is_reserved_type` deliberately excludes so decl-site shadowing keeps working) — 34 names, no new list. Escape hatch: `import lib.int as ints`. The gate is ROOT-SWEPT to every bind that lands in the VALUE namespace, so the `from` form is covered too — a reserved ALIAS (`import x as Ok from M`) *and* a reserved bare MEMBER (`import str from lib.sh`, where a module global/fn named `str` destroyed the `str()` ctor exactly like `import std.str` did) are both rejected (`imported name 'str' is reserved (builtin) — alias it: …`); a reserved TYPE member binds no value and is the LICENSING import of the builtin itself (`import Shared from std.concurrency`, `import ptr from std.ffi`), so it stays legal un-aliased. RESIDUAL (out of scope): a module bind colliding with a USER struct/enum ctor of the same name still wins in expression position. **(2) A nested `import` was a LYING ACCEPT** — an `import` in a fn body/block parsed, checked clean and was a complete NO-OP (even for a module that does not exist): the resolver only scans module-level stmts. It is now a PARSE error (`import must be a top-level declaration`), the third instance of the existing `extern`/`native` depth>1 gate; `<importStmt>` moved from `<simpleStmt>` to `<topLevelStmt>` in `docs/reference/grammar.bnf` + `tests/corpus/reject/import_nested.chz`. **(3) Rebinding a from-imported global was silently accepted** — `import COUNT from lib.st` + `COUNT = 99` wrote a local alias, lost forever, while the qualified `st.COUNT = 5` correctly errored. The SNAPSHOT semantics are correct (verified CPython-identical) and unchanged; only the WRITE path is now rejected (`cannot assign to 'COUNT' imported from module 'lib.st' …`), gated on the name resolving at module scope so a fn-local `:=` shadow stays assignable — and RE-DECLARING the name at module scope (`COUNT := COUNT + 1`) hands it back to this module (`declare` drops the import entry, like it drops a loop-var mark), so the module's own binding stays assignable — and only for VALUE binds — mutating THROUGH a from-imported container (`LST.push(7)`) is a different arm and keeps working. **(4) A module-qualified generic fn whose type param appears only in the return type was unreachable** — `geo.empty_list[int]()` said *"method 'empty_list' takes no type argument(s) (it declares no own type parameters)"* (it plainly does) and the escape hatch `xs: List[int] = geo.empty_list()` leaked an unsolved `List[T]`. `method_has_own_type_params` gained a `Ty::Module` arm, and the module-fn arm of `infer_method_call` now threads BOTH the turbofish and the expected-type hint into `infer_generic_call` (fixing only the first would silently IGNORE the turbofish). Multi-arg qualified TYPE turbofish stays a documented clean parse error. Tests RED-first: parser (nested import) + conformance reject corpus + checker `files_ok`/`files_reject` over a REAL module graph (reserved bind/alias + escape hatch, rebind vs mutate-through vs local shadow, qualified generic turbofish + hint + arity boundary) + two-engine parity runs (`import std.string` with `str(5)` in the same module; `geo.empty_list[int]()`). Docs: `docs/reference/syntax.md` §12 (import is top-level-only, reserved module-bind rule, `from M import X` snapshot semantics, qualified generic-fn turbofish), `docs/reference/grammar.bnf`, `docs/reference/stdlib.md` / `docs/reference/spec.md` / `docs/design/ffi-and-packaging.md` / `gaps.md` (the `std.str` → `std.string` rename; older dated changelog entries below keep the historical `std.str`/`std/str.chz` spelling).
- ✅ **A range in a value position is now a check-time TYPE ERROR (was: check-OK, then the program could not run at all)** (2026-07-13, `auto-task/range-not-a-value`) — `x := 0..3` type-checked `ok: no type errors` and then died on both engines with `runtime error: a range can only be used as the iterable of a \`for\` loop`, printing **nothing at all** (not even a preceding `print("before")`): a COMPILE error surfaced at run time, so `chezzi check` and the LSP were blind to a whole class of programs that cannot execute. Root cause: `infer_kind`'s `ExprKind::Range` arm (`src/checker/pattern.rs`) laundered `a..b` as **`List[int]`**, so every list operation on it type-checked — the checker's own `y: str = 0..3` diagnostic (*"cannot assign **List[int]** to variable of type str"*) named the bug. The compiler, which is type-blind, then rejected the range everywhere except the 3 positions it can actually lower. **Fix (checker-only, ONE arm → zero compiler/VM/codegen change, so parity- and perf-neutral):** every value position (assign RHS, call arg, collection element, binary operand, method receiver, plain index object, return, generic `Iterator[T]` bound arg, interpolation, pipe) funnels through `infer` → `infer_kind`, so that single arm now reports `a range is only valid as the iterable of a \`for\` loop or comprehension, as a slice receiver, or as a \`match\` pattern — use \`range(a, b)\` to materialize a \`List[int]\`` and yields `Ty::Unknown` (not `List[int]`) to suppress a misleading cascade. **No `Ty::Range` variant:** a range has no runtime value in any engine, so a new `Ty` would ripple through unify/compatible/assignable/Display/sendable/hover and buy nothing — the honest model is "a syntactic form legal in exactly 3 expression positions", which is already what the code assumed. The sanctioned positions never reach `infer_kind` and so keep working untouched: `for_bindings` (`src/checker/sig.rs`) pattern-matches `ExprKind::Range` **syntactically** for BOTH iterable forms (so `for i in 0..10`, `0..xs.len()`, expression bounds, and comprehensions are free, and **laziness is preserved** — `for i in 0..100000000000` still never materializes), and `case 1..5:` is a `Pattern::Range`, a different AST node entirely. Two deliberate bypasses were needed: `infer_slice` special-cases a range receiver → `List[int]` (mirroring the compiler's materialize-then-slice, keeping `(0..10)[::2]` / `(0..5)[::-1]` alive), and `infer_comprehension`'s Channel-drain guard now skips a `Range` clause iter (it re-infers `clause.iter` a second time AFTER `for_bindings`, which would otherwise have rejected `[i for i in 0..3]` — the likeliest over-rejection). Also DELETES the piecemeal prior-art guard in `infer_binary`'s `In` arm (a range RHS is now rejected by the generic arm, so keeping it would DOUBLE-report — pinned by an exactly-one-error assertion). **`List(0..3)` / `Set(0..3)` are deliberately REJECTED, not made to work:** the materialization escape hatch ALREADY EXISTS and is documented — the `range(start, end[, step])` builtin returns a real `List[int]`, so `Set(range(0, 3))` works today; range-aware ctors would be pure surface duplication (a checker whitelist + a compiler special-case + grammar/docs churn) for zero new capability. The hint therefore points at something TRUE TODAY with **zero surface addition**. **INVARIANT (the acceptance bar):** the checker's accepted `Range` set is now exactly {for-iterable, comprehension-clause iterable, slice receiver} — the only three sites that reach a `Range` without `infer_kind` — and that is exactly the compiler's lowered set, so **no check-clean program can reach the compiler's error**. Note `(0..10)[2]` (a plain INDEX, not a slice) was a third instance of the bug nobody had listed; the single arm rejects it for free, and no Index bypass was added (one would make the compiler error reachable again). The compiler's `CompileError` (`src/compiler/mod.rs`) STAYS as a defensive backstop — unreachable from a check-clean program, but the compiler is also driven WITHOUT the checker (synthesized ASTs from difftest/panicfuzz; the `run_capture` / `run_capture_parallel` VM test helpers skip type-checking) — and now carries a comment saying exactly that. **Blast radius: ONE in-repo `.chz`** — `tests/corpus/accept/ranges.chz` (`r := 0..10`, a parse-only corpus that never runs the checker, but it encoded a now-illegal program) rewritten to `for i in 0..10:`, still covering `# rule: rangeExpr`; ZERO changes in `std/*.chz`, `examples/*.chz`, `benches/chz/*.chz`, `judge/` (a corpus grep confirms the only non-`for` range uses are the slices in `examples/range_step.chz` and the comprehensions in `examples/comprehensions*.chz`, all still legal), and `docs/reference/grammar.bnf` needs no edit — `x := 0..3` still PARSES, the rejection is a TYPE error. +2 checker tests RED-first via `entry_rejects`/`entry_ok` (the real `build_graph` + `check_graph` path): 14 escape forms (incl. the repro, `List`/`Set` ctors, `.len()`, `.push()`, `+ [7]`, `== [0,1,2]` — which pins that the permissive `Eq | NotEq` arm still INFERS its operands, the one place the invariant could have leaked — a list/map literal element, a `List[int]` param, an `[S: Iterator[T], T]` bound, plain index, `in`, and the no-longer-laundered `y: str = 0..3`) and 15 must-keep-working boundary positions; + 2 two-engine RUN guards in `src/vm/parity_tests.rs` pinning the COMPILER half (all sanctioned lowerings still execute byte-identically, and `range(0, 3)` → `[0, 1, 2]`, i.e. the hint is true) and the backstop. Docs: `docs/reference/syntax.md` (the range section — a range is not a value + the `range(a, b)` escape hatch; the slice section), `docs/reference/spec.md` (M15 row — the `..`-stays-a-for/match-range design is now actually ENFORCED).
- ✅ **`os.exit(<negative>)` no longer reports SUCCESS to the shell/CI** (2026-07-13, `auto-task/three-runtime-fixes`) — `os.exit(-1)` — the canonical failure idiom — exited the process with status **0** on both engines: `VmHost::request_exit` (`src/vm/mod.rs`) did `code.clamp(0, 255)`, clamping every negative code UP to 0, so a deliberate failure exit was invisible to a shell/CI `$?` check (the real severity: a silent green build). **Fix (one line):** the status is now the POSIX **low 8 bits** of the code (`code & 0xff`) — exactly what `exit(3)` / bash / Python / Go all do: `-1` → **255**, `-2` → 254, `0` → 0, `1` → 1, `255` → 255, `-256` → 0. It is a *mask*, not a clamp, so this deliberately also changes **`os.exit(300)` → 44** (was 255; POSIX `exit(300)` is 44), making the rule total and one-sentence documentable, and idempotent with the CLI's existing `ExitCode::from(code as u8)`. No in-repo `.chz` is affected (`examples/exit.chz` = 2, `examples/parallel_cancel.chz` = 7 — all small positives, bit-identical). +2 tests RED-first in a NEW `tests/exit_status.rs` asserting the REAL PROCESS EXIT STATUS via `Command` on the built binary across `{run, run --serial}` (an in-VM assertion on `pending_exit` could not have caught this bug at all) + an in-VM `vm::parity_tests::exit_negative_code_masks_to_255` pinning the code on both engines through the cross-thread re-store. Docs: `docs/reference/stdlib.md` (the `exit` row), `docs/design/roadmap.md`.
- ✅ **A re-entrant `.next()` on a currently-RUNNING generator faults instead of silently reporting exhaustion** (2026-07-13, `auto-task/three-runtime-fixes`) — resuming a generator from inside its own body (`holder[0].next()` where `holder[0]` is the generator now executing) answered **`None`** on both engines: a live, non-exhausted generator reporting itself EXHAUSTED — a silently-wrong `Option`, not a fault. Root cause: `generator_next` (`src/vm/exec.rs`) parks `GenState::Done` in the heap object as the placeholder while the generator runs (`mem::replace`), so the re-entrant call hit the `Done` short-circuit and got `None`. **Fix (one guard, 3 lines):** at the TOP of `generator_next` — before the heap borrow and before the state take — `if self.active_generators.contains(&h) { return Err(self.err("generator already running", span)) }`, reusing the resume path's OWN root list (pushed/popped around the run) rather than adding a `GenState::Running` variant that would need explicit write-back on every early-return path. Because the pop happens on **every** unwind path, the guard is **self-clearing** — a generator can never be poisoned as permanently "running" (pinned by a test per path: normal exhaustion, an early consumer `break` then a legitimate resume, a body that faults and is caught by `recover:` then a fresh generator still runs, and a generator driving a *different* generator — no over-rejection). The fault is an ordinary `RuntimeError`: catchable by `recover:`, never a host panic, byte-identical on both engines (Python: `ValueError: generator already executing`). A generator whose body faulted stays **closed**, like Python's (later `.next()` → `None`) — unchanged, now documented. +3 two-engine tests RED-first (the repro printed `1 / reentrant: None / 2` before the fix). Docs: `docs/reference/stdlib.md` ("Iterator cursors & generators" re-entrancy rule), `docs/reference/spec.md` (beside the existing sendability caveat).
- ✅ **`iter.reduce` on an empty list gives a named fault instead of leaking the std module's internal index error** (2026-07-13, `auto-task/three-runtime-fixes`) — `iter.reduce([], f)` faulted with `runtime error (line 72, col 12): index 0 out of bounds (len 0)` — a std-module implementation detail (a line number *inside* `std/iter.chz`, at an index the user never wrote) surfacing as the user-facing diagnostic. **Fix (`std/iter.chz`, pure Chezzi — it must stay Chezzi for `--parallel` callback parking, D5):** a leading `if xs.len() == 0: panic("reduce: empty list with no initial value")` guard (the existing std fault idiom — cf. `std/str.chz`'s `pad_left: fill must not be empty`), matching Python's `TypeError: reduce() of empty iterable with no initial value`. Still a recoverable fault, just an honest message. **Sibling audit of the same leaked-internal-error-on-empty class** (per the fix-at-the-root rule): `reduce` was the ONLY unguarded `[0]`-class index — `str.reverse` and `iter.shuffle` compute `len() - 1` but their `while` guards make `-1` a no-op on empty; `path.basename` gets a 1-element list from `split`; every `collections.chz` index (`min_heap` pop/peek, `deque` peek_front/back) is already `len`-guarded. Nothing else to fix. +2 two-engine tests RED-first (named-message + `recover:`-catchable, with a non-empty `[1,2,3]`→`6` / single-element `[7]`→`7` boundary). Docs: `docs/reference/stdlib.md` (`std.iter` `reduce` entry).
- ✅ **Two check-OK → runtime-fault holes closed: a bound method is no longer readable as a value, and `index`/`set_index` must now be V-coherent** (2026-07-13, `auto-task/checker-method-value-index-coherence`) — both were checker-only accepts of programs the runtime cannot execute; both fixes are checker-only rejects (no compiler/VM change → parity- and perf-neutral). **(1) Bound-method value.** `g := s.get` on a `struct S` with `fn get(self) -> int` type-checked, then faulted on both engines with `no field 'get' on S(n=5)`. Root cause: `infer_field`'s `Ty::Struct` arm (`src/checker/pattern.rs`) fell back to the METHOD table after the data-field lookup missed and handed back a `Ty::Func` still carrying the **un-bound `self` slot typed `Ty::Unknown`** — nothing lowers that (the compiler emits a plain field load), and the `?` self slot **laundered types**: `g("anything")`, `apply(s.get, s)` against a `fn(S) -> int` param, `xs := [s.get, s.get]`, and `g := self.get` inside a method body ALL checked OK and then faulted. The four sibling receiver kinds (enum / newtype / protocol existential / the assign path) already rejected correctly, which pinned the seam exactly. Methods are **not first-class values** (nowhere sanctioned in the docs — an accidental accept, not a feature): the fallback is deleted, so control falls through to the sibling error, with a method-aware hint — `type S has no field 'get' ('get' is a method — methods are not values: call it (\`x.get(…)\`) or wrap it (\`fn(): x.get()\`))`. The misleading follow-on arity message (`'closure' expects 1 argument(s), got 0`) disappears for free: the expr is now `Ty::Unknown` and `infer_call`'s Unknown arm stays silent — exactly ONE diagnostic (asserted). Fields are searched BEFORE methods in the same arm, so the closest neighbour — a genuinely **fn-TYPED field** (`f: fn(int) -> int`, `s.f(3)` AND `g := s.f; g(3)`) — still works, as do method calls, chained calls, calls on a nested field, a field whose name collides with a method, and static/associated CALLS (a bare `S.make` VALUE is not legal today anyway — `unknown name 'S'`). **(2) `index`/`set_index` V-incoherence on a COMPOUND index-assign.** A struct with `index(self, int) -> str` but `set_index(self, int, val: int)` accepted `s[0] += 1`, then faulted with `cannot apply Add to str and int`. `x OP= v` is defined as exactly `x = x OP v` (`docs/reference/syntax.md` §3) — and the compiler lowers it that way (`Dup2 → GetIndex → op → SetIndex`) — so a compound index-assign's LHS must be typed from **`index`'s RETURN**; the direct index-assign path took it from `set_index`'s `val` instead. Fix (`src/checker/sig.rs`, the one `other =>` arm of `check_assign`): a compound now types its LHS through the existing `index_kv` (the READ side) and then requires that value to fit `set_index`'s `val` (and the keys to agree) — the same `IndexSet[K, V]` coherence the bounded `[C: IndexSet[K, V]]` path already enforces — reporting exactly ONE error, `type S does not satisfy IndexSet (index returns str but set_index's val is int)`, with no `cannot apply +=` cascade. A **plain** `s[k] = v` is deliberately NOT gated: it never reads through `index`, so an asymmetric pair (a safe-read `index -> V?`, a widening writer) is sound and keeps type-checking and running exactly as before — `index_set_kv` still returns the `set_index`-derived write slot, so the write side loses no checking (an un-pinned generic receiver still rejects `b[0] = ["oops"]` against a `List[int]` val). Behaviour preserved: coherent user `Index`/`IndexSet` pairs (read / write / compound / negative index / all open+step slice bounds), `index`-only (read-only) and `set_index`-only structs, builtin targets (`m[k] += 1`, `xs[i] += 1`, `obj.f += 1`), and the once-only evaluation of a side-effecting index expr (Python parity). **Blast radius: ZERO in-repo `.chz`** — no example/std/bench/test reads a bound method or compound-assigns an incoherent pair (`examples/slicing.chz`'s `Ring` and the parity `BUF_PROG` are coherent). +11 checker tests RED-first via `check_entry`/`entry_ok`/`entry_rejects` (the real `build_graph` + `check_graph` path) incl. every launder variant, the exactly-one-error assertions and the no-over-rejection guards, + 4 two-engine RUN guards in `src/vm/parity_tests.rs` (coherent `str`-valued IndexSet, asymmetric plain write, index-expr-evaluated-exactly-once, fn-typed-field-as-value). Docs: `docs/reference/syntax.md` (§3 compound-assign ≡ `x = x OP v` + the read-side coherence rule + once-only index eval; §7 methods are not first-class values — call or wrap; §7b `IndexSet` compound-read coherence).
- ✅ **`pad_left` with an empty `fill` no longer LIVELOCKS; a multi-char `fill` no longer overshoots `width`** (2026-07-13, `auto-task/pad-left-empty-fill`) — `"a".pad_left(5, "")` type-checked `ok` and then SPUN FOREVER on both engines with zero output and no diagnostic (stdout is buffered to process exit, so the user saw literally nothing). Root cause: two independent copies of the same unbounded prepend loop — the native method (`src/vm/call.rs`) and the pure-Chezzi free fn (`std/str.chz`) — grew the string until it reached `width`, which an empty `fill` never does. Both now raise the recoverable fault `pad_left: fill must not be empty` **eagerly**, before the `width <= len` early-out, so the diagnostic can't depend on whether padding was even needed (`"".pad_left(0, "")` faults too — deliberate, pinned by a test). Same commit fixes the sibling overshoot bug: a multi-char `fill` is now a repeating cycle **truncated to fit**, so the result is exactly `width` codepoints (`"a".pad_left(4, "xy")` → `"xyxa"`, was the 5-char `"xyxya"`). The never-shrinks early-out (`width <= len` → `s` unchanged) is taken BEFORE the `need = width - len` subtraction in BOTH copies, so a very negative `width` — down to `i64::MIN`, reachable from safe source — can neither overflow i64 (debug host panic / release wrap into a colossal `take`) nor raise Chezzi's checked-`-` `integer overflow in Sub` in the free fn, counting is by codepoint throughout (`chars()`, never byte length or byte slicing, so a non-ASCII fill counts as 1 and no mid-char slice can panic), and the native pad allocation routes through `repeat`'s existing capacity guard (`checked_mul` + `isize::MAX` + `try_reserve_exact`) → a recoverable `string pad capacity overflow` instead of an OOM/abort for a huge `width`. The O(n²) `format!`-in-a-loop is gone (one allocation). **Documented divergence:** that capacity guard has no counterpart in the `.chz` free fn, which cannot probe the allocator — the second sanctioned exception to the byte-identical-alias contract, written down at `docs/reference/stdlib.md`. +6 tests RED-first (the empty-fill test HUNG for the full 900s timeout before the fix, it did not merely fail): 4 native in `src/vm/tests.rs` (empty-fill fault incl. a `recover:`-catch, exactly-`width` multi-char, codepoints-not-bytes, huge-width capacity fault) + 2 free-fn in `src/vm/parity_tests.rs` via the module-graph runner, all on both engines; `examples/str_methods.chz` gained the multi-char/non-ASCII/negative-width/no-shrink golden lines. In-repo callers (`std/datetime.chz`, `examples/std_demo.chz` — all single-char fill, `width >= len`) are bit-identical. Docs: `docs/reference/stdlib.md` (`pad_left` row + the alias carve-out). `str.pad_right`/`center` remain unimplemented (`docs/contributing/gaps.md`) — untouched.
- ✅ **`return` inside a `defer:` / `spawn:` block is now a check-time error (was: type-checked, then silently discarded at runtime)** (2026-07-13, `auto-task/reject-return-defer-spawn`) — `fn f() -> int!: defer: return Err("hijack")` / `spawn: return 7` checked OK (the return was even validated against the enclosing fn's return type) and then the value was dropped at runtime on both engines — the worst of both worlds. Chezzi has no named return values: a `defer:` block is its own closure and a spawned task outlives the frame, so such a `return` can never mean anything. **Fix (checker-only, zero runtime/codegen change → parity- and perf-neutral):** reuse the escaping-flow guard `recover:` already uses — renamed `recover_escaping_flow` → `escaping_flow` (`src/checker/mod.rs`) and called it at the `Defer(DeferTarget::Block)` + `SpawnTarget::Block` arms (`src/checker/sig.rs`) with `in_loop = true` (both arms already zero `loop_depth`, so `break`/`continue` keep their own "break outside loop" message — no double diagnostic). Messages: `'return' is not allowed inside a defer block` / `… spawn block` — one diagnostic, naming the block the `return` is LEXICALLY in: the walker does NOT descend into a nested `defer:`/`spawn:` block (each is guarded at its own site), so `defer: > parallel: > spawn: > return` reports "spawn block" once (it used to double-report, and `recover:` + `spawn: return` regressed the same way). The walker DOES descend into `wait:` arms + their `else` (a `return` there was the same silently-discarded bug; this tightens `recover:` identically). Still legal (boundary-tested): a `return` inside a nested `fn` DECLARED in either block, `return` in a `parallel:` body, `?`-in-`defer:` (still short-circuits + discards), and every defer/spawn/parallel nesting combination. (A closure body is a single expression, so it can't hold a `return` at all.) +14 checker tests (RED-first via `entry_rejects` / error-count assertions, incl. `wait:`-arm + `wait:`-`else` + nested-block-noun) + a two-engine `vm::defer_spawn_nesting_matrix_parity` regression net. Docs: `docs/reference/syntax.md` (defer block form + spawn nursery; also fixed a stale `# prints "x = 1" — snapshotted here` comment → by-reference, reads `2` at exit).
- ✅ **L3-1: a pure-panic inline body (`fn boom(): panic("x")`) now infers `-> nil`** (2026-07-12, `auto-task/inline-panic-infers-nil`) — a function whose SOLE body is a diverging call was spuriously rejected with "cannot infer return type of '<name>'; add a -> annotation", even though a void body (`fn v(): print(...)`) infers `-> nil` and an annotated `fn b() -> int: panic(...)` type-checks (bottom fits any return). Root cause: `panic(...)` is bottom-typed `Ty::Unknown` (`expr.rs`), so the inline-expr body path (`src/checker/sig.rs:390`) set `inline_ret = Some(Unknown)`, which the finalizer rejected as residual un-inferable. Fix (checker-only, ~one branch): bind `let t = self.infer(e)` and substitute `Ty::Nil` iff `t.is_unknown() && Self::expr_is_diverging_call(e)` (the existing narrow bare-`panic`/`exit`/`os.exit` predicate) — so the caller can't use a value anyway, `nil` is the natural default. `self.infer(e)` still runs so panic's arg checks fire in pass 2. Genuine un-inferable cases (Err-only/None-only/`[]`/pure recursion — all block-body `return` forms) still rejected untouched. +2 tests RED-first: `checker::inline_diverging_body_infers_nil` (infers nil; `panic(123)` still rejected) + two-engine `vm::inline_panic_body_faults_both_engines` (caller prints "start", faults "x", exit 1; `recover:`→`Err("x")`). CLI-verified on `run` + `run --serial`. Docs: `docs/reference/syntax.md`.
- ✅ **Deadlock fault message is now engine-agnostic + manifest entrypoint rejects `/` path separators** (2026-07-12, `auto-task/low-fixes-deadlock-entrypoint`) — two small independent fixes. (1) A blocking `recv`/`for v in ch:`/`wait:` with no producer and no runnable task deadlocks on BOTH engines, but the message hardcoded "sequential executor cannot block waiting for a producer" — misleading under the default M:N engine (one code path serves both). Reworded the two `self.err(...)` sites (`src/vm/netio.rs:934`, `:1109`) to engine-agnostic "deadlock — nothing is queued and no task can ever send" (recv keeps its C5 mid-flight hint). Detection/exit/`recover:` catchability unchanged (text-independent). (2) `entrypoint_file` (`src/main.rs`) split the module path only on `.`, so a manifest `entrypoint = "src/main:main"` (slash instead of dotted) resolved by accident via `PathBuf::push`; added a guard rejecting embedded `/`/`\\` with "the module path must use '.' separators, not '/'". Valid dotted forms unaffected. +3 tests RED-first (two-engine `deadlock_fault_message_is_engine_agnostic` + `deadlock_fault_is_recoverable_new_message` via `parity_entry_fault`; extended `entrypoint_file_validates_dotted_path`). Docs: `docs/history/designs/cross-nursery-flat-scheduler.md`, `docs/reference/concurrency.md` (paraphrased fault quotes updated). Text-only + guard-only; no engine-behavior change.
- ✅ **`i64::MIN` decimal literal `-9223372036854775808` now lexes/parses (was "number too large")** (2026-07-11, `auto-task/three-small-fixes`) — the boundary value i64::MIN was unwritable as a literal: the lexer read the positive magnitude `9223372036854775808` (== i64::MAX+1 == 2^63, which overflows i64) BEFORE the parser applied the unary minus, so it lex-errored "number too large to fit in target type"; users had to write `-9223372036854775807 - 1`. **Fix (lexer + parser, no checker/VM change):** the lexer's decimal-int branch (`src/lexer/mod.rs`), on an i64 parse overflow, now emits a distinct nullary `Token::IntMinMagnitude` iff the magnitude is EXACTLY 2^63 (via a `u64` reparse), else propagates the original error — so `9223372036854775809`+ still lex-errors. `parse_unary` (`src/parser/mod.rs`) folds `Neg` + `IntMinMagnitude` straight into `ExprKind::Int(i64::MIN)` (NOT `Unary(Neg,…)` — runtime-negating the magnitude would overflow); `expect_pattern_int` mirrors the fold for match/range patterns (`-9223372036854775808:` works). A BARE (un-negated) `9223372036854775808` still errors "number too large" (via new arms in `parse_primary`/`expect_int`), so the value can never leak as a positive int. `IntMinMagnitude` maps to the `INT` grammar terminal in `conformance.rs::symbol()` (no grammar.bnf drift — `-INT` already covered). Radix forms (`0x8000000000000000` etc.) are a SEPARATE lexer branch and still error at i64::MIN — deliberately out of this minimal scope (known v1 limit). +3 tests RED-first (`lexer::lexes_i64_min_magnitude`, `parser::i64_min_literal_folds_to_int_min`, two-engine `vm::parity_tests::i64_min_literal_runs_parity` covering print + arithmetic + `match`), CLI-verified `-9223372036854775808` prints correctly on `run` + `run --serial`, bare 2^63 and `-9223372036854775809` still error. Docs: `docs/reference/syntax.md` (literal examples).
- ✅ **`json.parse` rejects non-finite/overflowing numbers at decode (Go `encoding/json` policy)** (2026-07-11, `auto-task/three-small-fixes`) — `json.parse("1e400")` returned `Ok(Json.Num(+inf))`, manufacturing a value its OWN `json.stringify` then FAULTED on ("cannot serialize non-finite float to JSON") — parse produced something that could not round-trip, violating the documented invariant that parse "never emits [a value] that Chezzi's own `parse` would reject." (`decode[int]("1e400")` already returned a clean `Err`; only the raw `Json` parse layer was the outlier.) **Fix (one guard, pure Chezzi, `std/json.chz` `parse_number`):** after `f := float(raw)`, `if f - f != 0.0: return Err("invalid number: value out of range")` (the same finite-check idiom `num_str` uses — `f - f` is `0.0` for finite `f`, `NaN` for `±inf`/`NaN`), matching Go's reject-at-decode. Underflow to `0.0` (`1e-400`) is finite and stays `Ok`. +1 two-engine parity test (`json_parse_rejects_non_finite_parity`: `1e400`/`-1e400`/`[1e400]`→Err, `1.5`/`123`/`1e-400`→Ok); the existing `json_as_int_out_of_range_parity`'s `1e400` probe flips `NONE`→`PARSEERR` (updated same commit). CLI-verified on `run` + `run --serial`. Docs: `docs/reference/stdlib.md` json section (parse-rejects-overflow, symmetric with the stringify fault).
- ✅ **`str.split` empty-input invariant locked (regression guard)** (2026-07-11, `auto-task/three-small-fixes`) — investigated a reported `"".split(",") == []` bug; it does NOT exist. `"".split(",")` already correctly returns a one-element list holding `""` (length 1, `x[0] == ""`) via Rust's `str::split` at `src/vm/call.rs`, honoring `pieces == separators + 1` on both engines. The report misread the list's DEBUG RENDERING: a single empty string renders as `[]` because `""` prints as nothing (same as `",".split(",")` → `[, ]`). No production change; added a regression test (`str_empty_split_returns_single_empty_element_parity`) asserting length + element rather than rendering. Docs: `docs/reference/stdlib.md` split entry now states the empty-input result explicitly.
- ✅ **`Self` usable in inherent struct/enum/newtype method signatures + bodies (checker-only)** (2026-07-11, `auto-task/self-and-compound-overload`) — `Self` resolved inside a `protocol` method sig but was rejected (`unknown type 'Self'`) in a plain `struct`/`enum`/`newtype` inherent method — an arbitrary asymmetry (Rust allows `Self` in inherent impls, not just traits). Fix: a new checker field `current_self_ty: Option<Ty>` set (via `mem::replace`) at the method-sig hoist sites (struct/enum/newtype in `setup.rs`) and at `infer_fn_ret`/`check_fn_body` entry from their `self_ty` arg, plus ONE `resolve_type` arm (`"Self" if self.current_self_ty.is_some()`) placed AFTER the type-param arm — so a PROTOCOL method's `Self` (already in `type_params` as `Ty::Param("Self")`, `current_self_ty` left `None`) keeps its existential-param binding UNCHANGED, and `Self` outside any method stays `unknown type 'Self'`. Resolves to the CONCRETE enclosing `Ty::Struct/Enum/NewType(key, [own type params])`, so `-> Self` enforces the real type (returning a different type is still a type error) and a generic `Box[T]`'s `-> Self` carries `[Param(T)]` matching `return self`. Newtype included (deliberate consistency: it routes through the same `infer_fn_ret`/`check_fn_body` funnel). Checker-only — runtime already dispatches these methods; both engines byte-identical. +7 checker tests (RED-first, `entry_ok`/`entry_rejects` module-graph path) + golden `examples/self_method.chz` (struct/enum/newtype/generic, two-engine parity). Docs: `docs/reference/syntax.md` (Self in inherent methods).
- ✅ **Compound assignment honors struct/enum/newtype operator overloading (checker-only)** (2026-07-11, `auto-task/self-and-compound-overload`) — `x OP= v` is documented as exactly `x = x OP v`, but `check_assign_value` (`sig.rs`) whitelisted only str+str / non-widening numeric / three collection forms, so `a += V(10)` was rejected (`cannot apply += to V and V`) for a `struct V` with an `add` overload even though `a = a + V(10)` type-checked. Fix: the arith-compound arm now also computes `overload_ok = op_overload_result(target, val, proto).is_some_and(|res| assignable(target, &res))` — reusing the SAME `op_overload_result` the binary-operator checker (`infer_binary`) consults (`Add`/`Sub`/`Mul`/`Div`/`Mod` from the op) — and accepts when it holds. Because `op_overload_result` returns `Some` only for same-typed operands satisfying the operator protocol (or a same numeric-newtype auto-flow), a no-overload struct / `V += int` / `Box[int] += Box[str]` all stay rejected — no blanket compound-assign acceptance; existing str/numeric/collection forms unchanged. Runtime already lowers `a OP= b` through the same `Op::Add`/… opcodes as `a = a OP b`, so parity is automatic (verified by running both engines). +6 checker tests (RED-first) + golden `examples/compound_overload.chz` (struct `+=` == explicit form, numeric newtype `+=`, two-engine parity). Docs: `docs/reference/syntax.md` (compound-assignment overload note).
- ✅ **Named-factory-import member resolution fixed (checker-only) (gap #4)** (2026-07-08, `auto-task/named-factory-import-member-resolution`) — a NAMED import of a factory *function* (not its return type) — `import make from lib; w := make()` — left the returned struct/enum/newtype's FIELDS and METHODS unresolvable (`type Widget has no method 'bump'`), even though the checker correctly TYPED the value (it rejected `x: int = w`). Member lookup was wrongly gated on whether the type NAME was imported into the current module (the per-module `self.structs`/`enum_methods`/`newtype_defs` tables are populated only by a whole-module OR named-TYPE import; a named-fn import injects nothing), so only `Match`/`Response`/`ProcResult` (globally seeded) resolved import-free. Broke the documented equivalence — `import lib; lib.make().bump()` and `import make, Widget from lib` both WORKED while `import make from lib` alone FAILED for an identically-typed value — and documented stdlib APIs (`import manual from std.cancel; manual().cancelled()`; `import min_heap from std.collections; min_heap().push(3)`). **Fix:** a LAZY, MISS-ONLY fallback keyed on the value's OWN module-scoped identity — on a local-table miss, resolve the shape from the OWNING module's `ModuleSig` by scanning `module_sigs` for the def whose `type_key(mid,name)` == the value's key (helpers `struct_shape`/`owning_struct_def`, `enum_methods_of`/`enum_type_params_of`/`owning_enum_def`, `newtype_methods_of`/`newtype_type_params_of`/`owning_newtype_def` in `src/checker/setup.rs`; swapped 6 member sites in `expr.rs`/`pattern.rs`/`sig.rs`). Purely additive: fires only on a miss (never shadows the globally-seeded seeds, zero cost on the local-hit path), reads `module_sigs` only — does NOT touch `struct_names`/`bare_types`, so **naming/constructing an un-imported type still errors** (`import make from lib; w := Widget(1)` → `unknown type 'Widget'`) and a same-named LOCAL type is unaffected (distinct keys, local table hits first). Transitive cross-module works (a `helper` returning a `cancel.Token`, `main` importing only `helper` → `.cancelled()` resolves; `cancel` is a transitive graph dep present before `main` is checked). Checker-only — no VM/runtime change (whole-module import already ran at runtime on both engines). +9 checker tests (RED-first, `check_graph`/multi-file graph) + 3 two-engine runtime parity tests; CLI-verified all three import forms print `11` and the stdlib APIs run on `run` + `run --serial`. Docs: `docs/reference/spec.md` (member-access-is-import-free clarification).
- ✅ **`json.stringify` faults (Go-style) on non-finite floats instead of emitting invalid JSON (Finding C)** (2026-07-10, `auto-task/json-nonfinite-fault`) — `stringify` emitted a `Json.Num` holding `NaN`/`+inf`/`-inf` as the bare tokens `NaN`/`inf`/`-inf` (`num_str` in `std/json.chz` fell through to `str(f)`), which are (a) not valid JSON and (b) rejected by Chezzi's own `json.parse` — so `parse(stringify(x))` failed and the library silently produced malformed output with no fault. **Policy chosen: FAULT, Go `encoding/json`-style** (over JS `null`-emission / Python's non-standard tokens) — clean here because `stringify` returns bare `str` with no `Result` channel, and prelude `panic(msg)` raises a recoverable `RuntimeError` catchable under `recover:` with a byte-identical message on both engines. **Fix (pure Chezzi, one guard, `std/json.chz` `num_str`):** as the FIRST body line, `if f - f != 0.0: panic("cannot serialize non-finite float to JSON")` — `f - f` is `0.0` for every finite `f` and `NaN` (≠ 0.0) for `NaN`/`±inf`, so it fires iff non-finite, INDEPENDENTLY of the ±9e15 int-collapse range below (a large FINITE float like `1e300` is outside ±9e15 but must NOT fault — reusing the range check would be wrong). +2 two-engine parity tests (`json_stringify_non_finite_faults_parity` covering NaN/+inf/-inf via `recover:` + `e.message()`; `json_stringify_finite_roundtrip_unchanged_parity` regression guard proving `1e300`/whole-floats/negatives/strings/nested Arr+Obj stringify byte-identically and round-trip), RED-first. Auto-applies to all engines (pure-Chezzi, identical bytecode). CLI-verified the task repro faults identically on `run` + `run --serial`. Docs: `docs/reference/stdlib.md` json section (non-finite policy, previously unspecified).
- ✅ **Two JSON number→int boundary bugs fixed at the shared f64→i64 seam (both total, consistent)** (2026-07-08, `auto-task/json-int-boundary`) — JSON stores every number as an f64 and both int-conversion paths mishandled the float→i64 boundary in opposite wrong directions. **Bug 1 (`json.decode[int]`)** SILENTLY SATURATED an out-of-range JSON integer: `decode[int]("1000000000000000000000000000000")` (10^30) returned `Ok(9223372036854775807)` (i64::MAX), `-1e30` → `Ok(i64::MIN)`, u64-max → saturated — Rust's `f as i64` cast saturates and the `D::Int` arm (`src/vm/call.rs`) range-checked nothing (only `fract`/`finite`). Fix: a range guard before the cast — `if f < i64::MIN as f64 || f > 9_223_372_036_854_775_808.0 { Err(out of range) }` — STRICT `> 2^63` upper bound so i64::MAX (which f64-rounds to exactly 2^63) still round-trips via the saturating cast while everything strictly beyond is rejected. **Bug 2 (`json.as_int -> Option[int]`)** FAULTED (uncaught, aborts the program) on out-of-range: `as_int` on `9999999999999999999` called the builtin `int()` which RAISES out-of-range. Fix (pure Chezzi, `std/json.chz`): gate on the same i64 window and `return None` when unrepresentable/non-finite, with a `n == 2^63` saturate special-case → `Some(i64::MAX)` (since `int(2^63)` itself faults). Both sites now map 2^63→i64::MAX, reject `|x|>2^63`, accept i64::MIN — for the identical out-of-range input `decode[int]` returns `Err` and `as_int` returns `None`; neither saturates, neither faults. Return type stays `Option[int]`; fractional truncation (`as_int(2.5)`→`Some(2)`) unchanged. +3 two-engine parity tests (`json_as_int_out_of_range_parity`, `json_decode_int_out_of_range_parity`, `json_int_boundary_consistency_parity`), RED-first, incl. the i64::MAX/MIN round-trip fences. Residual (inherent): a value exactly `2^63` (`"9223372036854775808"`) maps to i64::MAX at both sites — f64 cannot distinguish it from i64::MAX — consistent, not a saturation of the reported `|x|>2^63` class. Docs: `docs/reference/stdlib.md` json section.

- ✅ **`print`'s `str(self)` display hook is gated on ACTUAL `Stringable` conformance, killing an uncatchable SIGABRT** (2026-07-04, Bug B) — the VM stringifier selected a user `str(self)` method as the implicit display hook by NAME + ARITY only, ignoring its return type. A `fn str(self) -> S` (returns the struct, not `str`) was chosen; the stringifier got an `S` back, re-stringified it, re-invoked the hook, and recursed forever → `fatal runtime error: stack overflow` (SIGABRT, uncatchable by `recover:`) on a check-accepted program. Fix (VM-only, `src/vm/stmt.rs` struct/enum/newtype display-hook arms): invoke the hook, then use its result ONLY when the RETURNED VALUE is a `str` (new `Vm::is_str_value`, mirroring `arith.rs` `struct_hash`/`enum_hash`'s invoke-then-check shape); a non-`str` result is NOT re-stringified — it falls back to the default repr, like a wrong-arity `str` already did. Checking the returned VALUE (not the declared syntax) covers an annotated `-> str`, an INFERRED (un-annotated) str, and a str type-ALIAS return alike — an earlier syntactic `-> str` gate was rejected in review because it silently regressed idiomatic un-annotated/aliased `str` hooks to the default repr (checker-vs-VM `Stringable` divergence). GC-safety: the non-`str` fallback re-reads the LIVE rooted struct/enum/newtype (the hook may have mutated a field and swept the pre-hook clone), so the default render never dereferences a dangling GcRef. `str` stays a normal user method — a direct `obj.str()` returning non-str is untouched (no checker rejection). +5 tests (`src/vm/tests.rs`, all `assert_mc_parity` → serial + M:N): the repro (struct/enum/newtype self-return → default repr), annotated/inferred/aliased str all still used by print + interpolation, direct-call-returns-non-str still works, and a GC-stress fallback (hook mutates a non-interned `List` field + 100k-alloc loop + returns self → correct re-read state, no panic). Docs: `docs/reference/syntax.md` Stringable section now documents the display-hook resolution rule.
- ✅ **A runtime fault inside a `"{…}"` interpolation fragment now reports the fragment's real source LINE (was `line 1, col 1`)** (2026-07-04, `auto-task/interp-span-fix`) — any runtime fault (div-by-zero, index-out-of-bounds, integer overflow, …) whose faulting op sat INSIDE a string-interpolation fragment reported its span as `line 1, col 1` instead of the fragment's true line; the identical fault OUTSIDE interpolation reported correctly. This was the runtime/compiler counterpart of the 2026-06-30 `never-recover-span` checker fix (which corrected only the fragment ROOT nil-error span). Root cause: interpolation fragments are re-lexed from the escape-processed `raw` substring via `lexer::tokenize` (`src/interpolation.rs`), and `Lexer::new` hardcodes `line = 1`, so every fragment token span — and thus the arith/index opcode span the compiler emits and `Vm::err` renders — was fragment-relative (root at 1,1). Both serial and M:N VM share this codegen, so both printed the identical wrong span (why two-engine parity never caught it — it is a shared misleading-diagnostic bug, not a divergence). Fix (Strategy A, span-metadata only, runtime-inert): added a `base_line: usize` field to `Lexer` (default 0 → all normal lexing byte-identical) applied in the sole span funnel `span_at` as `line: self.line + self.base_line`, plus `Lexer::new_at` / free `lexer::tokenize_at`; `parse_interpolation` passes `base_line = span.line - 1` per fragment — the string literal's OPENING source line — so a fault inside any fragment reports that real line instead of `line 1`. We anchor to the opening line rather than the fragment's exact inner line ON PURPOSE: `raw` is the post-escape payload, where a `\n` ESCAPE and a genuine (triple-quoted) source newline are indistinguishable, so counting newlines in `raw` would inflate the reported line past an escape and point at UNRELATED code (a confidently-wrong diagnostic — flagged and fixed in review before merge). Opening-line is honest and never misattributes. COLUMN stayed best-effort/fragment-relative (`col 1`) at the time — **superseded on 2026-08-10 (M24 round 4)**, which added the matching `base_col` after fragment-relative columns turned out to alias two witness-table keys onto one entry; it was exact for a single-quoted literal and short by a bounded, documented amount past a triple quote or an escape — **both offsets superseded 2026-08-11 (M24-6)** by a per-literal `PosMap`, which deletes `base_line`/`base_col`/`new_at`/`tokenize_at` and makes every fragment span a real physical position. The shared parser hands the checker opening-line fragment spans too (symmetric, zero blast radius). +6 tests (`src/vm/tests.rs`): div-by-zero / index-OOB / overflow inside interpolation each report line 4 on BOTH engines, a multi-line triple-quoted fragment attributes to the string's OPENING line 4, a `\n`-escape-before-fragment fault stays on line 4 (regression guard against the escape miscount), and a non-interpolation fault + valid interpolation regression proves `base_line=0` leaves normal lexing byte-identical — all RED-first. Docs: refreshed the now-stale span doc comment in `src/checker/pattern.rs::check_interpolation`.
- ✅ **A nursery deadlock-abort now preserves a still-parked task's buffered stdout (two-engine parity)** (2026-07-05, `auto-task/deadlock-flush-parked-stdout`) — when a `parallel:` nursery was aborted by the M:N scheduler's deadlock detector, a still-PARKED task's ALREADY-buffered stdout was silently DISCARDED on the default M:N engine, while `--serial` printed it — a two-engine divergence on a DETERMINISTIC program (a consumer that prints three lines then blocks forever on a second `recv()` lost all three on `chezzi run`, kept them on `run --serial`). This was the exact gap the fault-output-flush entry below left open: `SchedCore::flag_deadlock` (`src/vm/mod.rs`) wrote each parked fiber's `TaskOutcome::Fault` slot with `out: String::new()`, discarding the fiber's own buffered output (`swap_ctx` had moved it into `f.ctx.out`/`f.ctx.stderr` when it parked), so the downstream `reduce_task_slots` propagated the deadlock error with an EMPTY buffer. Fix (~4 lines, the exact analogue of 888684d's real-fault fix): `flag_deadlock` now moves `f.ctx.out`/`f.ctx.stderr` into the `Fault` slot instead of allocating empties (`task_index`/`scope_id` are Copy, read before the partial move). `reduce_task_slots` was UNCHANGED at the time — it flushed only the lowest-index propagating fault's buffer at its task-order slot, so for a sole-printer parked task the transcript is byte-identical to serial; the MULTI-parked case was left open (now closed by the entry below). Scoped to the ONE `flag_deadlock` method (reached only by the M:N nursery deadlock detector); serial, real-fault, completed-sibling, and non-nursery-deadlock paths never route through it, so none regress. +1 two-engine parity test (`parallel_nursery_deadlock_flushes_parked_stdout_2engine`, sole-printer, looped 50× for interleaving flakiness), RED-first on the M:N arm only (serial passed pre-fix). Docs: `docs/history/designs/concurrency-tier-d.md` (Decision F), `docs/history/designs/concurrency-b3.md` (B3.5).
- ✅ **A MULTI-parked nursery deadlock-abort now flushes EVERY parked task's buffered stdout (two-engine parity)** (2026-07-05, `auto-task/multiparked-deadlock-flush`) — closes the remaining gap the entry above left open. When a `parallel:` nursery deadlocked with TWO-OR-MORE parked fibers, only the LOWEST-index propagating fault's buffer flushed on M:N, so a HIGHER-index parked fiber that printed before blocking had its output SILENTLY DROPPED — while `--serial` printed it live (a two-engine divergence on a DETERMINISTIC program: `silent`/idx0 parks empty and wins `first_fault`, so `printer`/idx1's `HI-FROM-PRINTER` was lost on `chezzi run`, kept on `run --serial`). Root cause: `flag_deadlock` faulted every parked fiber into a `TaskOutcome::Fault` slot, and `reduce_task_slots`'s `first_fault.is_none()` guard flushed only the first. Fix (3 touch points, additive): a DISTINCT `TaskOutcome::Deadlocked{err,out,stderr}` variant (`src/vm/mod.rs`), emitted by `flag_deadlock` instead of `Fault`; `reduce_task_slots` (`src/vm/sched.rs`) gets a `Deadlocked` arm that ALWAYS flushes out/stderr at its task-order slot (no `is_none()` gate) and a `deadlock_err` local, propagating ONE deadlock error (terminal match precedence Exit>Fault>Deadlock). Chosen over string-matching `DEADLOCK_MSG` (collision-fragile with a user `panic("deadlock: …")`) — a separate variant keeps the REAL-fault multi-fault ordering (the residual race the prior entry documented) provably byte-identical. `Deadlocked` never coexists with a real `Fault`/`Exit` (a real fault/exit trips `terminate` first). +2 two-engine parity tests: `parallel_nursery_deadlock_multiparked_flushes_higher_index_2engine` (2-fiber, sole-printer, order-deterministic exact match) and `parallel_nursery_deadlock_multiparked_multiprinter_set_3parked` (3-parked, two disjoint printers, order-insensitive SET via `assert_same_lines` per mn-parity discipline), both looped 50×; RED-first on the M:N arm only. Unit `mnsched_deadlock_when_all_parked_runq_empty` updated to assert `Deadlocked`. Verified end-to-end on the release binary at `--serial` and `--threads=1/2/8` (byte-identical `HI-FROM-PRINTER` on stdout, deadlock error on stderr, exit 1, thread-count independent). Docs: `docs/history/designs/concurrency-tier-d.md` (Decision F), `docs/history/designs/concurrency-b3.md` (B3.5).
- ✅ **A faulting `--parallel` task now preserves the stdout it buffered before the fault (two-engine parity)** (2026-07-04, `auto-task/fault-output-flush`) — a spawned task that FAULTS (panic / uncaught runtime error propagating to the nursery join) silently DROPPED all stdout it emitted *before* the fault, but ONLY on the default M:N OS-thread engine — the `--serial`/interp oracle preserved it (a two-engine divergence losing the user's debug output right before a crash, on the DEFAULT `chezzi run`). Root cause in `src/vm/mod.rs`: `TaskOutcome::Fault` was a tuple variant carrying NO buffered output, unlike `Exit { code, out, stderr }` and `Done(WorkerResult)`; every Fault construction site dropped the shell buffer and `reduce_task_slots` flushed only `Done`/`Exit` output in task order. The old rationale ("a faulting worker's partial output never had a deterministic position") was wrong for the LOWEST-index *propagating* fault: it has exactly the slot position the serial engine emits it at (after lower-index Done/Exit, before the propagated error is handled). Fix (localized to the fault-output-flush seam): made `Fault` a struct variant `{ err, out, stderr }` mirroring `Exit`; the real Fault sites that own a live buffer (`run_outcome`, `classify_mn_outcome`) `mem::take` the shell `out`/`stderr` (the Rust-panic-to-fault site carries an empty buffer; the deadlock-terminate site was later fixed to carry the parked fiber's own buffer — see the deadlock-flush entry above); `reduce_task_slots` flushes the terminal (`first_fault.is_none()`) fault's buffer INLINE at its task-order slot, then records the error. Higher-index racy faults + `Cancelled` still drop (no deterministic slot); `Exit`-over-`Fault` precedence byte-for-byte unchanged; the cooperative/`--serial` oracle untouched (the fix makes the default engine MATCH it). Fault-free goldens only ever hit `Done`, so byte-identical. **Residual (intentionally not chased):** byte-for-byte oracle parity holds only when the faulting task is the nursery's SOLE output-producer — with additional output-producing siblings the M:N transcript can still diverge from serial's stop-at-first-fault order (a sibling reaching `Done` before the faulter's cancel-trip keeps output serial never produced; `Fault`-vs-`Cancelled` classification is itself a scheduler race), a pre-existing nondeterminism the buffer-and-flush model cannot reconcile. +1 three-engine parity test (`parallel_faulting_task_flushes_partial_output_3engine`, single-faulter — the only deterministic fault-output shape), RED-first on the `--parallel` arm only. Docs: `docs/history/designs/concurrency-tier-d.md` (Decision F), `docs/history/designs/concurrency-b3.md`.
- ✅ **Float→string for large integral floats now shortest-round-trip-correct** (2026-07-01, `auto-task/float-shortest-roundtrip`) — the integral-valued branch of float formatting used `format!("{x:.1}")` (exact fixed-point expansion of the binary `f64`), so a large whole-valued float printed the artifact digits of its binary value instead of the documented shortest decimal that round-trips: `1.5e23` → `150000000000000004194304.0` (should be `150000000000000000000000.0`), `6.022e23` → `602200000000000027262976.0`. Contract (`docs/reference/syntax.md:1787`, unchanged — the docs already promised "shortest-round-trip-correct … spelled out in full") was violated by the implementation. Fix: render the integral branch via Rust's default shortest `{}` Display (guaranteed fewest round-tripping digits AND never scientific notation for f64) then append a literal `.0` to preserve Chezzi's always-a-decimal-point invariant — `format!("{x:.1}")` → `format!("{x}.0")` in ALL THREE lockstep sites (`vm::format_float`, `interp::value::format_float`, `fmtspec::format_float_like`, single commit) so the stringify path and the bare-format-spec path stay identical and VM==interp holds. Behavior-preserving for every already-correct case (`3.0`, `-0.0`, `0.0`, `100.0`, `1e20`→`100000000000000000000.0`, negatives); the explicit `:e`/`:f`/precision spec arms are untouched. +1 golden `examples/float_large_integral.chz` (bare interpolation + bare-spec + small-integral controls, VM==interp==`.expected`), RED-first; `examples/literals.expected` avogadro artifact updated to the shortest form.
- ✅ **Never/bottom `recover:` payload now CONSISTENT + accurate interpolation-fragment nil-error span** (2026-06-30, `auto-task/never-recover-span`) — two coupled checker-only corner fixes. (A) `infer_recover` (`src/checker/mod.rs`) typed a `recover:` block by its tail: a direct `recover: panic(...)` went through the `StmtKind::Expr` arm and `infer`'d `panic` to bottom (`Ty::Unknown`, accepted as an `Ok` payload in value position), but a tail whose panic was reached through one more **statement-form** layer (`recover:\n  match 1:\n    _: panic("boom")`) went through `_ => check_stmt` and left `value_ty = Ty::Nil`, so the `Ok(v)` payload typed as `nil` and was rejected ("expression returns no value (nil) and cannot be used as a value") — the SAME bottom value usable in one path, banned in the other. Fix: after the tail check, `if value_ty == Ty::Nil && Self::stmt_terminates(last) { value_ty = Ty::Unknown; }`, reusing the existing sound, conservative divergence predicate (statement-form match all-arms-terminate, `while true:`, all-branch-returning `if/else`, trailing `exit`/`panic`). Both repro forms now accept; the `== Ty::Nil` guard keeps concrete-tail (`recover: 5` → `int`) and non-diverging-statement-tail (`recover: x := 5` → `Result[nil]`, still nil-banned) recovers untouched. (B) `check_interpolation` (`src/checker/mod.rs`) inferred each `{…}` fragment from a sub-parse with fragment-relative spans (root at `line 1, col 1`), so a nil-in-value-position error keyed on the fragment ROOT reported the bogus `(1,1)` fallback instead of the offending string literal; now stamps the whole-string-literal span onto the fragment root (`e.span = span`) before `infer_value`, matching the compiler's emit site, so the diagnostic carries the real line/col. Parity-safe by construction (check-time only; rejected programs never run, and the newly-accepted match-panic recover runs identically on VM and `--serial` → the panic takes the `Err` branch, the `Ok(v)`/`Unknown` arm is statically unreachable). +4 checker tests via `entry_ok`/`entry_rejects`/`check_entry` (diverging-match-tail accepts, direct-vs-match-panic consistency, concrete/non-diverging-tail regression fence, interpolation-void-fragment span ≠ (1,1)), all RED-first.
- ✅ **Expected-type inference: a type ANNOTATION now pins a generic ctor / generic fn-call's type params** (2026-06-30, `auto-task/expected-type-generic-inference`) — checker had checking-mode ("expected type flows IN") only for empty container literals and closures bound to `fn`-typed annotations; a generic constructor or generic function call was always inferred bottom-up, with the annotation used only as a POST-HOC `assignable_w` check. So `a: Heap[int] = Heap([], fn(x, y): x < y)` (and the return-type / call-arg forms) hit the un-inferable-closure-param deadlock — the empty `[]` couldn't pin `T`, the bare comparator params couldn't either, and `report_uninferable_closure_params` fired *before* the annotation could break the tie ("cannot infer type parameter `T` of `Heap`" + "cannot infer type of parameter 'x'/'y'"). Fix (checker-only, parity-safe by construction — generics are type-erased at runtime, no opcode/runtime change): a new `expected_hint: Option<Ty>` field threads the expected type from the three annotation sites — a `let`-binding's declared type (`StmtKind::Let` non-`ref` single-name non-closure branch), a function's declared **return** type (`check_return` non-closure branch), and a call **argument**'s declared parameter type (`infer_arg` non-closure branch) — into `infer_call`, which `take()`s it FIRST (so nested arg calls see `None`, no leak) and threads it through every generic ctor/call dispatcher (`infer_named_call` struct/newtype arms, `infer_qualified_struct_call`, `infer_newtype_call`, `infer_variant_call`, `infer_generic_call`). Each consumes it via a new `seed_from_hint(hint, &<declared-return-SHAPE>, &mut sub)` (`Struct(key,[Param…])` / `NewType` / `Enum` / a generic fn's `sig.ret`) placed AFTER arg-unification + `recover_iter_elems`/`recover_index_args` and BEFORE `report_uninferable_closure_params` — so precedence is **turbofish > arguments > annotation** (`unify` only binds a still-free param; an arg that pins `T` differently is the usual mismatch). Once `T` is seeded the existing `check_generic_arg` re-infers the comparator closure in checking-mode against `fn(int,int)->bool`, so the secondary "cannot infer parameter 'x'/'y'" errors also vanish. Bonus (same seam, for free): generic **newtype** ctor annotations (`e: Stack[str] = Stack([])` — previously needed a turbofish) and a return-only param of a generic fn (`xs: List[int] = empty()` for `fn empty[T]() -> List[T]`, previously "cannot assign List[T] to List[int]"). +6 graph_tests (3 primary repros let/return/call-arg + qualified-ctor + free-fn-return + a turbofish/annotated-closure/args-win-mismatch regression guard), all RED-first. **Remaining gap (documented, not forced):** a generic ctor nested inside a *container literal* (`a: List[Heap[int]] = [Heap([], …)]`) — the outer expr is a list literal, never reaches `infer_call`, so it would need a separate `infer_list` element-hint; annotate the closure params or turbofish there. Docs: `docs/reference/syntax.md` (closure-param inference + generic-newtype §), `docs/reference/spec.md` (new expected-type-inference note + newtype ctor), `docs/reference/stdlib.md` (Heap §).
- ✅ **Container constructors `List[T]()` / `Map[K,V]()` / `Set[T]()` + bare `List()`/`Map()`; un-inferable-closure-param diagnostic; std standalone-check; std-module test wiring** (2026-06-29, `auto-task/container-ctor-turbofish`) — four bundled audit findings. **A (the ask):** the turbofish was rejected on every builtin but `Channel` — `name_is_generic` (`src/checker/mod.rs`) now also accepts `List`/`Map`/`Set`, and the three ctor arms read the type args (1 for List/Set, 2 for Map; arity-checked), so `List[int]()` pins an empty list's element type, `List[int]([1,2])` checks elements against it, and bare `List()`/`Map()` are now legal (mirroring the already-legal `Set()`), refined from the expected type / first use. **A is NOT checker-only** (audit's claim was wrong): 0-arg `List()`/`Map()` were rejected at RUNTIME in both engines too, so `builtin_list`/`builtin_map` in `src/vm/mod.rs` AND `src/interp/mod.rs` now return an empty container for 0 args (Set's existing shape) — two-engine parity held via a new `examples/container_ctor.chz` golden. **B:** `Heap([], fn(a,b): a<b)` leaked a misleading "cannot compare T and T" from inside the lambda; a new `report_uninferable_closure_params` guard (bare struct-ctor path + `infer_generic_call` + the module-qualified struct-ctor path `infer_qualified_struct_call`, so `c.Heap([], fn(a,b): a<b)` gets the same message) detects the genuine deadlock — an unbound type param appearing in an *unannotated* closure's PARAMETER slot — and emits "cannot infer type parameter `T` of `Heap`; annotate `Heap[T](…)` or the closure parameters", binding the param to `Unknown` to suppress the cascade. Scoped to parameter positions only (so `Mapped`-style `fn(T)->U` with `U` inferred from the body does NOT trip it) AND **probed against the closure body** (`trial_check_closure_args`, two trials — params left as the unbound `Ty::Param` vs. bound to `Unknown`): it fires ONLY when the body actually constrains the param (`a < b` errors unbound but is clean under `Unknown`), so a harmless body that doesn't need `T` (`each([], fn(x): print(x))`, `mapper([], fn(x): 42)`) keeps type-checking — it ran on `main` and must not be newly rejected — and an unrelated body error (errors under BOTH trials) is left for the normal per-arg check to report as itself. **C:** standalone `chezzi check std/…chz` reported phantom "unknown type 'RwShared'/'Shared'" — stdlib auto-privilege was granted only on the import path; `LoadedModule::is_std` (`src/resolver/mod.rs`) is now path-aware (file under `std_root()`), fixing the editor/LSP false positives; new lib test standalone-checks every `std/**/*.chz`. **D:** the committed per-module std test files (`collections`/`concurrent_collection`/`datetime`/`path`) existed but only 4 unrelated `_test.chz` were in the `cargo test` dogfood guard — all four are now registered (`src/test_runner.rs`). Docs: `docs/reference/syntax.md` + `docs/reference/stdlib.md` (turbofish + bare-empty constructor forms). `Shared`/`RwShared`/`Atomic` turbofish left OUT (value-first; tests intentionally reject it). **[SUPERSEDED 2026-06-30:** the value-first concurrency boxes now ALSO accept an optional, value-checked turbofish — see the "Turbofish construction on the value-first concurrency boxes" entry in *Current focus*.**]** Checker/resolver-only on the type side; both engines exercised; conformance green.
- ✅ **Negative int literal/range patterns + match-doc qualifier fix** (2026-06-28, `auto-task/neg-match-patterns`) — two bundled match-pattern changes, one commit. PART 1 (parser/grammar only — AST/checker/compiler/vm/interp already `i64`-signed, NO runtime change): a leading `-` in a pattern was a hard parse error ("expected identifier, found '-'"). Root cause: `parse_pattern_primary` (`src/parser/mod.rs`) only entered its int branch on `Token::Int`. Fix: new `expect_pattern_int` helper (eats an optional `Token::Minus` then `expect_int`), and widened the literal arm to `Token::Int(_) | Token::Minus`, using it for the literal AND both range bounds → `-3:`, `-10..-5:`, `-10..5:`, `0..-5:` all parse (and compose with guards/or-patterns). Stays **int-only**: a negative float `-3.0:` now routes through `expect_int` and is rejected "expected integer, found float" (no float pattern added; positive `3.0:` unchanged). A negative literal arm is still refutable — `_` is still required for exhaustiveness. `docs/reference/grammar.bnf` `<patternPrimary>`: +4 `MINUS` alternatives + int-only comment; new accept/reject conformance corpus (`tests/corpus/accept/match_neg.chz`, `tests/corpus/reject/match_neg_float.chz`). +2 parser tests, +2 VM `run_parity` tests (neg literal/range + neg-with-guard/or), +2 checker exhaustiveness tests. PART 2 (doc-only): `docs/reference/syntax.md` match/enum examples showed BARE arms (`Circle(r):`, `Leaf:`) but the impl requires QUALIFIED `Enum.Variant` — qualified all of them (Shape/Tree/Color groups) to match the implementation + prose; each edited snippet CLI-`check`ed clean. i64::MIN (`-9223372036854775808`) stays unparseable (lexer rejects the magnitude) — known limit, unchanged. Two-engine parity (VM==interp) green; `cargo test conformance` green.
- ✅ **`?` sum-type KIND soundness: a Result-`?` is rejected in an Option-returning fn (and vice versa)** (2026-06-27, `auto-task/try-kind-check`) — checker hole in `infer_try` (`src/checker/mod.rs`): the pre-computed `ret_err` collapsed both `Ty::Option(_)` and `Ty::Nil` enclosing returns to `None`, so (1) the `Ty::Result` operand arm SKIPPED its compatibility check whenever `ret_err==None` (a `Result`-`?` slipped through an `Option`-returning fn) and (2) the `Ty::Option` operand arm never inspected `current_ret` (an `Option`-`?` slipped through a `Result`-returning fn). The mistyped fn then returned the wrong sum-type and FAULTED a downstream exhaustive `match`/`??` at runtime ("no match arm for variant 'Err'/'None'") even though `check` passed. Fix (checker-only, no runtime/parity change — both engines inherit the stricter validation): dropped `ret_err`, folded a `current_ret` KIND match into each operand arm — `Result`-operand ⇒ enclosing must be `Result` (keeps the existing error-TYPE check) or `Nil`; `Option`-operand ⇒ enclosing must be `Option` or `Nil`; mismatched kinds get distinct errors ("'?' propagates a Result error, but the enclosing function returns Option, not Result" / "…returns Result, not Option"). `Nil` (top-level/`main`) still accepts either; `Unknown` enclosing (inferred-closure return) stays REJECTED. +4 checker tests (2 RED-first KIND-mismatch repros + 2 compatible-still-ok guards); existing error-TYPE + closure + recover guards stay green. Docs: `docs/reference/syntax.md` §9 `?`-kind clarification.
- ✅ **Inferred struct/enum method return types now FLOW (soundness; closed an unchecked-struct-body hole too)** (2026-06-27, `auto-task/method-return-inference`) — checker bare-key vs module-key divergence: in the `build_graph`/`check_graph` path the struct layout is stored under `<module-key>::Name`, but `struct_self_ty` + `infer_returns_pass`'s struct branch + the pass-2 struct body-check guard all looked up `self.structs.get(name)` by the BARE name → misses. Three coupled defects, one root: (1) an un-annotated method's inferred return was written to a non-existent slot, so `s: str = P(3).val()` silently accepted `int` into `str` (and protocol satisfaction read `Unknown` — an inferred `compare→bool` wrongly satisfied `Comparable`); (2) `struct_self_ty` built `Ty::Struct(BARE, [])` corrupting `self`'s type; (3) the pass-2 guard missed → struct method **bodies were entirely UNCHECKED** in the entry path (`y: str = self.x` passed). Plus (4) `infer_returns_pass`/`count_uninferred` had no `Enum` arm → enum method returns never inferred. Fix (checker-only, parity-safe by construction — no opcode/runtime change): bare_key the three struct lookups + the `Ty::Struct` key (mirror the already-correct `enum_self_ty`), and add the `Enum` arm. Turning struct-body checking ON surfaced a pre-existing latent checker bug — the duplicate-binder pre-pass (`first_duplicate_binder`) counted a bare nullary-variant ident (`None`) as a binder, so `(None, None, None)` was falsely "bound more than once" (only ever hit inside struct method bodies, e.g. `examples/slicing.chz`); fixed by passing it the same variant-name predicate `bind_subpattern` uses. +11 checker tests (entry_rejects/entry_ok on build_graph path — the single-module `ok()`/`rejects()` helpers mask the bug). Newtype method-return inference left intentionally unfixed (out of scope; `newtype_self_ty` already key-correct) — known consistency follow-up. Docs: `docs/reference/syntax.md` return-inference note (methods + protocol satisfaction).
- ✅ **Match-exhaustiveness soundness: guarded / refutable-payload variant arms no longer close a variant** (2026-06-27, `auto-task/match-exhaustiveness-guard`) — checker hole: `bind_match_arm` inserted a variant into the `covered` set UNCONDITIONALLY, ignoring (a) the arm's guard and (b) refutable payload sub-patterns (literals/ranges/nested-variants). So `E.A if false: …` / `Some(0): …` / `P.Pair(0, y): …` passed `chezzi check` then FAULTED at runtime ("no match arm for variant …"). Fix (checker-only, no runtime/parity change — both engines already fault identically): threaded `guarded: bool` into `bind_match_arm`, collected `payload_irref` from the existing `bind_subpattern` zip loop, and gated coverage on `!guarded && payload_irref`; duplicate-arm detection now keys on `covered.contains` (a PRIOR fully-closing arm) so the standard `E.A(n) if c` → `E.A(n)` guard-then-fallback idiom is ACCEPTED instead of wrongly rejected as "duplicate match arm". Tuple + int/str/bool literal scrutinees untouched (already conservative). +5 checker tests (4 RED-first repros + a duplicate-still-rejected regression guard). Docs: `docs/reference/syntax.md` §8 refutable-payload clarification.
- ✅ **str methods (split-brain, minimal subset) + safe numeric parse** (2026-06-23,
  `auto-task/str-methods-safe-parse`) — gaps #1 (str half) + #7. Added 11 receiver methods on `str`
  that forward to the existing `std.str` free fns (`ends_with`/`replace`/`repeat`/`reverse`/`pad_left`/
  `index_of`/`count`/`strip_prefix`/`strip_suffix`/`split_lines` + `strip`, a `trim` alias) so
  `s.ends_with(x)` works like `s.starts_with(x)` with no import; plus `to_int() -> int?` /
  `to_float() -> float?` that return `Some`/`None` instead of raising on bad input (trim + `parse`,
  reusing the `int()`/`float()` parse path). Pure-native Rust in **both** engines (checker
  `str_method_sig`, VM `core_method` Str arm, interp `str_method`), byte-identical to the std.str
  codepoint-loop oracle — `index_of` returns a **codepoint** index (not Rust's byte offset), `replace`/
  `count` guard the empty-arg edge, `repeat` n≤0 → `""`. The `std.str` free fns are untouched
  (`examples/str_more.chz` still green). Golden `examples/str_methods.chz` exercises every method incl.
  multibyte + `Some`/`None`, asserted byte-identical across all three engines. Out of scope (left open):
  the full `std.iter`/`std.cmp` receiver re-export half of #1. Docs: `docs/reference/stdlib.md` str method table +
  `std.str` note, `docs/reference/syntax.md` method cheat-sheet, `gaps.md` (#1 str half + #7 → resolved log).
- ✅ **Left-shift overflow now a recoverable fault** (2026-06-23, `auto-task/shift-overflow`) — `1 << 63`
  silently wrapped to `i64::MIN`, violating the "every i64 overflow is a recoverable fault" policy
  (the shift handler validated only the shift-*amount* range, never value overflow, unlike `+ - * / %`).
  Fix (both engines, `vm/mod.rs` `bitwise()` + `interp/mod.rs` `eval_binary` Shl arm): a left-shift-only
  round-trip check — `(a << b) >> b != a` ⇒ raise the shared `integer overflow in Shl`. Round-trip-safe
  shifts incl. `-1 << 63 == INT_MIN` still succeed; `>>` is unchanged (arithmetic, never overflows).
  Golden `examples/edge_cases.chz` `shift_ovf63` probe pins it on all three engines + a VM unit test
  guards the non-overflow regressions. Docs: `gaps.md` nit resolved, `docs/reference/spec.md` overflow policy +
  `docs/reference/syntax.md` shift note updated.
- ✅ **`list.map`/`.filter`/`.fold` OOB-on-shrink fixed** (2026-06-21, `auto-task/list-hof-shrink-oob`) —
  VM `list_hof` captured `n = v.len()` once then indexed the *live* heap list, so a callback that
  shrank the receiver (`xs.pop()`) ran a stale index past the now-shorter `Vec` → `index out of bounds`
  panic (vm/mod.rs:6840 map/filter, ~6890 fold) on both engines. Fix: allocate a **rooted snapshot**
  of the receiver's elements at call time and index that (mirrors `list_sort_by`; the interp already
  snapshots `elems` before dispatch, so this aligns the VM to interp). **Chosen semantics: snapshot** —
  map/filter/fold iterate the receiver's elements as of call time; a callback that shrinks **or** grows
  the receiver does not perturb iteration (consistent with comprehensions/`for`-loops/Python). Tests:
  `map`/`filter`/`fold`_shrinking_callback_no_panic + golden `examples/list_hof_shrink.chz` (VM==interp).
  Docs: `docs/reference/stdlib.md` (snapshot note), `gaps.md` (entry → ✅ RESOLVED).

- ✅ **User-callable `panic(msg: str)` builtin** (2026-06-20, `auto-task/panic-builtin`) — exposes a
  user-facing way to raise the **same** recoverable `RuntimeError` the runtime already uses internally
  (overflow / OOB / bad decode); the M11 `recover:`/`defer` machinery catches it unchanged. `panic`
  **unwinds** (it is NOT sugar for `return Err(...)` — that already exists for *expected* errors):
  caught by the nearest `recover:` as `Err(e)` with `e.message() == msg`, else it aborts the program
  with that message + non-zero exit (byte-identical to an integer overflow), running `defer`s on the
  way out. It is **bottom-typed** (`Ty::Unknown`, no new `Ty::Never`): type-checks as a statement, as
  a diverging branch tail (no explicit `return` — `expr_is_diverging_call` generalizes the `exit`
  precedent), and in value position (`x := if ok: v else: panic("no")` takes `v`'s type via
  `unify_branch`). Pure-builtin path — compiles to `Op::CallBuiltin("panic", 1)`; each engine's
  name-keyed dispatcher returns `Err(RuntimeError{message, span})` (VM `do_builtin` early-return /
  interp `eval_call` interceptor) instead of an `Ok` value. Registered across all four name tables
  (checker `is_reserved_name` + `builtin_call`, interp + compiler `is_builtin`). No grammar change
  (plain call). New golden `examples/panic.chz`; checker/interp/VM unit tests + cross-engine parity.
- ✅ **Match arms accept module-qualified enum-variant patterns (`geo.Color.Red`)**
  (2026-06-20, `auto-task/qualified-variant-patterns`) — match is now symmetric with construction:
  for an enum from a whole-module `import geo` you can write `match c:\n  geo.Color.Red:` directly
  (was a `parse error: expected ':', found '.'`; workaround was `import Color from geo` + bare
  `Color.Red`). The 3-part spelling is `module.Enum.Variant` (the binder is the bound module name —
  last path segment or `as` alias); `import geo as g` → `g.Color.Red:`; payload bindings work
  (`geo.Shape.Circle(r):`). A new `module_name: Option<String>` on `Pattern::Variant` carries the
  binder; the **parser** accepts an optional leading `IDENT.` (a 3rd dot deterministically means
  module-qualified — unambiguous); the **checker** (`check_pattern_qualifier`) validates the module is
  bound + owns the enum (errors render BARE names, never the `::` identity key) then resolves the enum's
  identity key and delegates to the existing scrutinee-driven validation; **both engines drop the binder**
  and key on the same `(enum, variant)` identity as the bare/named-import form, so VM == interp ==
  `--serial` == `--parallel` byte-for-byte (exhaustiveness unchanged, by identity). A bare user-variant
  is still rejected with the "write it qualified" hint; `Ok/Err/Some/None` stay bare; a 2-part
  `module.Variant` (dropping the enum) is NOT accepted. Docs: `docs/reference/grammar.bnf` (+conformance green),
  `docs/reference/syntax.md` match section.
- ✅ **C-ABI FFI: module-qualified type at the extern boundary (`mod.Type` / `mod.Alias`)**
  (2026-06-20, `auto-task/ffi-qualified-type`) — fixed a scoping bug in the module-scoped-types
  feature: a module-qualified type written at an `extern` boundary (`cdefs.DivT`, `w3.Len`, AST
  `Type::Qualified`) was not lowered to a C type, so the checker (which resolves `Qualified`) and the
  backends disagreed. Symptoms: a qualified RETURN struct silently became void (`cannot read field … of
  nil`); a qualified PARAM panicked the VM at the marshal loop's `.expect`. Root cause: `qualify_ffi_type`
  (compiler) and the interp `qualify` closure only rewrote a bare `Type::Named` struct → identity key and
  passed `Type::Qualified` through unchanged, so the byte-identical `ctype_of` twin (no `Qualified` arm)
  lowered it to `None`. Fix: both rewrites now resolve `Qualified { module: binder, name, .. }` via
  `imported_modules`/`module_types`/`type_keys` → a qualified STRUCT becomes `Named(identity_key)` (hits
  the identity-keyed `struct_fields`), a qualified WIDTH ALIAS becomes `Named(bare name)` (hits the
  bare-keyed `aliases`), all BEFORE `ctype_of` so the twin stays byte-identical. Also converted the
  param-marshal `.expect("checker verified marshallable param")` (both engines) into a graceful
  compile/runtime error mirroring the checker's "not C-marshallable" wording — a user program can no
  longer panic the VM via this path (the checker remains the real gate). Named-import spelling
  (`import DivT from core.cdefs`) already worked; only the DOTTED spelling was broken. Tests: three new VM
  parity tests (qualified return struct → 3/2, qualified width param → 7, non-marshallable qualified →
  clean error not panic), two new checker guard tests; full suite (2279) + conformance green, clippy
  clean. Docs: `syntax.md` §12b, `ffi-and-packaging.md`, this file. Out of scope (untouched): the
  separate "type alias to an FFI STRUCT at the boundary" inconsistency.
- ✅ **C-ABI FFI follow-up: module-qualified WIDTH ALIAS resolves to its DEFINING module's width**
  (2026-06-20, `auto-task/ffi-qualified-type-fix`) — the adversarial panel found the prior fix
  reintroduced the bare-name class for the WIDTH-ALIAS case: the qualified arm rewrote `mod.Alias` to a
  bare `Named(name)`, which `ctype_of` then resolved through the flat, program-global, **bare-keyed**
  `aliases` table (last-write-wins). So when two reachable modules both declared `type Len` with
  DIFFERENT widths (`core/w3.chz` int64 + a colliding local `type Len = int8`), `w3.Len` collapsed to
  bare `Len` and silently marshalled through the WRONG width — the checker said OK (int64) but all three
  engines printed `44` (int8-truncated `abs(-300)`) instead of `300`. Fix (module-scoped, mirrors
  `type_keys`): added a `module_aliases: (module_idx, name) → body` map to BOTH engines, populated
  alongside the existing alias gather; the qualified width-alias arm now looks up the body by the
  ALREADY-resolved defining-module index `tidx` and returns THAT (an `int64` width scalar `ctype_of`
  resolves directly, no flat-map hop), so a colliding local alias can't hijack the C ABI — matching the
  checker, which resolves a `Type::Qualified` alias via the defining module's `type_aliases`. The
  qualified STRUCT path, the non-colliding qualified width path, the bare/named-import path, and the flat
  `aliases` table are all untouched. Tests: one new VM 3-engine collision parity test (`w3.Len`=int64 +
  local `Len`=int8 → `abs(-300)`=300 on VM/`--serial`/`--parallel`); the existing non-colliding twin
  (→7), struct (→3/2), and clean-error guards stay green; full suite + conformance green, clippy clean.
  Docs: `ffi-and-packaging.md`, this file. (The single-hop fix's chained-alias gap is closed by the
  ROOT fix below — chains are now resolved fully module-scoped at all depths.)
- ✅ **C-ABI FFI FINAL ROOT fix: qualified/imported/aliased extern types resolve via the CHECKER**
  (2026-06-20, `auto-task/ffi-qualified-type-fix4`) — ended the AST-recursive alias-spelling
  whack-a-mole (fix..fix3 each closed one spelling and the next re-entered a flat bare-name alias map).
  Confirmed-still-broken on fix2: a **named-import chain hop** (`core/widths` = `import int64 from
  std.ffi` + `type W = int64`; `core/w3` = `import W from core.widths` + `type Len = W`; `main` =
  `import core.w3` + colliding `type W = int8` + `extern fn abs(n: w3.Len) -> w3.Len`) — `check` OK
  (w3.Len → W(from widths) → int64) but `run`/`--serial`/`--parallel` all printed **44** (main's
  colliding int8) instead of **300**. Root cause: the backend's `qualify_ffi_type`/`resolve_qualified_
  alias` only knew aliases DECLARED in the defining module (`module_aliases`); a name brought in via
  `import X from other` matched neither key and fell back to the flat last-write-wins bare `aliases`
  map → collision. **The robust fix (mandated): one resolver — the checker.** New
  `checker::resolve_extern_signatures(graph) -> ExternTable` runs the SAME deps-first module pass and,
  for each `extern` fn, records the fully-resolved width-bearing `CType` per param/return via a new
  `resolve_ctype` walk that mirrors `resolve_ty_ro`'s alias/`from`-import/`Qualified`/cycle logic but
  stops at the WIDTH leaf (`Ty` collapses every FFI width to `Ty::Int`, so the carrier must be a
  `CType`, not a `Ty`). The width crosses module boundaries via a new `AliasSig.ctype` (computed in the
  defining scope) + a parallel `imported_alias_ctypes` populated in `bind_import`. **Both backends now
  consume the table** (keyed by `(graph module idx, fn name)`, the index both derive) and NEVER
  re-resolve alias names — closing every spelling at once: single-hop, local chain (any depth),
  named-import hop, qualified hop, AND mixed chains. **Deleted** the dead machinery: `qualify_ffi_type`
  + `resolve_qualified_alias` + `module_aliases` in BOTH engines. (At fix4 the standalone source-string
  test path still kept a LOCAL-only `ctype_of` fallback — **that second resolver was deleted in fix5
  below**; the standalone path now goes through the checker too, so there is exactly ONE resolver.) The
  fix2 "cross-module qualified body mid-chain (`type Len = other.X`)" `None`
  case is now resolved too (the checker has each module's real import-binder map). Tests: new VM 3-engine
  parity tests for the named-import hop and a LOCAL→named-import→QUALIFIED **mixed** chain (each hop a
  collision, all → 300 on VM/`--serial`/`--parallel`), 7 new checker `resolve_ctype` unit tests
  asserting the exact `CType` per spelling (the dual-resolver-drift guard), and all prior FFI guards
  (single-hop/chain collisions → 300, struct → 3/2, width param → 7, cyclic → clean error, non-
  marshallable → clean check error) stay green. The stale `extern_cross_module_alias_runs` test (which
  asserted a BARE cross-module alias the checker now rejects as module-scoped) was corrected to the
  `import Size from sizes` spelling. Full suite (2292) + conformance green, clippy `--all-targets`
  clean; CLI repro 20×/`--parallel` deterministic at 300.
- ✅ **C-ABI FFI ARCHITECTURALLY-FINAL fix: struct FIELDS resolve in the STRUCT's defining scope +
  the second resolver is DELETED** (2026-06-20, `auto-task/ffi-qualified-type-fix5`) — closed the one
  regression the fix4 redesign introduced and made dual-resolver drift structurally impossible. **The
  regression:** a qualified/imported extern RETURN STRUCT whose FIELDS are typed via the DEFINING
  module's local alias (`core/cdefs.chz`: `type Half = int32` + `struct DivT{quot:Half; rem:Half}`;
  `main`: `extern fn div(...) -> cdefs.DivT`) resolved to a **void return (nil)** — `run`/`--serial`/
  `--parallel` all faulted with `cannot read field 'quot' of nil` (expected quot 3, rem 2). Root cause:
  the checker's `resolve_struct_ctype` read the struct's raw field ASTs but resolved each field via
  `resolve_ctype_d`'s alias arms against the **importing** module's `aliases`/`imported_alias_ctypes`,
  where `Half` is invisible → field `None` → whole-struct `CType` `None` → backend lowered the return as
  void. **Structural fix (extends the `AliasSig.ctype` precedent to structs):** a graph-wide
  `struct_ctypes: HashMap<identity-key, Option<CType>>` cache on the `Checker`, populated once per module
  after `hoist` (all that module's aliases/`from`-imports live) and before the check_stmt loop, each
  struct's complete by-value `CType::Struct` computed **in its OWN defining module's scope**. Modules are
  checked deps-first, so an importer's extern returning `mod.Struct` reads the cached defining-scope CType
  **verbatim**; `resolve_struct_ctype` became a pure cache read (the bare/same-module arm keeps a
  field-walk fallback in the defining scope for forward-ref nested structs; the qualified arm NEVER
  field-walks — it only reads the cache). **Single-resolver enforcement (deletion):** removed the
  backends' second resolver entirely — `compiler::ctype_of`/`ctype_of_visiting` + `gather_aliases` + the
  `aliases` field + their `ctype_of_maps_*`/`ctype_of_struct_cyclic_alias_no_overflow` tests, and
  `interp::ctype_of`/`ctype_of_visiting` + the `extern_aliases`/`extern_struct_fields` fields + their
  gather loops + parity-twin tests. The two `.or_else(ctype_of…)`/`None => ctype_of(…)` fallback arms are
  gone; both backends now read `extern_sigs` (the checker's `ExternTable`) **verbatim**. The standalone
  single-file paths (`compile_module_standalone`, `Interp::execute`) route through a new
  `checker::resolve_extern_signatures_standalone(stmts)` (a synthetic one-module `<main>` graph
  delegating to the same `resolve_extern_signatures`), so there is now **exactly ONE** extern-type
  resolver in the codebase — drift is impossible by construction. (`compiler::struct_fields` is retained
  for `json.decode` only; it no longer feeds extern lowering.) Tests: new checker
  `resolve_extern_ctype` units (aliased-field regression repro; a named-import + qualified + nested
  struct-field case where each field's DEFINING width wins over a colliding importer alias), a VM
  3-engine `extern_qualified_return_struct_aliased_field_runs` (quot 3 / rem 2 on VM/`--serial`/
  `--parallel`), and a standalone-path `extern_standalone_source_string_struct_return_runs` guard locking
  the single-resolver wiring; all prior FFI guards (single-hop/chain/named-import/mixed → 300, plain
  struct → 3/2, width param → 7, cyclic → clean error, non-marshallable → clean check error) stay green.
  Full suite (2290) + conformance green, clippy `--all-targets` clean; CLI struct-aliased-field repro
  20×/`--parallel` deterministic at 3/2.
- ✅ **C-ABI FFI ROOT fix: module-qualified width-alias CHAIN resolves module-scoped at ALL depths**
  (2026-06-20, `auto-task/ffi-qualified-type-fix2`; **superseded by fix4 above** — the backend
  re-resolvers it added are now deleted) — the deeper adversarial find on the single-hop
  fix above: it only resolved the FIRST hop in the defining module's scope. A CHAINED qualified alias
  (`type Len = Inner; type Inner = int64` in `core/w3`) returned w3's RAW ONE-HOP body (`Named("Inner")`)
  and handed it to `ctype_of`, which resolved the INNER name `Inner` through the flat, last-write-wins,
  **bare-keyed** `aliases` map — so a colliding `type Inner = int8` in the CALLING module hijacked the
  inner hop. `check` was correct (the checker fully resolves the chain in the defining module's scope),
  but `run`/`--serial`/`--parallel` all printed `44` instead of `300`; the same fault held at depth 3+.
  Fix: a new `resolve_qualified_alias(tidx, name, …)` helper in BOTH engines follows the WHOLE chain
  in its defining module's scope (each inner bare `Named(inner)` is interpreted as `tidx`'s `inner` via
  `module_aliases`/`type_keys`), so NO hop ever re-enters the flat bare `aliases` map; it returns a
  scalar/FFI-width LEAF or a struct identity key, never a re-entrant alias name. The qualified-alias arm
  in `qualify_ffi_type` (compiler) / the `qualify` closure (interp) now calls it. Bounded by a visited
  `(module_idx, name)` set: a cyclic alias (`type A = B; type B = A`) ⇒ `None` ⇒ `ctype_of`'s clean
  "not C-marshallable" error — no hang, no stack overflow, never a silent wrong width. A cross-module
  qualified body mid-chain (`type Len = other.X` declared inside the defining module) is the one
  remaining `None` case (it needs that module's own import-binder map, not threaded here) — a clean
  error, not the bare-`Named`-chain family this closes. Both engines kept byte-identical in logic
  (two-engine parity). Tests: new VM 3-engine parity tests at depth 2 AND depth 3 with colliding inner
  alias names across modules (`abs(-300)`=300 on VM/`--serial`/`--parallel`) plus a cyclic-alias
  clean-error/no-hang test; the single-hop collision (→300), non-colliding width (→7), struct (→3/2),
  and clean-error guards stay green; full suite (2283) + conformance green, clippy `--all-targets`
  clean. Docs: `ffi-and-packaging.md`, this file.
- ✅ **C-ABI FFI follow-ups: `bool`=C `_Bool`, precise width-alias gate, redundant self-rename allowed**
  (2026-06-18, `auto-task/ffi-bool-cbool-alias-gate`) — three FFI loose ends from the prior reviews.
  (1) **`bool` now means C `_Bool` (1 byte)**, not C `int` (4 bytes): re-mapped `CType::Bool`'s libffi
  lowering in `src/native/cffi.rs` only — `ffi_type` → `Type::u8()`, param `Vec<u8>`, `write_field`/
  `read_field` 1 byte, and a `_Bool` **return reads register-width then narrows to a byte + `!= 0`** (the
  libffi rvalue-widening rule, same as the narrow-int OOB fix). `ctype_of` is unchanged in **both**
  engines (the divergence hazard doesn't apply; both call the shared `Cffi::call`), so parity holds. A
  struct `_Bool` field now has correct 1-byte size/offset — closing the prior footgun. **Behavior change:**
  a C function using the int-as-bool idiom (`isdigit`, arbitrary nonzero `int` for true) must be bound
  `-> int` and tested `!= 0`, **not** `bool`. There is **no separate `bool8` type** (the planned one is
  mooted). (2) **Closed the width-alias gate hole** (`!alias_resolving.is_empty()` relaxation in
  `resolve_type`): a `type Len = int32` whose defining module never imported `int32` no longer launders the
  bare width name. The opt-in is now **precise** — recorded in a program-global `ffi_alias_ok` set at
  alias-definition time (only when the defining module imported the width); the gate accepts a width name
  through an alias iff the innermost resolving alias is licensed. (3) **Allow the redundant identical
  self-rename** `import int32 as int32` (was rejected "cannot be renamed"): the guard now fires only when
  the as-name differs from the member — a true rename (`as W`) or wrong-width trap (`int8 as int32`) still
  rejects. Tests: `cffi.rs` `bool_marshals_as_one_byte_cbool` + `struct_bool_field_marshals_one_byte`;
  `checker/tests.rs` `width_alias_without_any_import_rejected` + `width_alias_defined_with_import_resolves_in_extern`
  + `width_import_redundant_self_rename_ok` (all RED-first). Docs: `syntax.md` §12b, `spec.md` §Level-3,
  `ffi-and-packaging.md` §1b (supersedes the `bool8` note). Two-engine parity green on the FFI examples.

- ✅ **C-ABI FFI structs by value (flat scalar fields)** (2026-06-18, `auto-task/ffi-struct-by-value`)
  — an extern fn can take and/or return a C struct **by value** (not by pointer): name a Chezzi `struct`
  as a param/return type and its fields marshal in declaration order into a C-ABI struct layout. New
  `CType::Struct{name, field_names, fields}` in `src/native/cffi.rs` carries **only owned data** (no
  libffi `Type`, which is `!Send`/`!Sync`/`!Clone`) — the libffi structure type + per-field offsets are
  rebuilt per call via `ffi_get_struct_offsets` (platform ABI — small-struct-in-registers vs by-hidden-
  pointer — is libffi's, never hand-rolled), keeping `Cffi` `Send + Sync` for `--parallel`/M:N (made
  `CType` non-`Copy`; by-ref matching). A struct **param** writes its fields into a per-arg buffer at the
  libffi offsets (reusing the scalar `as`-casts incl. the fixed-width widths) via a new
  `Host::arg_struct_fields`; a struct **return** drops to the raw `ffi_call` with an own rvalue buffer
  sized `max(struct_size, sizeof(ffi_arg))` (the register-width floor from the narrow-int-return fix) and
  reads each field at its libffi offset into a `NativeRet::Struct` both engines already lower. `ctype_of`
  (compiler + interp, byte-identical) maps a struct `Named` to `CType::Struct` recursively with a shared
  visited-set (cyclic alias/struct ⇒ `None`, no overflow); interp pre-gathers a program-global
  `extern_struct_fields` like `extern_aliases`. **v1 = flat scalar fields only** — the checker rejects a
  struct with a `str`/nested-struct field (error naming the struct + field) and a generic struct; a
  `type P = Point` alias works like the bare struct. Golden `examples/ffi_struct.chz` binds
  `div_t div(int, int)` (pure libc; `{3, 2}`, byte-identical VM/`--interp`/`--parallel`); cffi round-trip
  unit tests (struct return + mixed long/double/long + fixed-width-field layout), checker + ctype_of
  parity tests. Docs: `syntax.md` §12b, `spec.md` §Level-3, `grammar.bnf`, `ffi-and-packaging.md`. Nested
  structs / `str` struct fields stay deferred.
- ✅ **C-ABI FFI width type names moved to `std.ffi` type imports** (2026-06-18,
  `auto-task/ffi-width-type-imports`) — the eight fixed-width integer TYPE names (`int8`..`uint64`) are
  **no longer global builtins**: they are now **imported per-name from `std.ffi`** (`import int32, uint32
  from std.ffi`) — **Chezzi's first type import**. `native::ffi::TYPE_NAMES` is the single declaring
  authority; `std.ffi`'s `ModuleSig.types` carries them, `bind_import` records each into a per-module
  `imported_ffi_types` set, and `resolve_type` maps a width name to `Ty::Int` **only** in a module that
  imported it (else *unknown type 'int32' (import it from std.ffi …)*). A bogus `import int99 from
  std.ffi` errors like any bad import. Both runtime engines' `from`-import binders **skip** the value-less
  width imports (parity by construction). Per-module: A's int32 struct field is usable from B with no B
  import; a width name written in B's own source needs B's import. **No runtime/marshalling change** —
  `cffi.rs` `CType` + both `ctype_of` untouched, the same C calls run, goldens byte-identical. FFI-special
  + minimal: NOT a general user type-export mechanism; `ptr`/`owned_str` stay bare builtins (NOTE:
  later superseded for `ptr` — see "task 2/5: FFI `ptr` gated behind `import std.ffi`" above; `ptr` now
  requires the import too, `owned_str` stays bare). Five new
  checker tests (no-import-rejected, import-then-extern+struct-ok, bogus-import, cross-module isolation
  ±), three existing FFI checker tests converted to `entry_ok` + import line, both goldens
  (`examples/ffi_int.chz` + `ffi_struct.chz`) gained the import line (`.expected` unchanged). 2202 tests
  green. Docs: `syntax.md` §FFI + §std.ffi, `spec.md` §Level-3, `PROGRESS.md`.
- ✅ **C-ABI FFI fixed-width integers — `int8`..`uint64`** (2026-06-18, `auto-task/ffi-fixed-width-ints`)
  — eight bidirectional integer marshalling type names (`int8`/`int16`/`int32`/`int64`/`uint8`/`uint16`/
  `uint32`/`uint64`) on the `extern "lib":` surface (later moved to per-name `std.ffi` type imports — see
  the entry above; **zero grammar/lexer/parser change**). Resolves the FFI-2 known
  limit (prior: *"scalars only — int ↔ long, no fixed-width int type"*). Each resolves to a plain `int`
  (`Ty::Int`) for the program; the width/signedness is a runtime-only marshalling distinction the backends
  recover via `ctype_of` (the platform-exact libffi `Type::i8()`/`u8()`/…/`i64()`/`u64()`; bare `int`
  keeps `c_long()` for back-compat). Unlike `owned_str` (return-only), these are **bidirectional**. C-cast
  boundary semantics, **no overflow trap**: a param **truncates** the Chezzi i64 to the C width (wrapping
  — `255` → `int8` is `-1`); a return **sign-extends** (signed) or **zero-extends** (unsigned) back to i64
  (`int32` `-1` → `-1`; `uint32` `0xFFFFFFFF` → `4294967295`). `uint64` above `i64::MAX` wraps negative
  (documented limit). Alias-safe: `type Len = int32` marshals as the int32 width (the alias resolves one
  hop into the leaf, placed before the alias fallthrough), and a cyclic alias still errors at the checker
  (no stack overflow). Eight flat `CType` variants + `ffi_type()`/param-cast/return-lower arms in the
  shared `Cffi::call()` (parity by construction); the two `ctype_of` sites (compiler + interp) mirror
  verbatim, guarded by twin tests. No C-spelling aliases (`c_int`) yet — width is platform-dependent,
  deferred. Five MockHost unit tests (round-trip, int8 truncation, sign-extend, unsigned zero-extend +
  high-bit), three checker tests (param+return for all 8, alias, cyclic-alias), twin `ctype_of` tests,
  golden `examples/ffi_int.chz` (atoi/htonl/abs) through both engines. ~2181 tests green.
- ✅ **C-ABI FFI `str`-return deepening — `owned_str` + `str?`** (2026-06-18, `auto-task/ffi-str-return`)
  — two paired, return-only opt-ins on the `extern "lib":` `char*` return path, implemented as **pure
  type-machinery (zero grammar/parser change)** — both ride a `Type` the backends' `ctype_of` recognizes,
  exactly like `ptr`. **(1) `owned_str`** (fixes the FFI-3 leak): a return-only marshalling type name
  (resolves to a plain `str` for the program) whose `char*` is copied into a Chezzi str **and then freed**
  with libc `free` (resolved once via `dlsym("free")` at `Cffi::new`, cached as a `usize`; best-effort —
  degrades to the old leak if unresolvable, never aborts). NULL still faults. **(2) `str?`** (`Option[str]`,
  already parses): a nullable `char*` — `NULL` → `None`, non-null → `Some(str)` — the opt-in escape from
  the non-null `str` faulting-on-NULL rule (kept byte-identical). Composes: `owned_str?` → nullable + owned.
  Three flat `CType` variants (`OwnedStr`/`OptStr`/`OptOwnedStr`), each `Type::pointer()` to libffi; both
  are **return-only** (a surface guard in the extern param loop + `assert_marshallable` reject them as
  params). Parity by construction (shared `Cffi`, `NativeRet::Some/None` already lower identically); the two
  `ctype_of` sites (compiler + interp) mirror verbatim. Golden `examples/ffi_str.chz` (strdup + getenv,
  byte-identical VM/`--interp`/`--parallel`); 4 cffi unit tests, 5 checker tests, 1 ctype_of test, 2 goldens.
  **Limits:** libc `free` only (a custom user-named deallocator stays deferred); `owned_str` is a user
  assertion the buffer is genuinely `malloc`'d (a static-string mis-declaration corrupts the heap). Docs:
  `syntax.md` §12b, `spec.md` §Level-3 (FFI-3 resolved), this file. `cargo test`/conformance green, clippy clean.
- ✅ **Comprehension nested clauses** (2026-06-17, `auto-task/comprehension-nested-clauses`) — a
  comprehension may now have 2+ `for` clauses (cartesian/nested iteration, first clause outermost,
  later clauses see earlier clauses' bindings), with one or more `if` guards allowed after ANY clause,
  across list/set/map forms (Python semantics). The `Comprehension` AST node now carries
  `clauses: Vec<CompClause>` (each `{ vars, iter, guards }`). VM folds the clauses right-to-left into
  nested `compile_for`s (reusing the for-loop lowering verbatim — no new bytecode); interp recurses
  left-to-right (`eval_comp_clauses`) for byte-identical iteration order + guard placement. Checker
  scopes progressively (per-clause `for_bindings`/`declare`, channel-drain rejection per clause).
  Grammar gains `<compClauses>`/`<compGuards>` (conformance green). `examples/comprehensions_nested.chz`
  + 5 cases asserted byte-identical on VM/`--serial`/`--interp`.
- ✅ **Comprehension stateful-iterator parity fix** (2026-06-17, same branch) — the interp now drives
  a comprehension's iterable LAZILY (`eval_comp_clauses` pulls one element, binds it, tests guards,
  then recurses/collects, then pulls the next), reusing the same per-element struct-`next()` loop as
  the `for` statement and the VM's `compile_for`. Previously it eagerly drained the iterator into a
  `Vec` first (via `collect_iter_rows`, now removed), so a comprehension whose element/guard read a
  stateful struct iterator's live field (`[x*100 + c.n for x in c]`) saw the fully-advanced state on
  the interp but the per-step state on the VM — a real two-engine divergence. This was **pre-existing
  for the single-clause form on `main`** (same eager `collect_iter_rows`); the nested form inherited
  it. List/map/set/str/range iterables are stateless, so their order/semantics are unchanged.
  `examples/comprehension_iter_state.chz` + interp/VM/golden parity tests.
- ✅ **`ref T` — transparent by-reference bindings** (2026-06-17) — a binding MODIFIER (locals + params
  only) that lowers to the existing `std.ref` `Ref[T]` box, **entirely in parser → checker → desugar**
  (no new runtime/VM op, so two-engine parity is by construction — all read/write/init lowering lives in
  `src/desugar/mod.rs`, run inside `resolver::build_graph`, which both engines + the checker consume).
  AUTO-DEREF (the user-approved design — no `^` operator, no call-site `ref` marker): a read `r` lowers
  to `r.get()`, `r = v` to `r.set(v)`, `r += 1` to `r.set(r.get()+1)`; init creates a fresh `Ref(v)` or
  ALIASES the same box when the RHS is already a `ref` binding. Coercion table enforced: `ref→ref` param
  aliases the box, `ref→T` param auto-derefs to a copy, a by-value local or a literal into a `ref` param
  is an error. `ref` is barred (parse error) from return types, generic args, collection elements, tuple
  elements, struct fields, and destructuring bindings; a `ref`-over-generic-param is a type error. Concurrency:
  a `ref T` is a `Ref[T]` → non-sendable, so crossing the airlock is rejected (matches `Ref[T]`; use
  `Shared[T]`). `ref` is now a keyword (corpus-safe; `import std.ref` paths still parse via a path-segment
  exception). Goldens `examples/ref_binding.chz` + `examples/ref_airlock.chz` (byte-identical on
  run/--serial/--interp); parser/desugar/checker unit tests + grammar.bnf REF terminal + corpus
  accept/reject fixtures. Docs: `docs/reference/syntax.md` §3, `gaps.md` (RESOLVED), `docs/design/roadmap.md` (item 12
  landed), `docs/reference/concurrency.md`. `cargo test` green (2052+), `cargo test conformance` green, clippy clean.
- ✅ **`ref T` arg coercion is type-directed (indirect callees + closures + protocols)** (2026-06-17) —
  follow-up hardening the `ref` arg alias/deref/error decision so it follows the *resolved* callee, not a
  purely-syntactic name lookup. The decision still lives in `src/desugar/mod.rs` (it must — desugar runs
  inside `build_graph`, the one pass the checker and both engines share), but `callee_param_is_ref` now
  resolves indirect callees through local binding tracking: a LOCAL fn-value (`g := bump`/closure literal
  → `local_fn` flags) and a method call whose receiver's struct type is known locally (`x := S(...)` /
  `x: S = ...` → `local_struct`, looked up in a new `(struct, method)`-keyed spec map). Fixes (1) calling
  a `ref`-fn through a local fn-value (was a false `expected Ref[int], found int`), (2) a method name
  shared by structs that disagree on ref-ness (resolved by receiver type), (3) **closure `ref` params**
  (were silently inert) — now `bind_ref`'d in desugar and typed `Ref[T]` in `infer_closure`, so a `ref`
  arg aliases and a by-value arg is the same row-3 error as a named fn. (4) **Protocol `ref` params** are
  now honored (`Ref[T]`) in the protocol method sig so a conforming `ref` method matches. (5) Diagnostics
  for `ref` bindings render the `ref T` surface the user wrote (`ty::ref_display`), never leaking the
  lowered `Ref[T]`. Golden `examples/ref_indirect.chz` (byte-identical run/--serial/--interp); 13 new
  parser/desugar/checker tests. Known boundary: a method whose receiver's struct type is NOT statically
  known locally (e.g. `foo().apply(r)`) still resolves only when all same-named methods agree on ref-ness
  — otherwise it falls back to deref (the checker then gives a transparent `ref T` error). Docs:
  `docs/reference/syntax.md` §3. `cargo test` green (2068), conformance green, clippy clean.
- ✅ **C-ABI opaque `ptr` handle for `extern "lib":`** (2026-06-18) — the first half of the FFI
  handle-unlock: a C library built around a `void*` handle (`FILE*`/`sqlite3*`/`create→use→destroy`)
  can now be driven over a dlopen'd `.so` with **no chezzi recompile**. New builtin opaque type `ptr`
  (↔ C `void*`), threaded through the whole pipeline: `CType::Ptr` marshalling in `src/native/cffi.rs`
  (arg + return; NULL return ⇒ `Ptr(0)`, **not** a fault, unlike `str`), `NativeRet::Ptr` +
  `Host::arg_ptr` in the seam, `Obj::Ptr(usize)`/`Value::Ptr(usize)` on both engines (GC leaf, no
  Drop, value-compared by address, `<ptr null>`/`<ptr>` stringify — **never** the raw address, which is
  non-deterministic across engines), sendable by value (`WireValue::Ptr`, fast-path snapshot),
  `Ty::Ptr` in the checker (marshallable + sendable; `ptr==ptr` only, no methods/fields/arithmetic).
  New **`std.ffi`** native module (`null() -> ptr`, `is_null(p) -> bool`) — the C value vocab lives in
  the library, not the language (no new keyword/literal). **Decisions:** untyped handles (one `ptr` for
  all — ctypes-level, C-UB on mismatch) + **manual free** (no auto-Drop → parity-clean; leaks if you
  forget, like FFI-3) + allow-NULL. Golden `examples/ffi_ptr.chz` (byte-identical VM/`--interp`, uses
  `/dev/null` + a bad path so it needs no writable fs); cffi unit tests (tmpfile/fclose round-trip,
  NULL-non-fault), checker tests, `std.ffi` unit tests. Docs: `syntax.md` §12b + stdlib, `spec.md`
  §Level-3, `ffi-and-packaging.md` (C half shipped; Rust `Arc<dyn Any>` userdata still forward-design).
  The Rust compiled-in handle (Burn) + registry stay deferred. `cargo test`/conformance/clippy green.
- ✅ **Checker control-flow boundary for `spawn:`/`defer:` blocks** (2026-06-16) — fixes a three-way
  divergence where `break`/`continue` lexically nested in an enclosing loop but placed inside a `spawn:`
  or `defer:` block passed `check`, raised `break outside loop` at runtime on the VM, and was silently
  treated as a block exit by the interp. Both block arms now save-zero-restore `loop_depth` around the
  body check (mirroring `check_fn_body`/`infer_closure`), so the existing `loop_depth == 0` guard rejects
  at check time with the uniform diagnostic; a legitimate loop INSIDE the block stays legal. Checker-only
  (no VM/interp/compiler edits); two-engine parity restored (runtime paths now unreachable from checked
  source). 4 rejection + 3 positive-guard tests in `src/checker/tests.rs`.
- ✅ **Adversarial-review remediation — `wait`/timer + C-ABI FFI** (2026-06-13, merges `b697ce0` (wait) +
  `e9dc3c1` (ffi)) — fixes the 8 findings from an adversarial review of the freshly-merged `wait`/`select`
  and FFI features, run as two file-disjoint auto-task worktrees (post-merge-gated, both `ship`; 1801 tests).
  **WAIT (vm only):** the `--parallel` `wait` lost-wakeup — a live `timer(N)` arm + live channel arm with
  nothing ready inline-`thread::sleep`d the worker and unconditionally took the timer, stranding a sibling
  `send` that landed mid-window (HIGH) and pinning the OS worker (MEDIUM). Fix = **full timed-park**: arm one
  background `timer::submit_at(deadline, send_wake(true))` on the soonest timer arm's own channel and fall
  through to the existing snapshot-park, so the `WaitPark` claimed-CAS sweep picks exactly one of {a sibling
  send/close, the timer's deadline send}; demote path (`native_reentry>0`) threads the deadline into the
  bounded poll. An **arm-once `ChannelCore.timer_armed` CAS latch** stops a re-park (woken by a `close` with
  no value) re-arming a redundant job (adversarial low finding). Cooperative VM + interp inline-sleep
  unchanged (parity oracle, `--parallel`-only + licensed-nondeterministic; 5 new VM tests, 600-race stress).
  **FFI (checker/parser/native/docs):** reject an `extern fn` colliding with a builtin/`print`/constructor
  or a struct/variant name (was silently shadowed → dead extern + startup `dlsym` abort) — order-independent,
  and corrected to NOT reject enum *type* names (not callable, so reachable; adversarial fix); reject
  non-top-level `extern` at the parser + grammar (was skipping marshallability validation); gate `cffi`
  `#[cfg(unix)]` (LLP64 `c_long` truncation now unreachable; project is unix-only); documented v1 limits
  (int↔C `long` width, malloc'd `char*` leak, non-reentrant C under `--parallel`).
- ✅ **Level-3 dynamic C-ABI FFI (v1)** (2026-06-13, `feat/c-abi-ffi`) — reverses the documented
  non-goal. New `extern "lib":` indentation block of statically-typed C signatures (`Token::Extern` →
  `StmtKind::Extern{lib, fns}` → `parse_extern` mirroring `parse_protocol`; grammar `<externDecl>` +
  conformance corpus). New `src/native/cffi.rs` holds `Cffi` (`dlopen`'d `Library` + symbol as `usize`
  + per-call `Cif`) whose `call(&mut dyn Host)` reuses the **same** `Host`/`NativeRet` seam as the std
  modules, so VM + interp + `--parallel` emit identical output (structural parity). `extern` fns are
  module globals (`vm::Obj::Cffi(Arc<Cffi>)` via `Op::MakeCffi`/`CffiDef`; `interp::Value::Cffi`), so
  the normal call-dispatch + `infer_named_call` type-check paths work with zero call-site special-casing.
  Checker enforces C-marshallability (int/float/bool/str + void) on the **resolved** type (aliases OK).
  `Cffi` is `Send+Sync` (symbol as `usize`, `Cif` rebuilt per call — both libloading `Symbol`/libffi
  `Cif` are `!Send`); the M:N snapshot path shares the `Arc<Cffi>` (same address space, no re-dlopen).
  v1 = scalars only (structs/callbacks/varargs/userdata/`char*`-ownership deferred); extern stays OUT
  of `is_blocking` (a slow C call runs inline). Golden `examples/ffi.chz` (cos/sqrt/strlen) two-engine
  parity-tested + `cargo test cffi/conformance/golden_ffi` green; +`libffi`/`libloading` deps.
  **Post-review blocker fixes** (merge `0a5938d`, after adversarial reject): (1) `nil` is now a
  return-only type — rejected as a param (the backend's `ctype_of` has no nil case, so accepting it
  panicked every engine on a *checked* program); (2) compiler + interp now resolve type aliases
  **program-globally** (matching the checker), so a cross-module alias used bare in an `extern` sig no
  longer panics / silently-voids the return — backends use `and_then` (None ⇒ void) not `.expect`;
  (3) a `str`-declared return that comes back `NULL` now **faults** instead of silently yielding `nil`
  (was a static non-null-`str` soundness hole). +5 regression tests (checker nil-param, vm+interp
  cross-module-alias + explicit-`-> nil`-return, cffi NULL-str-fault). Merged over `wait_select`
  (2 union conflicts: `<compoundStmt>` grammar + compiler imports); re-verified on merged HEAD —
  **1790 pass, conformance 7, clippy clean**; post-merge-gate verdict **ship**.
- ✅ **Match or-patterns + nested nullary variants** (2026-06-13) — one new AST `Pattern::Or(Vec<Pattern>)`,
  no new opcodes. `p1 | p2 | ...` at the top of an arm AND in sub-positions (`(1|2, x)`, `Some(a|b)`);
  every alternative must bind the same variables (checker-enforced, clear error otherwise); a full enum
  or-pattern is exhaustive without `_`, but the open int/str/bool domains (incl. `true | false`) still
  need a `_` (one rule preserved). Nested nullary variants (`Some(None)`, `Ok(Err(e))`) are now refutable
  variant matches — checker promotes a bare nested capitalized ident via the variant registry; compiler +
  interp route by the same registry so all three engines agree (golden `examples/match_or.chz` byte-
  identical on VM / `--interp` / `--parallel`). Grammar `<pattern> ::= <patternPrimary> ("|" ...)*`;
  `cargo test conformance` green.
- ✅ **D6c — per-socket read/accept/write timeout** (`--parallel`) — `read(n, timeout_ms)` /
  `write(s, timeout_ms)` / `accept(timeout_ms)` → `Err("timeout")`; reuses the deadline-bounded poll, no
  new thread/heap/job. In-callback (Path-C) timeout out of scope v1.
- ✅ **D6a/D6b — netpoller + non-blocking `std.net`** — epoll/kqueue poll thread (`src/vm/poller.rs`)
  turns a would-block socket op into a fiber-park; `Obj::Socket`/`Obj::Listener` over `Arc` cores; true
  non-blocking `connect` (`socket2`); drain-on-fault re-injects socket-parked fibers; timer folded into
  the poll thread. Echo server services 100 conns ≫ workers in one `parallel:`.
- ✅ **D5 — dirty/blocking pool** (+ owes #1–#3) — a blocking off-heap-safe native suspends the fiber and
  hands the call to a growable pool instead of pinning a core worker; process-wide timer thread for
  `sleep_ms`; `request`/`process` classified blocking; `iter.*` HOFs (chezzi source) let a `recv` in a
  callback park; **Path C** demotes the worker (one raw replacement thread) for a `recv`/`sleep`/socket op
  reached inside a native callback. Residual #2 (executor-spanning demote) WON'T FIX by design.
- ✅ **D4 (a–e) — Go-style work-stealing** — per-worker local run queues (`LocalQ`) + shared global
  overflow + random-victim steal-half + periodic global check; runnable-gated park wake (the mutex *is*
  the StoreLoad barrier — no Go fence). The conditioned single-wake (`notify_one`) is a deferred
  throughput-only refinement.
- ✅ **D3 — reduction-counting preemption** (BEAM-style) — a fiber's `reds` budget yields at exhaustion to
  the run-queue tail, so a CPU-bound fiber can't starve siblings; the yield unwinds every nested
  `run_until` level via a `paused()` helper.
- ✅ **D2a/D2b — M:N scheduler** — lightweight share-nothing fibers (own heap in a swappable `FiberCtx`)
  multiplexed over the bounded pool, **parking on `recv` instead of pinning OS threads**; exact
  single-coordinator deadlock predicate; the inline join shell alone guarantees completion (decision B).
- ✅ **D1 — lazy module snapshot** — a shared read-only `Arc<ModuleSnapshot>` faulted into each worker
  heap on first access, killing the per-task module-graph rebuild.
- ✅ **D0 — O(N²)→O(N·logN) cooperative ready-queue** — per-nursery `ready` set + parked-index buckets,
  keyed by `ChannelCore` pointer; 50k fibers: seconds → tens of ms.
- ✅ **Per-connection `spawn`** — eager injectable nursery so a nested `parallel:` `spawn` runs
  concurrently with the rest of the body (the canonical accept-loop server shape). v1: ≥2 cores, bounded
  accept loops.
- ✅ **`Channel.close()` + `try_send` + `for v in ch:`** — clean producer→consumer termination, closed-
  channel fault semantics, channel-iteration (both engines); comprehension-over-channel checker-rejected.
- ✅ **Pending-`spawn`-drop on early `parallel:` escape** — unstarted tasks cancel-and-report on
  `?`/`return`/`break`/`continue` before the join (both engines, parity-restored).
- ✅ **B3.6 — `Executor` on the pool + A3b `submit`-capture gate** — submitted closure crosses by value
  on BOTH engines (`WireValue::Closure` via `wire_callable`; the cooperative by-handle path was retired
  once the `interp` oracle was removed — see "Executor.submit coop==M:N by value").
- ✅ **B3.4/B3.5 — cancellation + cross-thread `os.exit` + thread deadlock detection** — per-nursery
  `cancel` flag (first fault/exit trips it; `os.exit` wins; cancel bypasses `recover:` but runs `defer`s).
  Single-level cancel only (nested propagation deferred).
- ✅ **B3.3 (a–d) — `str`-by-value + G1 module-globals checker gate + worker module-graph reconstruction +
  real OS threads behind `--parallel`** — mutating a `spawn`-reachable module global is a checker error
  ("use Shared[T]"); bounded pool, parent participates inline.
- ✅ **B3.0–B3.2 — `WireValue` airlock + cores into `Arc<…Core>` + `Arc<Program>` + isolated worker VMs**
  — `deep_clone` → wire round-trip; `Channel`/`Shared`/`Executor` cores out of the heap; cross-heap safety
  enforced (`ensure_crossable`). All single-thread, byte-identical. See `docs/history/designs/concurrency-b3.md`.
- ✅ **Concurrency A1 — `Channel.try_recv() -> T?`** — non-blocking poll (both engines), un-deferred once
  B1/B2 landed.
- ✅ **Concurrency C5 / Group B — B1 + B2 cooperative fibers + blocking `recv`** (VM) — suspendable
  execution: a `recv` on an empty channel parks the fiber and the nursery-local scheduler runs a sibling.
- ✅ **Concurrency C5 — `Executor` escape hatch** + **A2 program-exit auto-drain** + **A3a** (pinned) — the
  sequential-subset `Executor()` / `submit` / `shutdown[_now]`, drained at clean exit (both engines).
- ✅ **Concurrency C4 — VM parity for `spawn`/`parallel:`/`Channel`/`Shared`** — ported C1–C3 onto the
  default bytecode engine (heap objs, ops, VM `deep_clone`, sequential nursery executor).
- ✅ **Concurrency C3 — `Shared[T]`** (interp) — cross-task mutable box (`get`/`set`/`update`); handle
  sendable, `Ref[T]` forced non-sendable.
- ✅ **Concurrency C2 — `Channel[T]` + sendability** (interp) — buffered FIFO mailbox; a `sendable(Ty)`
  predicate gates element types, `spawn` args, and capture reassignment.
- ✅ **Concurrency C1 — `spawn` / `parallel:` nursery** (interp, sequential executor) — structured
  concurrency; `spawn f(x)` and `spawn:` block run to completion FIFO at the dedent.
- ✅ **Integer overflow policy** — every `i64` overflow is a recoverable fault (never wrap/crash).
- ✅ **Gaps pass II** — `Ref[T]` mutable box (`std/ref.chz`); `sort_by_key`; call fn-typed field
  `self.f(x)`; relaxed non-const defaults; runtime stack traces (both engines).
- ✅ **String format specifiers** (6th/last of the f-string ergonomics batch) — Python-style
  `{expr:[[fill]align][sign][0][width][.precision][type]}` after a `:` in interpolation. Type chars
  `d f x X b o e %`; string `.N` truncates. **Width/precision capped at 4096 at parse time** (fixes a
  prior OOM from unbounded `repeat`). Spec parse+format is a single shared module `src/fmtspec.rs`
  (`split_spec`/`parse`/`apply` + neutral `FmtArg`) routed through BOTH engines (`Op::ToStrFmt` in the
  VM, `interp::interpolate`) → byte-identical output. `:`-split is bracket/quote-aware (`{m["a:b"]}`,
  slices). Unknown type char = compile error; type/value mismatch = runtime error (same message both
  engines). Golden `examples/format_specs.chz` parity-checked VM/interp/--parallel.
- ✅ **Scripting-ergonomics gap pass** — hex/bin/oct literals; list `.concat`/`.extend` + map
  `.merge`/`.update`; tuple-destructuring `for` + `enumerate`/`zip`; `?.` + `??`; tuple destructuring +
  match-on-tuple + guards.
- ✅ **Fix — loop variable is immutable** — checker rejects assigning a `for`-loop var (was a VM/interp
  divergence); inner `:=` shadow stays mutable.
- ✅ **M18 — `defer` → block/lexical scope** — runs when its enclosing block exits on every path, LIFO,
  inner-block-first. Supersedes M17.
- ✅ **M17 — `defer` (Go-style, frame-scoped)** — runs at frame exit, LIFO; receiver+args evaluated at the
  `defer` statement.
- ✅ **M16 — comprehensions + `std.os.exit(code)`** — `[e for x in it if g]` (+ set/map forms),
  first-class AST node; hard uncatchable cooperative exit.
- ✅ **M15 — slicing + `Index`/`IndexSet`/`Slice` protocols** — **Python-style** `xs[a:b:c]` (open bounds,
  step, reverse `[::-1]`, bounds-clamped) + **negative indexing** `xs[-1]` (plain index faults out of range,
  slice bounds clamp — Python's asymmetry); the `..` operator stays the for-loop/match range. list/map/str
  intrinsic, user structs structural via `slice(self, start: int?=None, end: int?=None, step: int?=None)`.
  (Originally shipped as Rust-range `xs[a..b]`; migrated to colon syntax — see "Slice syntax → Python colon"
  below.)
- ✅ **M14 — method-level type params** · user-defined parameterized protocols · default + named args on
  methods (desugar-pass).
- ✅ **Default + named arguments** — free fns + struct ctors; scope-aware desugar pass, both engines
  consume a normalized AST.
- ✅ **Tech-debt sweep** — reject dup generic param `[T, T]`; nested `set` equality parity; explicit
  call-site type args `name[T,…](…)`.
- ✅ **M11 — panic recovery + Go-style errors** — 2-param `Result[T, E]` (`T!`/`T!E`), `Error` protocol,
  `recover:` boundary catching any transitive runtime fault.
- ✅ **M10 — type-system depth** — `Stringable`/`Hashable`, per-operator `Add`/`Sub`/`Mul` protocols,
  multi-bound `T: A + B`, transparent aliases, generic enums; `map`/`set` reworked into insertion-ordered
  hash tables.
- ✅ **M9 — Tier-2 stdlib** — `std.regex` (`regex` crate) + `std.request` (`ureq`+rustls, blocking).
- ✅ **M8 — Tier-1 stdlib** — iterable strings + `chars()`; `std.json` (pure-Chezzi + `decode[T]`); native
  `std.process`/`std.fs`/`std.time`; `set` type.
- ✅ **M7 — generics + structural protocols** — type-erased generic fns/structs, Go-style `protocol`s,
  `Comparable`; `std.cmp`; `list.sort()` widened.
- ✅ **Round 2 gaps #10–#15** — `sort_by`, `ord`/`chr`, int+float math, map `for`, nested/tuple match,
  bitwise ops; iterator protocol (`next()`), `Iterator[T]` bound + lazy adapters, match guards +
  half-open range patterns.
- ✅ **Tuples + multiple return + destructuring (gap #8)** — `(e1, e2, …)`, tuple types, `a, b := f()`,
  `.0`/`.1`; immutable, fixed-arity, GC-traced.
- ✅ **M6a/b/c** — core-type str/list methods; pipe `|>` (parse-time desugar); stdlib via the Level-2
  native FFI seam (`std.math`/`std.io`/`std.os` native, `std.str` pure Chezzi).
- ✅ **`Map[K, V]` dictionary (gap #5)** — literals, keyed read/insert/update, six methods, GC-traced.
- ✅ **Index & field assignment** — `xs[i] = v`, `p.x = v`, `+=`/`-=` in place (both engines).
- ✅ **M5a/b/c** — bytecode compiler + stack VM; hand-built mark-sweep GC; cross-engine parity + perf;
  CLI default flip to the VM (`--interp` for the tree-walker). `read_file` capped at 64 MiB.
- ✅ **M4.5 — modules / imports + resolver** — multi-file, `chezzi.toml` root, run-once dep order,
  cross-module home-globals, cycle detection; program-global type names.
- ✅ **M4 — type checker (local inference)** — bidirectional, no unification; return-type inference,
  `T?`/`T!` sugar, expression-valued `match`/`if`, Go-style error accumulation.
- ✅ **M3 — tree-walk interpreter** — full expr/stmt set, `?` operator, interpolation, 256 MB-stack thread
  + `MAX_CALL_DEPTH` guard.
- ✅ **M2.5 — canonical grammar + conformance** — `docs/reference/grammar.bnf` executed via the `bnf` crate,
  differential-tested vs the parser. `cargo test conformance`.
- ✅ **M2 — parser → AST** — recursive descent + Pratt; spans; depth-capped.
- ✅ **M1 — lexer** — full `examples/hello.chz` incl. Indent/Dedent; string escapes, numeric underscores.
  Shipped follow-ups: scientific-notation floats (`1e3`/`1.5e-9`/`6.022e23` — any exponent ⇒ float;
  bare `e` not half-consumed), single-quote strings (`'…'` ≡ `"…"`, same escapes & interpolation),
  unicode `\u{HEX}` escapes (1-6 hex digits, rejects surrogates/>10FFFF/malformed). Golden:
  `examples/literals.chz` (VM + interp + `.expected`).

---
