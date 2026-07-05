# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

> **Mode:** Claude implements directly — working, tested code each session (see `CLAUDE.md`).
> Full per-milestone detail lives in git history; this file is a forward-looking tracker, not a changelog.

---

## Current focus

**✅ EXACT-DUPLICATE LITERAL MATCH ARMS ERROR (2026-07-05).** `match n: 1: … 1: …` (and `"x":` twice,
`1 | 1`) is now a `duplicate match arm` error — dead code under first-match — matching the existing
enum-variant dup detection (was silently accepted, a diagnostic inconsistency). Same guard carve-out
(`1 if c:` then `1:` stays legal). Range *subsumption* (a literal inside an earlier covering range) is
still not flagged. `src/checker/pattern.rs` (the `MatchKind::Literal` arm), tests in `src/checker/tests.rs`.

**✅ `recover:` TRAILING STATEMENT-`match`/`if` IS THE BLOCK VALUE (2026-07-05).** A `recover:` block
whose TRAILING statement is a statement-form `match` (or `if`) with value-producing arms/branches was
typed `Result[nil]` and the produced value was SILENTLY DROPPED (`Ok(nil)`) — only a genuine trailing
*expression* (or the `y := match …; y` workaround) yielded `Result[<arm type>]`. **Now** a total tail
`match` (≥1 arm, every arm body ends in a value `Expr`) or `if/else` (has `else`, every branch/else body
ends in a value `Expr`) is treated as the block's value expression: its unified arm/branch type becomes
the `Result[T]` T, and `Ok(v)` wraps the real value. A non-value tail (a `let`, a non-total/`else`-less
construct, a nested-statement tail) stays `Result[nil]` byte-identically; an all-`panic` tail stays
bottom (`Result[Unknown]`, matching direct `recover: panic(…)`). Fix at BOTH stages that `split_last`
the recover block, gated on ONE shared `crate::ast` predicate (`match_tail_is_value`/`if_tail_is_value`/
`block_produces_value`) so checker + compiler can never drift on which tail is a value: checker
`infer_recover` (`src/checker/pattern.rs`) folds arm/branch trailing-expr types (dedicated
`infer_recover_tail_{match,if}`, statement-form persistent-refine, match/if typing elsewhere
untouched); compiler `compile_recover` (`src/compiler/mod.rs`) reuses `compile_match_{lit,general}` +
a value `run_body` / a value analog of `compile_if` so exactly one value converges pre-`Ok`-wrap (the
`DrainHandlerDefers`/`NewEnum Ok`/`PopHandler` tail stays byte-identical → serial == M:N). Defers inside
a value arm/branch run without clobbering the value (drain touches `frame.deferred`, not the stack). The
recover rejection rules (`return`/`break`/`?`-on-`Option`) are separate and untouched.
**Follow-up fix (heterogeneous arms → fall back to nil, not an error):** the syntactic
`match_tail_is_value` predicate is true even when arms produce genuinely *different* types (a void
`print(...)` arm mixed with an `int` arm, or `str` vs `int`) — those have no single value type. The first
cut folded them with the erroring `unify_branch`, which REGRESSED previously-valid fault-isolation
`recover:`s (`Ok(_)` consumer) into `branches have incompatible types`. Now the checker folds with a
NON-erroring `fold_recover_tail`: uniform arms → `Result[T]`; the moment two arms are incompatible it
latches non-uniform and types the block `Result[nil]` (per the design contract "do not force a value
where there isn't one"). The compiler is UNCHANGED — it still compiles the tail as a value, but because
the block is `Result[nil]` the nil-in-value-position ban makes the `Ok(v)` payload unusable in every
value context, so the heterogeneous runtime value can never be observed (no checker/runtime divergence,
no channel needed). TDD: checker `recover_tail_stmt_{match,if}_{value_*,heterogeneous_*}` + parity
`recover_tail_{stmt_match,stmt_if,match_heterogeneous,if_heterogeneous}_*` (RED-first: the pre-fix
branch binary rejected `match cmd: "a": "hello"; _: 42` with `str and int`). Docs: `docs/syntax.md`
recover section.

**✅ MULTI-BRANCH RETURN INFERENCE — JOIN merge + finalize, `Unknown`-leak fix (2026-07-05).**
Checker-only (`src/checker/sig.rs` + a closure hook in `src/checker/pattern.rs`); no runtime/VM/grammar
change, so two-engine parity + conformance hold by construction. **Before:** `infer_returns` was
first-concrete-return-wins with `Unknown`-fill — one branch's shape won and complementary type-arg slots
stayed `Ty::Unknown`, which then LEAKED as a type-check bypass (an `Err`-only fn's `Result[Unknown, str]`
let `x := err()?; y: int = x; z: str = x` all type-check). **Now:** ALL `return` branches (plus an
inline/implicit-trailing expr) are typed and folded with a join `J`: `a==b`; the one `{int,float}→float`
widen (bare scalars only, no recursion into slots); the **same** type-constructor (`Result`/`Option`/
`List`/`Map`/`Set` or same generic struct/enum/newtype) → **merge slot-wise** (`Ok("h")` ⊔ `Err("a")` =
`Result[str, str]`); otherwise a **conflict** (`cannot infer return type: conflicting branches (X vs Y)`
— NO common-supertype/protocol/`Any` search, so two distinct structs conflict, a protocol return must be
spelled). A post-fixpoint **finalize** fills the `Result` **error slot** default (`Unknown`→`Error`
protocol, matching `T!`, so `fn ok(): return Ok(5)` is `Result[int, Error]`) and REJECTS any other
residual un-inferable `Unknown` (`fn err(): return Err("x")`, `fn none(): return None`, `fn f(): return
[]` — the return-position analogue of the empty-collection diagnostic; also closes the old
baseless-recursion permissive gap). `Ty::Param` (generic fns / the proto.rs HOF loop-back) is left
untouched. Applies uniformly to free fns, struct/enum methods, AND free closures (`f := fn(): Ok(5)` →
`Result[int, Error]`; `fn(): Err("x")` rejected — gated to `expected.is_none() && !generic_arg_prepass`
so `fn`-typed slots / generic-HOF contexts are excluded). Cascade-safe: a body that already errored
suppresses the finalize diagnostic. 18 new checker tests (repro a–e + must-not-break neighbors +
closure-gated); 4 existing tests updated to the new documented semantics (int-vs-str/int-vs-str method
conflicts now say `conflicting branches`; pure self-/mutual recursion now REJECTED not permissive).
Acceptance demo: `fn res(): if …: return Err("a")` then `return Ok("h")` infers `Result[str, str]`
(byte-identical serial == M:N). Docs: `docs/syntax.md` "Return type inference", `docs/spec.md` widening
note. Full suite (3065) + conformance + clippy green.

**✅ GENERIC FN AS A VALUE — scope A + B, erased runtime (2026-07-05).** A generic function
(`fn ident[T](x: T) -> T`) is now a usable **value** once its type params are **pinned**: via an explicit
**turbofish** (`g := ident[int]` ⇒ `fn(int) -> int`, scope B) OR against a **known concrete
`fn(...) -> ...`** — an annotation (`h: fn(int) -> int = ident`), a HOF parameter (`applyit(ident, 5)`), a
return position (`fn getf() -> fn(int) -> int: return ident`), or an assignment target (scope A). Two
independent seams, both **same-module** gated so checker-accept ⟺ compiler-erase stay in lockstep:
**checker** (`src/checker/pattern.rs`) — `infer_ident` A-path (unify the declared sig against the
`expected_hint` `Ty::Func`, enforce bounds, return the substituted concrete fn — never `expected`, so an
unsatisfiable target is caught by the existing assignability check) and `infer_index` B-path (turbofish
`ident[int]` → `seed_targs` arity-check + `enforce_bounds` + `subst`); **compiler**
(`src/compiler/mod.rs`) — a tiny Index-arm erase (drop the type index for a non-shadowed top-level fn
name, load the plain fn value). Runtime is generic-**ERASED** (the value IS the underlying function), so
**serial == M:N** is automatic — but every accepted case has a both-engines RUN test (the bind-import
trap). Soundness rejects (all TDD'd, failing-then-green): unsatisfiable pin, bound violation (turbofish +
annotation), turbofish arity mismatch, downstream concrete-type misuse. Must-not-regress: direct calls,
non-generic fn values, call-site turbofish `ident[int](5)`, generic-HOF-param (closure path), and
compiler-erase shadow-safety (a local/param shadowing the fn name is a real index). New checker + parity
tests in `src/checker/tests.rs` + `src/vm/parity_tests.rs`. **Known v1 limits (deferred):** (C)
first-class / rank-N polymorphism — a bare un-pinned generic fn value (`g := ident` then `g(5)`), or one
binding used at two different types, stays a clean error (hint: turbofish or a `fn(...) -> ...`
annotation); and an **imported** generic fn used bare as a value stays the un-pinned error (same-module
only — resolves the accept⟺erase lockstep without a span side-table). Docs: `docs/syntax.md` fn-value
section. Full suite + conformance + clippy green.

**✅ CHECKER LENIENCY — five decl footguns now rejected (2026-07-05).** All checker-only,
reject-earlier in the decl-hoist pass (`src/checker/setup.rs` Struct/Enum/NewType arms) + a
cascade-suppression tweak in the pass-2 body loops (`src/checker/sig.rs`); no runtime/VM change, so
two-engine parity holds by construction. (1) **Duplicate instance method** (struct/enum/newtype) — was
silently last-wins, now `method 'f' is already defined` at its decl-site span. (2) **Duplicate struct
field** — was first-wins with a dead-but-positionally-required ctor slot, now `field 'x' is already
defined`. (3) **Field + method sharing a name** — now `'f' is declared as both a field and a method of
'P'` (mirrors the enum variant/static disjointness rule). (4) **Same-name method's confusing return-type
cascade** — the pass-2 body loops now `continue` past a duplicate-named method (`filter(count>1)`
guard), so the clear dup error is the sole diagnostic instead of the misleading "expected int, found
str". (5) **Newtype static method** (`fn zero()` — no `self`) — was unreachable → cryptic "unknown name
'Meters'", now a clear "static (associated) methods on a newtype are not supported yet (only struct and
enum have them)" at BOTH the decl site and any `Newtype.method()` call site (the feature stays deferred,
not implemented). Reuse: one `report_dup_names(iter<(name,span)>, kind)` helper (`setup.rs`) drives the
method-dup checks in all three arms + the field-dup check. Tests: 7 new negative tests in
`src/checker/tests.rs` (beside `duplicate_variant_within_one_enum_is_reported`). Full suite + conformance
+ clippy green; `all_shipped_examples_typecheck` unaffected (std/examples sweep found zero real clashes).
Docs: `docs/spec.md` M21 row + newtype note de-staled.

**✅ BUGFIX — `for`/`List()`/`Set()` over a NAMED builtin cursor now CONSUMES it in place (2026-07-04).**
A `for x in it:` (or `List(it)`/`Set(it)`) driven by a NAMED `Obj::Iter` cursor from `xs.iter()` used to
snapshot a private copy and never advance the shared cursor, while `.next()` and struct iterators DID
advance in place — so `for` had opposite semantics depending on the iterand kind, contradicting
`docs/syntax.md` ("reusing one exhausted cursor yields nothing on a second pass"). Fixed: added an
`IsCursor` opcode (mirrors `IsGenerator`); `compile_for` now routes a named/converted cursor onto the
lazy `next()` step (advances the shared heap cursor via `call.rs`), and `drain_iterable` consumes a
cursor in place (clone remaining, advance `pos` to end). Now `it := [1,2,3,4].iter(); for … break at 2;
List(it)` yields `[3, 4]`; a second `for` over the same cursor yields nothing; `next()` after a `for`
returns `None`. Invariants kept: non-cursor collections still fresh-snapshot each loop; `xs.iter().iter()`
is one fresh cursor; a fresh temp `for x in xs.iter():` still fully iterates; generators unchanged.
Serial==M:N byte-identical. (Multi-var `for a,b in named_cursor:` still snapshots — out of scope,
behavior unchanged; noted as a follow-up.) Tests: 6 new in `vm/parity_tests.rs` + golden
`examples/iterable.chz`. Docs: `docs/syntax.md` clause added.

**✅ REFACTOR — split the mega-files + REMOVED the tree-walk interpreter (2026-07-04).** Two parts:
- **File split (behavior-preserving).** `impl Vm` (one ~12.4k-line block) split across
  `vm/{exec,arith,call,sched,netio,stmt}.rs`; `impl Checker` split across
  `checker/{setup,sig,pattern,expr,proto}.rs`; the big inline `mod tests`/`gc_tests`/`parity_tests`
  moved to `vm/{tests,gc_tests,parity_tests}.rs`. `vm/mod.rs` 32,988→~3.5k, `checker/mod.rs`
  17,698→~4.5k. Mechanism: an inherent method's privacy keys off the *impl's module*, so split-out
  private methods are widened to `pub(super)` (visible throughout the parent module, still
  crate-internal). No logic changes.
- **Interpreter REMOVED.** `src/interp/` deleted; the tree-walk engine is gone. Two-engine parity is
  now **serial-VM (`parallel=false`) == M:N-VM (`parallel=true`)** — both are the same `Vm`, only the
  scheduler differs. The ~250 parity tests swapped their oracle interp→M:N (`run_capture`→
  `run_capture_parallel`, added `run_program_parallel`/`run_file_p` helpers). Tests that pinned
  *cooperative-only* semantics (Executor drain order, by-reference capture, racing spawns, GC-stress)
  dropped their now-invalid M:N cross-arm and keep their concrete cooperative expecteds; interp-only
  tests (`interp_rejects_generators`, `bench_vm_faster_than_interp`, …) were deleted. All `interp::`
  refs in prod code were only `/// Mirrors interp::X` doc-comments (no code coupling); `--serial`
  already routed to the VM. Docs updated: `CLAUDE.md`, `main.rs`, this file.

**✅ LANGUAGE — the `pass` keyword: no-op statement + empty protocol/struct bodies, and `Any` wired
into the prelude (2026-07-04).** `pass` is now a REAL reserved keyword (`Token::Pass` in the lexer
KEYWORDS table — reserved-as-a-name BY CONSTRUCTION, `expect_ident` rejects it like `ref`/`fn`), NOT
the discarded parser hack (which string-matched the identifier "pass" only in protocol bodies and
collided with `pass`-as-variable + `protocol pass:`). Three roles off the single token:
- **No-op statement (`StmtKind::Pass`):** modeled on `Break`/`Continue` — parse arm, checker no-op arm,
  compiler emits NOTHING, interp returns `Flow::Normal`, desugar/editor no-op arms. Valid in every
  statement-block position (fn/method body, if/elif/else, for/while, statement-match arm, concurrency
  blocks). A lone-`pass` fn body == a lone-`return` body (falls off end → nil). Statement-only (not
  valid in a closure / expression-match arm — those are single-expression positions; a no-op closure
  is `fn(): nil`). Two-engine byte-identical (compiles to no bytecode).
- **Empty protocol body:** `protocol Foo:` + a SOLE `pass` line = zero methods/embeds → an accept-all
  TOP type (structural ⇒ every type satisfies it). REUSES the existing empty-protocol short-circuit in
  `satisfies_args_d`; NO satisfaction change. A user empty protocol behaves byte-identically to `Any`
  (the accept-all is not keyed on the name — generalization guard test asserts Foo == Any behavior).
- **Empty struct body:** `struct S:` + a SOLE `pass` line = zero fields; `S()` ctor takes no args,
  prints `S()`, structural-equals another `S()`, and is intrinsically `Hashable` (usable as Set/Map
  key). New: a checker `satisfies_args_d` zero-field-struct `Hashable` intrinsic + a VM+interp
  `struct_hash` constant-0 path for a zero-field struct with no `hash` method (parity; `==`'s type-tag
  guard keeps distinct empty-struct types unequal despite the shared hash).
- `pass` is the SOLE-line marker only: `pass`+member and `pass pass` are parse errors (modeled as the
  body being exactly `pass NEWLINE DEDENT`, in both the hand parser and grammar.bnf `<structBody>`/
  `<protoBody>`). Empty ENUM is OUT — `pass` in an enum body is a clear parse error.
- **`Any` wired into the prelude:** `protocol Any:` + `pass` added to `std/prelude.chz` (17 reserved
  protocols now mirrored, was 16) + `Any` added to the `assert_native_protocol_shape_matches` drift
  list. `prebuilt_protocols()` stays the Rust source of truth; the prelude is the additive
  drift-guarded mirror. A USER redeclare of `Any` stays rejected (`is_reserved_protocol`); the prelude
  is exempt via the validate-and-no-op stdlib hoist. Empty protocols are NO LONGER "unparseable".
- Docs (same commit): docs/syntax.md `pass` section + Any update; docs/spec.md `pass` keyword + empty
  structs note; docs/grammar.bnf `PASS` terminal + `<passStmt>`/`<structBody>`/`<protoBody>`; corpus
  accept/reject files; 3 two-engine goldens (`examples/pass_noop`/`empty_protocol`/`empty_struct`);
  regenerated the editor TextMate grammar (pass now highlights as a keyword).

**✅ LANGUAGE — variadic parameters (`...xs: T`) + the `Any` top type, and `print` ported off its last
synthetic signature (2026-07-03).** One coherent feature in two phases.
- **`Any` top type:** an EMPTY structural protocol (zero methods) so EVERY type satisfies it — scalars
  included. Seeded in `prebuilt_protocols()` + added to `is_reserved_protocol` (now ALSO prelude-mirrored
  + drift-guarded — the `pass` keyword made empty protocol bodies expressible; see the entry above). The one real fix: `satisfies_args_d` now short-circuits `Ok` for any
  zero-embed/zero-method protocol right after the `Ty::Unknown` guard, so an empty protocol is a genuine
  top type for *every* `Ty` (before, only structs passed it — scalars fell through to `_ => Err`). NOTE:
  this generalizes to ANY user-declared empty protocol (correct semantics of an empty structural
  interface, additive). Not dynamic typing — an `Any` value carries no methods.
- **Variadic params:** `...name: T` collapses to a `List[T]` slot (Go/Swift `T...`). New `Token::DotDotDot`
  (lexer emits on a third `.`); `ast::Param.is_variadic` (runtime-inert like `is_ref`); `parse_params`
  gates it on `allow_defaults` (free fns / methods / native decls yes; closures / extern / protocol sigs
  no) and enforces ≤1 variadic, element-type-required, no-default. `FnSig.variadic: Option<usize>` +
  `fn_sig`/`harvest_native_fn_sig`/`register_native_decl` wrap the slot in `List[T]`. **Mechanism (the
  deliberate refinement over "compiler lowering + interp mirror"): the surplus positionals are collapsed
  into a synthesized `List` literal in the DESUGAR pass (`normalize_call`)** — the parity-by-construction
  seam — so the compiler AND interp need ZERO changes and VM==interp is automatic. Everything after the
  variadic is keyword-only (post-variadic param with a default = optional kw arg, without = required kw
  arg); a positional can never land in a keyword-only slot (all trailing positionals are swept). Collapse
  runs on desugar pass 1 only (idempotency — pass 2 would double-wrap). `PSpec.is_variadic` threaded
  through every spec builder. `examples/variadic.chz` golden asserted byte-identical on interp / --serial
  VM / M:N VM.
- **`print` port:** now declared `native fn print(...args: Any, sep: str = " ", end: str = "\n") -> nil`
  in `std/prelude.chz` (harvested into `native_prelude_sigs`), retiring `sig_print()` + the
  `builtin_container_sig` print special-case — the LAST synthetic Rust signature. Lowering is UNCHANGED:
  a direct `print(...)` still compiles to `Op::CallPrint`/`CallPrintSep` byte-identically (the file-backed
  decl is checker-only name/sig authority). The VALUE form (`p := print`) stays a FIXED 1-arg
  `Ty::BuiltinFn` via an `infer_ident` special-case (the specialized opcodes are unreachable through a
  bound value — a design-sanctioned split, not a gap); the existing `print` value-form / `defer` / `spawn`
  tests stay green.
- **Deferred (docs only):** `cast[T](val: Any) -> Option[T]` checked downcast — design + runtime-erasure
  policy recorded in `docs/future.md §3.14` (parameterized targets like `cast[List[int]]` unsound until
  runtime type tags exist). cFFI stays fixed-arity (`Any` does not feed the C vararg ABI — `docs/ffi-and-
  packaging.md §5`).
- **Heterogeneous args into `...xs: Any` (and `List[Any]`) — supported (the honest top-type element).**
  `fn describe(...xs: Any)` called `describe(1, "a", true)` collapses to a `List[Any]` and type-checks
  clean: every value vacuously satisfies the empty `Any` protocol. The synthesized variadic `List`
  literal (and any annotated `xs: List[Any] = [1, "a", true]`) is now checked **expected-type-directed** —
  the declared `List[E]` element type is driven onto each element (`Checker::infer_list` takes the
  `expected_hint`; when every element is assignable to `E` it types as `List[E]`, bypassing bottom-up
  sibling unification). Falls back to the bottom-up "list elements differ" diagnostic + int→float literal
  widening when `E` is NOT satisfied-by-all (so `List[int] = [1, "a"]` still errors). Golden:
  `examples/variadic.chz` (`describe(1,"a",true)` → `3`, `zs: List[Any] = [1,"a",true]` → `3`), byte-
  identical on interp / --serial / default. Tests: checker `variadic_any_accepts_heterogeneous`,
  `list_any_annotation_accepts_heterogeneous`, `list_int_annotation_still_rejects_heterogeneous`.
  (Adversarial-review fix: the earlier collapse synthesized a bare `List` literal that the checker
  inferred bottom-up, rejecting heterogeneous `Any` args — the exact opposite of `Any`'s purpose.)
- **Known v1 tradeoffs:** a heterogeneous variadic arg into a NON-top element slot (`f(1, "x")` into
  `...xs: int`) surfaces as a
  `List`-literal element-type error, not a precise per-arg message (still a compile error). A variadic fn
  used as a VALUE takes the collapsed `List[T]` slot (`g([1,2,3])` works, `g(1,2,3)` does not) — mirrors
  `print`'s fixed value form. A variadic CALL used as a parameter/field **default**
  (`fn g(x: int = sum_all(1,2,3))`) is **not** collapsed and so is a **compile error** (the desugar
  collapse runs on pass 1 only for idempotency; a default is spliced after pass 1) — it fails identically
  on both engines (a compile error, NOT a parity divergence). Wrap the default in a fixed-arity helper.
  Narrow enough to defer; a robust fix needs a per-call "already collapsed" marker.
- **Fix (adversarial-review follow-up):** a variadic METHOD call (`recv.m(a,b,c)`) is now collapsed even
  when another struct/enum defines a method of the SAME name with a DIFFERENT param list (a fixed-arity
  sibling, or two variadics differing only in the variadic param's NAME). The desugar method-spec
  resolution is now **receiver-aware first**: when the receiver's struct type is statically knowable
  (a let-bound local, an inline ctor, a struct-returning fn, or now a **typed parameter** `x: A`), it
  binds `m` against THAT struct's exact spec (incl. its variadic index) before the name-keyed all-agree
  table — so the surplus positionals collapse instead of reaching the checker uncollapsed and being
  rejected against the single `List[T]` slot. Typed params are registered in desugar's `local_struct`
  (`bind_param`), so a keyword-only post-variadic tail on a name-colliding method (`a.m(1,2,flag=true)`)
  also resolves rather than emitting the unsatisfiable "pass arguments positionally" error. A named call
  on a KNOWN receiver now resolves receiver-aware too (previously errored "multiple structs"); an
  UNRESOLVABLE receiver (unannotated closure param) still errors clearly. Regression tests: checker
  `variadic_method_*`, interp two-engine `variadic_method_name_collision_runs_byte_identical`,
  desugar `ambiguous_method_named_*`. **Lexer surface changed (`...` token):** editor TextMate grammar regenerated
  (`UPDATE_EDITOR_ASSETS=1 cargo test --test editor_tmlanguage`); **manual follow-up:** reinstall
  `chezzi-lsp` so editors stop serving stale highlighting. Docs updated: `spec.md` (variadic NON-GOAL
  overturned for arguments; variadic generics stay a non-goal), `syntax.md`, `stdlib.md`, `grammar.bnf`
  (`<param>` variadic alt), `ffi-and-packaging.md §5`, `future.md`.

**✅ LANGUAGE — conditional methods: `where` on a user struct/enum/newtype method's RECEIVER type
param (2026-07-03).** Closes the consistency gap left by the `where`-clause entry below: a user *method*
may now `where`-bound the ENCLOSING type's own type parameter (`struct Box[T]: fn top(self) -> T where
T: Comparable`), making the method callable only when the receiver's concrete type argument satisfies
the bound — Rust's `impl<T: Ord> Box<T>` conditional methods, and parity with native `List[T].sort`/`sum`.
**Mechanism (checker-only, additive):** `fn_sig`'s `where`-merge loop, when a `where` entry names
neither the method's own `[U]` nor an unknown param but IS the enclosing type's param (present in
`self.type_params`), records it on `receiver_bounds` → carried on the returned `FnSig.where_bounds`
(the SAME field the native harvest uses) instead of erroring; the struct/enum/newtype INSTANCE
method-call dispatch arms then call `enforce_bounds(&sig.where_bounds, {structParam_i → concreteArg_i},
span)` — byte-for-byte like the native `Ty::List` arm's `{T → elem}`. The instance enforcement fires
AFTER the `is_static` rejection (a static method wrongly called on a value yields ONLY the single
static-method diagnostic, no spurious bound error). A no-`self` (static) method carries a receiver
`where` too, so `infer_static_call` (the `Type.method(…)` path) ALSO enforces `sig.where_bounds`
against the inferred enclosing-param substitution — a conditional factory `Box.of(q)` rejects a
non-satisfying `q` at check time (closes the static accept-without-enforce hole). A `where` naming
NEITHER an own nor a receiver param still errors "unknown type parameter"; a method's OWN `[U]`
`where` still merges as before. Newtype is included for soundness (shared `fn_sig` accepts
receiver-bounds for newtype methods too → enforce at all three instance arms, no accept-without-enforce
hole). **Unknown/late-usage:** reuses
`enforce_bounds` verbatim — `satisfies_args_d` returns Ok for `Ty::Unknown` ("don't cascade"), so a
still-unpinned receiver arg DEFERS (never a spurious "does not satisfy"); a genuinely never-pinned
binding still fails at the pre-existing "cannot infer element type" error. **Three-engine byte-identical:**
`where` lowers to NOTHING — `src/interp`/`src/vm` get ONLY additive golden tests; `examples/conditional_
method.chz` (Box/Opt/Stack conditional methods invoked+printed, plus a `max2` whose body USES the
bound) asserted byte-identical on interp / --serial VM / M:N VM (`golden_conditional_method_chz` +
`..._matches_expected_and_interp`). No grammar change (`where` already grammatical on methods —
`cargo test conformance` green). checker + docs + additive golden tests only.
**Three follow-up fixes (adversarial review):** (1) the conditional method BODY may now use the bounded
op — `check_fn_body` merges `sig.where_bounds` onto the in-scope ENCLOSING param for the body's
duration (was recorded call-site-only, so `self.val < other` errored `cannot compare T and T`),
restoring symmetry with the free-fn `where` path (test `conditional_method_body_uses_receiver_bound`
+ enum mirror). (2) `fn_sig` DEDUPs the receiver-bound against the enclosing param's DECLARED bounds
(`struct Box[T: Comparable]` + `where T: Comparable`), so the static-dispatch path — which enforces
both `tps` and `sig.where_bounds` — no longer emits the identical "does not satisfy" twice (test
`conditional_static_method_redundant_decl_bound_reports_once`). (3) **SOUNDNESS — conditional
CONFORMANCE.** A conditional method that *is* a protocol's required method (e.g. `compare` ⇒
`Comparable`) makes the type STRUCTURALLY satisfy that protocol; enforcing the receiver `where` only
at explicit method-call dispatch left every `satisfies`-based consumer (operator dispatch `<`/`+`,
generic bounds `[U: Comparable]`, protocol-typed params) BYPASSING it — `Box[Tag] < Box[Tag]`
check-passed then ran into `Tag has no compare` (check-ok/run-diverge). Fixed at the source:
`satisfies_methods` now, after `method_matches`, verifies each candidate method's `where_bounds` hold
under the querying type's `{structParam → concreteArg}` map (`self.satisfies_args`, so `Ty::Unknown`
defers exactly like the call-site path). Conditional conformance is now honoured EVERYWHERE — `Box[int]`
is `Comparable`, `Box[Tag]` is not — closing the operator/bound/param bypass (tests
`conditional_method_operator_dispatch_enforces_receiver_bound`,
`conditional_method_as_generic_bound_arg_enforces`). Low blast radius: pre-conditional-methods code has
no method `where_bounds`, so the new check is a no-op for all existing structural conformance.

**✅ LANGUAGE — `where`-clause generic bounds + file-backed List `sort`/`sum` port (2026-07-03).** Adds
`where T: Bound, …` as an alternative spelling of generic bounds after a fn/native-fn signature
(`fn max[T](a: T, b: T) -> T where T: Comparable`), then USES it to finish the phase-5a container port:
`sort` is now fully file-backed as `native fn sort(self) -> nil where T: Comparable` in `std/prelude.chz`
and its bespoke Comparable arm in the `Ty::List` method-dispatch is DELETED (Comparable's satisfaction set
exactly matches runtime sort capability — fully sound); `sum` gains a `where T: Add` annotation (documentation
of a necessary bound) while its residual `!elem.is_numeric()` check-gate SURVIVES (Option B — sum's true
requirement is MONOID = Add + zero for the empty-list case, both runtimes numeric-only, so `where T: Add`
alone is too broad; a struct with a structural `add` still errors at CHECK time). **Mechanism:** new
`Token::Where` (KEYWORDS, corpus-safe) + `parse_where_bounds()` (reuses `parse_bound`) attaching an additive
`where_bounds: Vec<TypeParam>` to `FnDecl`/`NativeDecl`; for USER fns `fn_sig` MERGES each `where` entry's
bounds into the matching `[T]` type param (unknown-param = clear error; body-check enters the merged params so
a `where`-bounded op like `<` works in the body), so the existing `infer_generic_call`→`enforce_bounds` path
handles call sites with ZERO new machinery; for NATIVE methods `harvest_native_fn_sig` carries `where_bounds`
onto the sig and the `Ty::List` arm calls `enforce_bounds(&sig.where_bounds, {T->elem})`. (A user METHOD
where-bounding the receiver struct's OWN param — the "conditional method" shape — was subsequently
SHIPPED; see the conditional-methods entry above.) **BEHAVIOR-PRESERVING / three-engine byte-identical:** `where` lowers to NOTHING at runtime —
`src/interp` is UNTOUCHED and `src/vm` gets only a golden test; `examples/where_sort_sum.chz` (sort int/float/
struct-with-`compare`, sum int/float) is asserted byte-identical on interp / --serial VM / M:N VM
(`golden_where_sort_sum_chz_matches_expected_and_interp`). `docs/grammar.bnf` gains `<whereClause>`/`<whereList>`
(WHERE terminal + `parse_where_bounds` mapped in conformance; `tests/corpus/accept/where_clause.chz`). The
sort-arm-DELETED changes two existing tests' expected message from the bespoke `sort() requires …` to the
standard `does not satisfy Comparable` bound diagnostic. Lexer+parser+ast+checker+prelude+grammar+docs; both
engines untouched.

**✅ NATIVE-PRELUDE — phase 5c-protocols COMPLETE (all 16 builtin/reserved PROTOCOLS declared in
`std/prelude.chz` as plain `protocol` decls, a drift-guarded ADDITIVE mirror of the Rust seed)
(2026-07-03).** `Iterable[Elem]` (`iter(self) -> Iterator[Elem]`) lands as the 16th and last file-backed
protocol, closing the 5c port — its return type resolves via `resolve_type`'s dedicated `Iterator[T]`
value arm to the same `Ty::Struct("Iterator",[Elem])` the seed uses, so its shape byte-matches like the
other 15 (the earlier "parameterized-protocol-return rejected by `resolve_type`" claim was inaccurate; no
resolve fix was needed — the seam already yields the seed shape). The reserved structural protocols' SHAPE
(method sigs + `+`-joined embeds) lives in
`std/prelude.chz` — `Comparable`/`Stringable`/`Error`/`Hashable`, the operator protocols `Add`/`Sub`/`Mul`/
`Div`/`Mod`/`Neg`, the `Arithmetic` bundle (`: Add + Sub + Mul + Div`), `Iterator[Elem]`
(`next(self) -> Option[Self]`; Elem arity-only), `Iterable[Elem]` (`iter(self) -> Iterator[Elem]`), and
`Index[K,V]`/`IndexSet[K,V]`/`Slice[R]` — using the
EXISTING `protocol` decl syntax (no new grammar). **DRIFT-GUARDED PARTIAL PORT (the phase-5b precedent —
SHAPE moves, WIRING stays; the task's documented fallback, logged):** `prebuilt_protocols()` STAYS the live
runtime source (seeded at `Checker::new`, before any module); the `.chz` decls are NEVER inserted into the
protocol table — `hoist_protocol`'s reserved arm is now VALIDATE-AND-NO-OP in a stdlib module (early return,
no insert, no error), keeping the user-module `reserved (builtin)` rejection unchanged. Everything that
DECIDES conformance + operator binding stays 100% Rust-wired and UNTOUCHED: `satisfies` (int/float satisfy
`Add`/`Comparable`/`Neg` INTRINSICALLY with no method; a user struct satisfies structurally via its methods),
`iter_elem`/`iterable_elem` (Iterator/Iterable conformance), `recover_index_args`, operator lowering
(`+`→add, `<`→compare, `for`→Iterator, `[]`→Index, `[:]`→Slice, `?`/interpolation/Map-keys), `check_bounds`,
`is_reserved_protocol`, and BOTH engines (`src/vm`/`src/interp` are UNTOUCHED — checker-only, decl-shape-only,
so 3-engine parity holds by construction). **What the `.chz` decls buy:** a checked source-of-truth MIRROR
of each protocol shape, pinned to the Rust seed by `assert_native_protocol_shape_matches` (a debug-only
always-on `harvest_protocol_shape` + `debug_assert_eq!`/`fn_sig_eq` on the always-linked prelude — assert-only,
resolution-inert, keeps the harvest helper production-live) and by the unit guard
`native_protocol_shapes_match_prebuilt_seed` (harvested `type_params`/`embeds`/ordered method sigs
byte-equal `prebuilt_protocols()`). **COUNT:** 16 reserved protocols total; ALL 16 now file-backed +
drift-guarded (Iterable no longer the exception). Runtime `Iterable` satisfaction (`iterable_elem`/
`iter_elem`, for-loop lowering, `infer_method_call`'s `Iterable.iter` element recovery) is UNTOUCHED — the
`.chz` decl is validate-and-no-op at hoist, never inserted into the protocol table. **BEHAVIOR-PRESERVING / three-engine
byte-identical:** `examples/protocols_5c.chz` (int/float intrinsic arithmetic, a user 4-op struct under
`+ - * /` AND through `[T: Arithmetic]`, `[T: Comparable]` max over a Comparable struct, a user `Iterator`
struct in a `for`, builtin Index/Slice, a user IndexSet struct) is asserted byte-identical on interp /
--serial VM / M:N VM (`protocols_5c_3engine_parity` via `assert_mc_parity`), and every pre-existing
protocol/bound/operator-overload/generic-constraint/Iterator test stays green UNCHANGED. `grammar.bnf`
needs NO change (plain `protocol` already in the grammar; conformance green). Checker+prelude+docs only.

**✅ NATIVE-PRELUDE — phase 5b-native-enum (the builtin `Option`/`Result` variant SHAPE made
file-backed: `native enum Option[T]` (`Some(T)`/`None`) / `native enum Result[T, E]` (`Ok(T)`/`Err(E)`)
declared in `std/prelude.chz`, mapped ADDITIVELY onto the reserved `Ty::Option`/`Ty::Result`)
(2026-07-02).** Builds the ENUM analog of `native struct` — a new `native enum NAME[T…]:` decl form
(parser `parse_native_enum` + `StmtKind::NativeEnum` + hoist reject in user modules; body-less variants
via `parse_enum`'s variant loop, generics via `parse_type_params`, optional body-less `native fn`
methods with a leading bare `self` harvested like native-struct methods, no-self = parse error) — and
uses it to file-back the declarable variant SHAPE of the two most deeply-wired builtin enums.
**PARTIAL PORT (the task's documented fallback outcome — logged): SHAPE moves, WIRING stays.** Unlike
5a/4c (which retired a LIVE, consulted `*_method_sig` arm and rerouted resolution through the harvested
table), Option/Result have (a) ZERO bespoke methods (no `Ty::Option`/`Ty::Result` arm in method
resolution) and (b) NO variant-table consumer — their variant shape is synthesized INLINE from the `Ty`
shape at ~8 Rust sites (`variants_of`, `match_kind`, the `Ok`/`Err`/`Some`/`None` name-guards +
construction, `resolve_type` identity), none of which read `self.enums`. Rerouting those through a
harvested table IS touching the `?`/match core the phase must keep byte-identical, so **nothing of the
wiring moved**: `?` propagation (Result AND Option), match exhaustiveness, `Ok`/`Err`/`Some`/`None`
construction (checker + `NativeRet` runtime), the `Result[T]`→`E = Error`-protocol surface default, and
top-level error unwind ALL stay 100% Rust-inline and UNTOUCHED. **What the `.chz` decl buys:** a checked
source-of-truth MIRROR of the variant shape, guarded against drift by `assert_native_enum_shape_matches`
(a `harvest_native_enum_table` + `debug_assert_eq!` on the always-linked prelude — assert-only, no
resolution effect, keeps the harvest helper production-live) and by the unit guard
`native_enum_option_result_shape_matches_inline` (parsed+resolved variants byte-equal
`variants_of(Ty::option/result_e(Param))`). The `NativeEnum` hoist arm CRITICALLY must NOT register into
`self.enums`/`enum_names` (that would mint a colliding nominal `Ty::Enum` and silently break `?`/match) —
it stays validate-and-no-op; identity stays 100% in `resolve_type`. **BEHAVIOR-PRESERVING / three-engine
byte-identical:** `src/vm` + `src/interp` gain ONLY forced no-op AST match arms; `examples/native_enum_smoke.chz`
(construction + `?` on a Result- and an Option-returning fn + exhaustive match) is asserted byte-identical
on interp / --serial VM / M:N VM (`golden_native_enum_smoke_chz_matches_expected_and_interp`), and every
pre-existing Option/Result/`?`/match/exhaustiveness test stays green UNCHANGED. New `nativeEnumDecl`
production in `grammar.bnf` (conformance green, corpus `accept/native_enum.chz`). Parser+checker+docs only.
`Iterator` (a protocol, NOT an enum) is untouched here — its protocol SHAPE is file-backed in phase 5c
(see above); conformance stays `iter_elem`-special.

**✅ NATIVE-PRELUDE — phase 5a-containers (the builtin `List`/`Map`/`Set` METHOD surface made
file-backed: `native struct List[T]` / `Map[K, V]` / `Set[T]` declared in `std/prelude.chz`, harvested
into method tables mapped ADDITIVELY onto the reserved `Ty::List`/`Ty::Map`/`Ty::Set`) (2026-07-02).**
The three builtin containers' METHOD sigs move out of the bespoke Rust `list_method_sig`/`map_method_sig`/
`set_method_sig` arms into body-less `native fn` methods (leading bare `self`, stripped by the harvest) on
`native struct` decls in the always-linked `std/prelude.chz` — the exact phase-4c-concurrency generic
native-struct + harvest pattern, now applied to the RESERVED UNIVERSE containers. **BEHAVIOR-PRESERVING:**
each harvested `FnSig` BYTE-MATCHES the retired arm (guarded by `container_method_sigs_byte_match`
enumerating all 24 flat methods with concrete K/V subst), and output is byte-identical on all three engines
(guarded by `examples/container_methods.chz` + `container_methods_3engine_parity`). **CRITICAL additive
subtlety (as concurrency/net):** `List`/`Map`/`Set` KEEP resolving to the reserved `Ty::List`/`Ty::Map`/
`Ty::Set` — the harvest attaches ONLY the method table (never a fresh `Ty::Struct`); the LITERAL syntax
(`[...]`/`{k:v}`/`{1,2}`) + the turbofish ctor (`List[int]()`, `builtin_container_sig`) + `resolve_type`'s
element-type arms stay 100% COMPILER-WIRED and UNCHANGED, and **runtime (`src/vm`/`src/interp`) is
UNTOUCHED** (method dispatch stays by name). Seeding follows the `ref_seed`/`concurrency_seeds` precedent:
the prelude's `List`/`Map`/`Set` tables are captured into a new `container_seeds` field when the prelude
module (graph order[0], always-linked) is checked, and `seed_stdlib_structs` re-seeds them bare (method-
table only — NO `struct_names`/`bare_types` licensing needed, they're UNIVERSE) into `self.structs`; the
cfg(test) single-module `check` path harvests them straight in via `seed_native_prelude_sigs`. The
`Ty::List`/`Ty::Map`/`Ty::Set` dispatch arms route through `native_handle_method` with the value's
element/key/value type substituted for `Ty::Param`. **The generic-recovery `List` HOFs are now ALSO
file-backed (UPDATE 2026-07-03, phase 6 — closure-return loop-back):** `map[U]`/`filter`/`fold[U]`/
`sort_by`/`sort_by_key[K: Comparable]` are declared in the prelude struct; the bespoke `infer_list_hof`
arm is DELETED. This needed two generalizations: (1) a **native method may declare its OWN `[U]` type
param** after the name (parser `parse_native` + AST `NativeDecl.type_params` + harvest onto
`FnSig.type_params`; grammar `nativeDecl`/`nativeMethodDecl` gain optional `<typeParams>`), so a
method-own param routes through `infer_generic_method`; (2) the generic solver gained a **closure-return
LOOP-BACK** — after the per-arg re-inference pass (which pins an unannotated closure's params and computes
its concrete body-return), `check_generic_arg` now RETURNS the refined actual type, and
`infer_generic_method` feeds those refined types into a SECOND `unify` pass, filling ONLY params still free
after pass 1 (safe because `unify` is only-bind-unbound + ignore-Unknown → every already-resolved generic
call is a strict no-op), then re-enforces bounds on the newly-bound params and degrades any still-free
param to `Unknown` **only when it appears in a PARAMETER position** (recoverable-in-principle but the
argument's type was itself `Unknown` — the empty-collection case `[].map(...)` → `List[?]`). A param
appearing ONLY in the return position and in NO parameter (`fn make[U](self) -> U`) is genuinely
un-inferable and is deliberately LEFT as a leaked `Ty::Param`, so assigning the result to a concrete type
is REJECTED (soundness: a wrong static type must not silently escape onto the value — an unconditional
degrade to `Unknown`, which `assignable` treats as universally assignable, would mask it). Recovers a
return-position param from an unannotated closure body generally (not
map-special): `Box(3).apply(fn(x): x+1)` on `fn apply[U](self, f: fn(T) -> U) -> U` also yields `int`.
Diagnostics are the uniform general-path wording (retired the bespoke "predicate"/"map expects…"/
"sort_by_key key type must be Comparable" strings). `sort` stays file-backed via `where T: Comparable`;
`sum` KEEPS its `!elem.is_numeric()` residual gate (Monoid requirement, `where T: Add` alone too broad).
Checker/parser-only; runtime type-erased + name-keyed → 3-engine byte-identical parity. `Map`/`Set`'s key/element type param carries a `Hashable` bound so the internal
`Map[K, V]`/`Set[T]` return types resolve past the hashable gate at harvest. The bespoke
`list_method_sig`/`map_method_sig`/`set_method_sig` fns are DELETED; `unique_member_owner`'s bail set now
checks the harvested tables' `methods.contains_key` (byte-identical to the retired arms' 9/8/7 flat
methods) and the `builtin_method_slices_all_resolve` hover drift-guard resolves the slices against the
seeded tables. Parser+checker-only. ~1283 checker tests + 3-engine parity green.

**Bug D fix (2026-07-04 — closure-return loop-back now recovers a method `[U]` through a NESTED FREE
generic call in the body).** `xs.map(fn(x): ident(x))` where `fn ident[T](x: T) -> T` was spuriously
rejected (`cannot apply + to T and int` on `ys[0] + 1`): the closure param `x` inferred `int`, but the
UNANNOTATED body `ident(x)` was prepass-inferred under `generic_arg_prepass` (`x: Unknown`), so
`infer_generic_call(ident, [Unknown])` could not bind ident's own `T` and returned a LEAKED
`Ty::Param("T")`. Pass-1 `unify` in `infer_generic_method` then prematurely pinned `map`'s return-position
`U := Param("T")`, and the loop-back — which only fills params still FREE — could not correct it, so
`ys: List[T]` leaked. FIX (checker-only, `src/checker/proto.rs` + a `mask_closure_ret` helper in
`src/checker/mod.rs`): in `infer_generic_method`, when the arg is a closure **with NO return annotation**,
(1) unify pass-1 against a RETURN-MASKED copy of its actual `Func` (return → `Ty::Unknown`) so only its
PARAMETER positions can bind a method type param, and (2) ALWAYS mask the same closure's fallback return in the
`check_generic_arg` assignability check (the prepass leaked `Param` would otherwise mismatch `want`'s return —
whether that return is a still-free `[U]` OR already concrete), keeping the internal check to params + arity.
This defers `U` to the loop-back's checking-mode re-inference, which recovers it as the CONCRETE return (`int`)
→ `ys: List[int]`, prints `2`. SOUNDNESS is upheld by a SEPARATE explicit check, not by the mask's presence:
after `check_generic_arg` returns the REFINED (checking-mode re-inferred) closure type, when the closure's
expected return is ALREADY concrete (e.g. `fold[U]`'s `U` pinned to `int` by `init`) the refined return is
asserted assignable to it — so `xs.fold(0, fn(acc,x): "wrong")` is rejected (`str` ≠ `int`) while
`xs.fold(0, fn(acc,x): ident(x))`/`ident(acc)` — whose prepass leaked a rigid `Ty::Param` but whose refined
body types `int` — is ACCEPTED. (The earlier gate that masked only a still-free `[U]` (`closure_ret_wants_free_mtp`)
was WRONG: it spuriously rejected exactly those concrete-return nested-generic-call `fold` bodies — the
adversarial-review-caught regression — because the unmasked prepass `fn(?,?) -> T` failed the internal check.
Checking the refined type fixes both directions.) `U` is bound concretely, never degraded to `Unknown`
(assigning the result to `List[str]`/`List[List[int]]` is still cleanly rejected). An annotated closure return
(`fn(a,b) -> int: …`) is left authoritative (no mask), preserving the exact arity-mismatch diagnostic. Runtime is generic-erased → serial==M:N automatic.

**Bug D FREE-FN analog fix (2026-07-05 — the same closure-return recovery now runs on the generic
FREE-FUNCTION / module-qualified-fn HOF path).** The Bug D fix above landed only on the METHOD path
(`infer_generic_method`); the symmetric `infer_generic_call` deliberately DISCARDED the refined closure
type (`let _ = self.check_generic_arg(...)`), so a user free-fn HOF with a **return-only** type param
leaked `Ty::Param` into its return — `fn applyone[U](x: int, f: fn(int) -> U) -> U` called
`applyone(5, fn(x): x*2)` then `+ 1`, and the `-> List[U]` container form `mymap([1,2,3], fn(x): x*2)`,
both rejected with `cannot apply + to U and int`; the sibling-pinned `fn apply[A,B](f: fn(A)->B, a: A)
-> B` (`apply(fn(x): x*2, 5)`), the protocol-bounded `fn mapadd[U: Add](...)`, and nested-free-generic
bodies (`fn(x): ident(x)`) likewise. FIX (checker-only, `src/checker/proto.rs`): Bug D's FINAL sound
mechanism (return-masked pass-1 unify + REFINED-type capture + the SEPARATE concrete-return soundness
check + the loop-back second `unify` + newly-bound bound re-enforcement + the method-only param-position
degrade) is factored into ONE shared helper `recover_return_only_params` called by BOTH
`infer_generic_method` (a byte-identical refactor — the existing Bug-D method tests are the safety net)
and `infer_generic_call`. The free-fn path additionally masks bare-closure returns in its pass-1
`unify` loop (mirroring the method path) so a nested-free-generic body's leaked prepass `Param` cannot
prematurely pin the return-only param before the loop-back. **Two adversarial-review bugs fixed on a
follow-up pass (2026-07-05):** (bug 1) the free-fn path's un-inferable-param probe
(`report_uninferable_closure_params`) runs BEFORE the loop-back, so a return-only `[T]` bound only from
a bare closure's CONCRETE return was still masked-away and mis-reported as a deadlock when a SIBLING
closure used the same `[T]` in PARAMETER position (`fn pair[T](f: fn()->T, g: fn(T)->int)` called
`pair(fn(): 5, fn(x): x+1)` — accepted on `main`, wrongly rejected on the branch). FIX: a small
concrete-return sub-pass right after pass-1 binds `[T]` from any bare closure whose prepass return is
already concrete (`ty_contains_param` FALSE) — AFTER value/param args (only-bind-unbound `unify`, so a
sibling value arg still wins, no binding race) and BEFORE the probe; a leaked-`Param` prepass return
stays masked/deferred to the loop-back. (bug 2) two closures binding the SAME return-only `[U]` to
CONFLICTING concrete types type-checked OK but bound `[U]` from only the first, dropping the second
(`fn pick[U](cond, a: fn()->U, b: fn()->U)` / `fn two[U](f: fn(int)->U, g: fn(int)->U)` with a `str` vs
`int` pair — accepted then crashed at runtime). FIX: the loop-back `unify` is now INTERLEAVED into the
per-arg loop, so once the first closure binds `[U]` the sibling's `want` return is CONCRETE and its
mismatching body is rejected by the SEPARATE concrete-return soundness check instead of being silently
dropped. IMPORTANT (adversarial-review fix): the
final param-position degrade-to-`Unknown` step is **gated `true` on the METHOD path only** (its
receiver-collection HOFs `[].map(...)` intentionally degrade an empty element param to `List[?]`) and
**`false` on the free-fn path** — `infer_generic_call` never degraded, so a still-unbound param-position
free-fn type param bound to nothing by an empty-collection arg (`fn first[U](xs: List[U]) -> U` called
`first([]) + 1`, or `fn tag[U](xs: List[U]) -> List[U]`) must stay a leaked `Ty::Param` that downstream
concrete use REJECTS and that keeps the deliberate Category-2 "un-inferred type parameter; bind at the
construction site" diagnostic; degrading it there laundered a compile error into a runtime panic and is
NOT this change's scope. Free-fn CLOSURE-param type params left un-inferable by an empty arg are already
`Unknown`-bound by `report_uninferable_closure_params`, so skipping the degrade there is
behavior-preserving. SOUNDNESS is upheld by the same SEPARATE check, not the mask:
when the return-only param is ALREADY pinned by a sibling value arg / explicit slot, the refined closure
return is asserted assignable to that pin, so `fn f[U](init: U, g: fn(int) -> U, ...)` called
`f(0, fn(x): str(x), ...)` is a clean type error (`closure argument to 'f' returns str, expected int`),
never laundered onto the pinned `int` — the free-fn analog of the `fold`-init laundering hole. A
genuinely un-inferable return-only param (`fn make[U]() -> U`) stays a leaked `Ty::Param` (concrete
assignment rejected); a genuinely ambiguous body (`fn(x): fn(y): x+y`) stays exactly one
`cannot infer type of parameter 'y'` error. OUT OF SCOPE (unchanged): the ctor paths
(`infer_generic_struct`/`infer_newtype_call`) share the identical discard but have no free-function
repro — a possible follow-up; Category-2 late/backward inference and the generic-fn-VALUE gap
(`g := ident; g(5)`) are distinct limitations, untouched. +7 checker tests (recovers scalar+container,
pinned-mismatch-rejected, boundaries, must-not-regress, ambiguous-stays-clean, empty-arg-stays-rejected,
plus the two adversarial-review regressions: sibling-closure-param-use-recovers [bug 1] and
conflicting-return-only-closures-rejected [bug 2]) + 3 parity tests
(`parity_free_fn_hof_map`/`_apply_sibling`/`_sibling_closure_param`), all RED-first on the release
binary. Runtime is generic-erased → serial==M:N automatic.

**✅ NATIVE-PRELUDE — phase 4c-followup (native instance methods now declare `self`, mirroring user
structs) (2026-07-02).** A `native fn` inside a `native struct` body is an INSTANCE method and now MUST
declare a leading bare `self` as its first parameter (`native fn read(self, n: int) -> Result[str]`,
`native fn get(self) -> T`) — resolving the DX asymmetry where native methods omitted `self` yet were
instance methods. **BEHAVIOR-PRESERVING:** the parser accepts `self` and harvest (`harvest_native_fn_sig(_,
skip_self=true)` in PASS 1b) STRIPS it BEFORE the param→`Ty` map (so `self` is never a spurious dynamic
`Ty::Unknown` receiver) AND before the optional-tail count — the resulting method-table `FnSig`
(params/min_params/ret) is BYTE-IDENTICAL to the pre-`self` spelling, so checker resolution, runtime
dispatch, and 3-engine parity are all unchanged (the existing `net_sig_from_file_not_native_module_sig` /
`concurrency_harvested_method_sigs_shape` sig-guards pass with their SAME asserted params — the
behavior-preserving proof). A self-less body `native fn` is now a parse error (`native instance method
must declare 'self' as its first parameter`) — the self-less form is **RESERVED** for a future native
STATIC method (not implemented — just the error). `self` is valid ONLY as the first param, and a
module-level (free) `native fn` may NOT take `self` (`parse_native(in_struct: bool)` threads the rule).
Updated `std/net.chz` (Socket/Listener, 6 methods) + `std/concurrency.chz` (Shared/RwShared/Atomic/
Executor); `regex.Match`/`request.Response` are fields-only (no change). Parser+checker-only; `src/vm` +
`src/interp` UNTOUCHED.

**✅ NATIVE-PRELUDE — phase 4c-concurrency (`std.concurrency` made file-backed: the four GENERIC native
types `Shared[T]`/`RwShared[T]`/`Atomic[T]`/`Executor` WITH method tables declared in
`std/concurrency.chz`) (2026-07-02).** The **LAST** virtual native module — after it EVERY native std
module is file-backed, and `native_module_sig` retains only the **`ffi`** (`ptr` + fixed-width names) +
**`time`** (`timer`) opcode/type-license tails (the `concurrency` arm is **DELETED ENTIRELY**, no
residual). This EXTENDS the 4c-net native-method-binding capability from non-generic native structs
(`Socket`/`Listener`) to **GENERIC** ones: a `native fn` in a `native struct Shared[T]` body harvests a
method sig carrying `Ty::Param("T")`, and at each call site `native_handle_method(ty, method, &[elem])`
**substitutes** the box's element type (`Shared[int].set` expects `int`) — the same per-type param subst
the generic-struct machinery uses. **CRITICAL additive subtlety (as net):** `Shared`/`RwShared`/`Atomic`/
`Executor` KEEP resolving to the RESERVED `Ty::Shared`/`Ty::RwShared`/`Ty::Atomic`/`Ty::Executor` (opaque
VM handles — NOT fresh `Ty::Struct`); the `.chz` `native struct` feeds the checker ONLY the type + method
sigs, the ctors STILL lower to `Op::NewShared`/etc **by name** and every method stays VM-intercepted —
**runtime UNTOUCHED**. The harvested tables are cached into `concurrency_seeds` (AFTER
`attach_native_module_metadata`, unlike net's before, because the metadata step mutates the read/submit
sigs) and re-seeded bare into `self.structs` by `seed_stdlib_structs` (method-table only — NO
`struct_names`/`bare_types` licensing, so the bare name stays import-gated by `imported_concurrency`).
**Two sigs a plain harvest can't express, ported as metadata** in `attach_native_module_metadata`:
`RwShared.read(f)` — declared UNANNOTATED, retyped to `fn(T) -> ?` (any R; the real R is recovered from
the closure at the `Ty::RwShared` dispatch arm); `Executor.submit(f)` — declared UNANNOTATED, retyped to
`fn() -> ?` (any return, zero-arity). **One dispatch-time residual:** `Atomic.add`/`sub` exist only for a
numeric `T` — a `!elem.is_numeric()` gate kept in the `Ty::Atomic` arm. The bespoke
`shared_method_sig`/`rwshared_method_sig`/`atomic_method_sig`/`executor_method_sig` fns are DELETED.
**Qualified-path fix (new vs net):** the harvested `sig.struct_defs` entry made `concurrency.Shared[int]`
(a qualified annotation / `type`-alias / `newtype` body) resolve as a nominal `Ty::Struct` — both
`resolve_type` and `resolve_qualified_ro` now skip a reserved native type (`qualified_builtin_ty` is
`Some`) so it keeps its reserved `Ty` (matching the bare-after-import path). **Stdlib consumers:**
`std/concurrency/collection.chz` (RwShared) + `std/cancel.chz` (Shared) now explicitly `import
std.concurrency` — the file-backed native module must be a graph DEPENDENCY so its method table is
harvested/seeded before those modules are checked, regardless of the entry program's import order
(behavior-preserving: the bare names were already stdlib-licensed). Tests:
`concurrency_sig_from_file_not_native_module_sig` (arm gone, four types harvested with method names),
`concurrency_harvested_method_sigs_shape` (metadata port: read=`fn(T)->?`, submit=`fn()->?`),
`concurrency_methods_resolve_via_harvested_table_with_subst` + `executor_submit_accepts_any_return_rejects_arity`,
`native_std_module_is_file_backed` (resolver — converted from `native_std_module_is_virtual`, no virtual
module remains), the VM 3-engine regression guard `concurrency_file_backed_three_engine`, and the sibling
provenance asserts retargeted to `std.ffi`'s residual type-license tail. Full suite green (3174 lib + all
integration), clippy clean, `grammar.bnf`/conformance unchanged. 3-engine CLI parity re-verified on
`examples/{shared,rwshared,atomic,executor,parallel_shared,native_qualified}.chz` (default==serial==expected).

**✅ NATIVE-PRELUDE TABLE — phase 1 (refactor-only, pure functions) (2026-07-01).** A single synthetic
Rust `const PRELUDE: &[PreludeFn]` in `src/checker/mod.rs` is now the **SINGLE SOURCE OF TRUTH** for the
four first-class universe FUNCTIONS (`print`/`ord`/`chr`/`panic`), replacing the scattered hard-coded
match arm each phase used to keep. Row shape: `PreludeFn { name, intrinsic: Intrinsic, first_class,
make_sig }` where `enum Intrinsic { Print, Builtin }` (`Print` ⇒ direct call lowers to
`CallPrint`/`CallPrintSep`; `Builtin` ⇒ `Op::CallBuiltin(name, argc)`). The signature is carried as a
const-safe `make_sig: fn() -> FnSig` fn pointer (a `FnSig` holds `Vec`/`Box`, so it can't be a literal
`const` field) and stays PRIVATE so the `pub(crate)` row never leaks the module-private `FnSig`.
Every phase now READS the table: checker `is_firstclass_builtin_fn` = table `.first_class`, `builtin_sig`
delegates the four sigs to `(make_sig)()`, the value-position `Ty::BuiltinFn` arm is unchanged (already
sources `builtin_sig`); compiler `is_builtin` + `compile_call`'s direct-call opcode selection derive from
`prelude_fn(name).intrinsic`; interp `builtins::is_builtin` derives the same way. **ZERO observable
behavior change** — direct calls emit byte-identical bytecode (`print(x)`→`CallPrint(1)`,
`print(x, sep=…)`→`CallPrintSep`, `ord`/`chr`/`panic`→`CallBuiltin`), the hot path only gains a
compile-time table lookup, and three-engine byte-identical parity (interp / `--serial` VM / M:N VM) on
`examples/defer_builtin_value.chz` + all existing guard tests stays green. **Native impls UNTOUCHED**:
`vm::do_builtin` arms, `builtin_ord`/`builtin_chr`, the print stringify, and all name-keyed runtime
dispatch (`Value::Builtin`/`Obj::Builtin`, `LoadBuiltin`, spawn/wire/snapshot) stay exactly where they
are — the table is COMPILE-TIME METADATA ONLY (the `NativeFn` host seam only takes int/str/map args,
which is precisely why `print` needs its dedicated value/opcode path). New drift guard
`prelude_table_is_single_source_of_truth` (checker/tests.rs) + a bytecode pin test
`direct_builtin_calls_lower_to_specialized_opcodes` (compiler tests) lock the invariant that every phase
agrees with the table — the whack-a-mole class this track kills.

**✅ NATIVE-PRELUDE TABLE — phase 2a (refactor-only, scalar-conversion ctors) (2026-07-02).** Added a
third intrinsic kind `Intrinsic::Ctor` and folded the **five scalar-conversion CONSTRUCTORS**
(`int`/`float`/`str`/`bytes`/`bytearray`) into the table as rows with `first_class: false` (ALWAYS —
types/ctors are NOT first-class values, uniform with `f := Point` / `f := List` staying rejected). Each
row carries the exact `FnSig` its old hard-coded `builtin_sig` arm did (`int`/`float`/`str` take `?`→
`Int`/`Float`/`Str`; `bytes`/`bytearray` take `?`→`Bytes`/`ByteArray`) and dispatches on a direct call
to the same name-keyed `Op::CallBuiltin(name, argc)` — so `int("5")`, `int("ff")`… emit **byte-identical
bytecode** and `vm::do_builtin`'s native conversion arms are **UNTOUCHED** (metadata only). The now-dead
`int`/`float`/`str`/`bytes`/`bytearray` arms in `builtin_sig` were deleted (the `prelude_fn` early-return
supplies them); `is_builtin` in compiler + interp drop those five from the hard-coded `matches!` and
read them from the table via `Intrinsic::Builtin | Intrinsic::Ctor`. **NON-FIRST-CLASS enforced**: every
first-class value path (`is_firstclass_builtin_fn`, `Ty::BuiltinFn` arm, `LoadBuiltin`) gates on
`.first_class == true`, so a `Ctor` row never leaks a first-class value — `f := int` / `defer str(...)`
stay rejected on the identical fall-through path as `f := List`, with zero new guard code. The drift
guard is extended (Ctor name-set, no `Ctor` row is first-class) plus a `scalar_ctor_conversions_parity`
two-engine test and the extended
bytecode pin. **The GENERIC / reserved-type container ctors** (`List`/`Map`/`Set`/`range`) were folded
in later → **phase 2b** (below), keeping their generic type-identity in `resolve_type`. **North-star:**
realized in **phase 3a** below — the signatures moved to a real `.chz` prelude; only `print` (variadic)
+ `range` (arity overload) remain synthetic carve-outs. (The earlier ".chz prelude blocked on user-facing
variadics" framing is **superseded**: a `native`-decl signature needs no `*args` syntax — only `print`'s
`sep=`/`end=` variadic still can't be spelled in `.chz`, so it stays the sole synthetic function row.)

**✅ NATIVE-PRELUDE — phase 4c-ffi (`std.ffi`'s 59 FUNCTION sigs made file-backed:
`std/ffi.chz`) (2026-07-02).** REFACTOR-ONLY, **ZERO observable change / three-engine byte-identical** —
the proven phase-4b/4d/4e/4f pattern applied to `std.ffi`. All **59** callable fns (`null`/`is_null`, the
`load_*` family — 14 loads × {base, `_at`} — the `store_*` family — 13 stores × {base, `_at`} — and
`alloc`/`alloc_zeroed`/`free`) are now bodyless `native fn` decls in a real **`std/ffi.chz`**, harvested by
the checker via `harvest_native_module`; the resolver loads the file while **KEEPING the `native` marker**
(runtime dispatch stays name-keyed via `native_members("std.ffi") => ffi::MEMBERS` — bytecode + `src/native/ffi.rs`
UNCHANGED). `std.ffi` added to the shared `crate::native::is_file_backed_native` predicate. The migration is
**PARTIAL BY DESIGN**: a `native fn` produces a `sig.functions` entry, but `std.ffi` ALSO exports **type-license-only**
names — the opaque `ptr` handle + the eight fixed-width C-ABI integer names (`int8..uint64` in `ffi::TYPE_NAMES`)
— which resolve to `Ty::Ptr`/`Ty::Int` via `resolve_type` gated on `imported_ffi_types` and have NO `.chz`
decl syntax (no way to spell a bare type-license name aliasing a builtin scalar). So the `native_module_sig("std.ffi")`
arm is **REDUCED to only that type-license tail** (the `TYPE_NAMES` loop + the `ptr` insert), mirroring the
residual `std.net`/`std.concurrency`/`std.time` arms — full deletion is NOT achievable without inventing a new
decl kind (out of scope). **Non-obvious blocker solved:** harvesting `native fn null() -> ptr` resolves `ptr`
through `resolve_type`'s `ptr` arm, which requires `imported_ffi_types.contains("ptr")`, but harvest runs WITHOUT
`begin_module` (that set is empty) → `harvest_native_module` now **transiently licenses** every `sig.types` name
that is `ptr`/in `TYPE_NAMES` into `imported_ffi_types` before PASS 2 and **restores exactly those** after (the
direct analog of the existing `struct_names` transient; driven off `sig.types` so module-agnostic; no leak — a
sibling that never imported std.ffi still rejects bare `ptr`). Every store (26) + `free` spells an explicit
`-> nil` (harvest maps a MISSING ret to `Ty::Unknown`, NOT `Ty::Nil` — the old arm returned `Ty::Nil`, so the
explicit `-> nil` is correctness-critical to byte-match). Tests: `enc_crypto_uuid_time_sig_from_file_not_native_module_sig`
(inverted for ffi — arm's fns gone, `ptr`+`TYPE_NAMES` license kept), `ffi_fn_sigs_exact` (all 59 harvested sigs
byte-equal to the deleted for-loops + MEMBERS len==59 cross-check), `ffi_ptr_license_does_not_leak_past_harvest`
(per-name `import ptr`/`int32` license + no cross-module leak), the existing 10 `ffi_*` typecheck tests unchanged,
and the 3-engine golden `golden_std_native_4c_chz_matches_expected_and_interp` (`examples/std_native_4c.chz`
alloc/store/load round-trip, VM==interp==M:N — FFI is layout-dependent UB, so a real round-trip, not goldens alone).
`grammar.bnf` unchanged (conformance green). **Remaining `native_module_sig` content after 4c-ffi:** `net`
(methoded `Socket`/`Listener`) + `concurrency` (opcode type-licensing) + `ffi`'s type-license tail — `net`
migrated next (see phase 4c-net below).

**✅ NATIVE-PRELUDE — phase 4c-net (native METHOD-binding capability built + `std.net` made file-backed:
native TYPEs `Socket`/`Listener` WITH method tables + `connect`/`listen` declared in `std/net.chz`)
(2026-07-02).** A genuine checker CAPABILITY build (not a mechanical batch): a `native fn` inside a
`native struct` body is now a body-less **method** sig, harvested into that type's method table
(`harvest_native_module` PASS 1b) and checked via the **normal method-resolution path** — retiring the
bespoke `socket_method_sig`/`listener_method_sig` arms. `std.net` becomes a **real `.chz`**: `Socket`
(`read`/`write`/`close`) + `Listener` (`accept`/`addr`/`close`) native structs + `connect`/`listen`
free fns, all harvested. **CRITICAL additive subtlety:** `Socket`/`Listener` KEEP resolving to the
RESERVED `Ty::Socket`/`Ty::Listener` (opaque VM handles — NOT a fresh `Ty::Struct`), so VM interception
(`connect`/`listen`/`read`/`write`/`accept` stay VM-intercepted by name) + `connect`'s `Result[Socket]`
return are UNCHANGED. The harvested method table is re-seeded (method-table only, NO bare licensing —
`net_socket_seed`/`net_listener_seed` → `seed_stdlib_structs`, the `ref_seed` precedent) into
`self.structs["Socket"]`/`["Listener"]`, and the `Ty::Socket`/`Ty::Listener` method arms look it up
there. Bare-name annotation stays import-gated via `imported_net` + `resolve_type`'s reserved arm.
**Gotcha fixed:** the native-module harvest branch never ran `begin_module`, so `current_module_is_stdlib`
was stale-false → `resolve_type(Socket)` in `connect`'s return would error `unknown type 'Socket'`; set
`c.current_module_is_stdlib=true` at the top of the native branch (every native module IS std;
additive-safe). This RETIRES the hand-built `"std.net"` `native_module_sig` arm (default-empty now).
`attach_native_module_metadata` port = **no-op for net** (no Socket/Listener method recovers a return
type from a closure arg — all concrete plain/optional-tail). Runtime (VM/interp socket/listener dispatch,
connect/listen interception, `bind_import` Socket/Listener skips) **UNTOUCHED**. **Three-engine
byte-identical** (checker-only cut): `net_sig_from_file_not_native_module_sig` (provenance — arm gone,
harvested Socket/Listener method sigs byte-exact to the retired bespoke arm), the D6c
`socket_read/write/listener_accept_with_timeout_type_checks` + arity/type rejects (now resolve via the
harvested table), `native_struct_parses_native_methods` (parser), `net_from_import_runs_both_engines`
(extended: whole-module + from-import, method calls in a checked body — VM==interp), existing
`examples/socket_timeout.chz` (--parallel golden) + `echo_server.chz`/`echo_server_spawn.chz` unchanged.
`grammar.bnf` unchanged (native-decl grammar exists from 3a/4a; conformance green). **Roadmap (DONE):**
after 4c-ffi + 4c-net + **4c-concurrency** (the last migration — generic types `Shared`/`RwShared`/
`Atomic`/`Executor`, see the top block), `native_module_sig` retains only **`ffi`'s residual type-license
tail** (`ptr` + fixed-width `int8..uint64`) + **`time`'s `timer`** opcode-license — no runtime member.

**✅ NATIVE-PRELUDE — phase 4f (`std.process` + `std.request` made file-backed: native TYPE + FNs
declared in `std/process.chz` / `std/request.chz`) (2026-07-02).** Mechanical application of the proven
phase-4b regex pattern to the two remaining fields-only native-struct modules. `std.process` and
`std.request` are no longer *file-less virtual* modules — each is now a **real `.chz`** whose fields-only
`native struct` (`ProcResult` [stdout, stderr, code] / `Response` [status, body, headers]) + `native fn`s
(process: `cmd`/`run`/`run_args`; request: `get`/`post`/`request`/`put`/`patch`/`delete`/`head`) are declared
**in-module** and harvested by the checker via `harvest_native_module`. The resolver loads the real files
while **keeping the `native` marker** (runtime member dispatch stays name-keyed via `native_members`;
bytecode UNCHANGED). This RETIRES BOTH the hand-built `"std.process"`/`"std.request"` **fn arms** AND their
`export_struct` **type arms** in `native_module_sig` (which now returns default-empty for both), plus the
post-match optional-tail install block. The **one subtlety over regex** — request's `get`/`post`/`request`
carry an OPTIONAL trailing `timeout_ms` — is spelled as a **trailing `= 0` default** in the `.chz`; harvest
PASS 2 counts trailing `default.is_some()` params and lowers to `FnSig::optional_tail` (min_params = len-1),
byte-identical to the deleted hand-built install. To admit that spelling, `parse_native` now calls
`parse_params(true)` (the grammar already permitted a param default in `<nativeDecl>`; the parser was merely
stricter — flipping it brings the parser INTO conformance, no `grammar.bnf` edit, conformance green). The
default EXPR is a **marker only** — desugar's `collect_module_reg` ignores `StmtKind::Native`, so it is never
injected at a call site (`arg_count()` stays truthful). `native fn`/`native struct` in a USER file is still a
clear checker error (stdlib-only hoist rejection fires before any default). The remaining hand-built runtime
layout copies (compiler `Compiler::new`, interp finalize, `native/process.rs`+`native/request.rs`,
`seed_stdlib_structs`) stay, **field-order drift-guarded** by `procresult_chz_matches_handbuilt_layouts` +
`response_chz_matches_handbuilt_layouts`. Import-gating (`ProcResult`/`Response` bare names licensed only by
importing their module) + the both-engine pure-type `bind_import` skip preserved by construction (harvest
forces origin=Builtin). **ZERO observable change / three-engine byte-identical** (checker/resolver-only cut):
`process_fn_sigs_exact` + `request_fn_sigs_exact` (sigs + StructInfo now come from the files, byte-equal to the
deleted arms; request's optional-tail min_params exact), `regex_sig_from_file_not_native_module_sig` (inverted
— asserts both arms gone), `request_optional_timeout_arg_typechecks` (both arities check), `native_fn_allows_optional_trailing_default`
(parser), `process_request_file_backed_three_engine_parity` + `pure_type_import_no_fault_both_engines`
(VM==interp==M:N), existing `examples/process_polish.chz` + `sys.chz` goldens unchanged on both engines.
**Roadmap (DONE):** after 4b/4f/4c-ffi/4c-net + **4c-concurrency** (the last migration), `native_module_sig`
retains only **`ffi`'s residual type-license tail** (`ptr` + fixed-width `int8..uint64`) + **`time`'s
`timer`** opcode-license. `grammar.bnf` unchanged (native-decl grammar + param defaults exist from 3a/4a;
conformance green).

**✅ NATIVE-PRELUDE — phase 4e (4 pure-function native modules made file-backed:
`std.encoding`/`std.crypto`/`std.uuid`/`std.time`) (2026-07-02).** REFACTOR-ONLY, **ZERO observable
behavior change / three-engine byte-identical** — a mechanical replay of the proven phase-4b regex
pattern onto four **pure-function** modules (no methoded types). Each now ships a **real
`std/<M>.chz`** whose current members are declared in-module as bodyless `native fn`s
(encoding: the 8 str↔str/`Result[str]` codecs + `query_encode(params: Map[str,str]) -> str`;
crypto: `sha256`/`md5`; uuid: `v4()`/`uuid_seed(n) -> nil`; time: `now`/`monotonic`/`sleep_ms(ms) ->
nil`/`format`). The resolver loads the real file (`visit_native_file`, fallible like the prelude) while
KEEPING the `native` marker, so **all runtime member dispatch stays name-keyed via
`native_members("std.M")` — bytecode + dispatch UNCHANGED**; the checker harvests each file's `native fn`
sigs via the existing `harvest_native_module`. This **RETIRED** the hand-built `std.encoding`/`std.crypto`/
`std.uuid` arms in `native_module_sig` (deleted — default-empty now) and reduced the `std.time` arm to
its **one load-bearing line**: `sig.types.insert("timer")`. `timer` is the sole subtlety — an
**opcode-backed builtin** (NOT a callable native member: no runtime value, lowers via the compiler's
name→opcode dispatch), so it is DELIBERATELY *not* declared as a `native fn` (that would bind a
nonexistent runtime value and fault); its import-license (`import timer from std.time` / `import std.time`
+ bare `timer(ms)`) is preserved by that minimal arm, harvest then filling the 4 real time fns on top.
The two file-backed gates (resolver `visit_native_file` + checker harvest) now share one predicate
**`crate::native::is_file_backed_native(name)`** ({regex,encoding,crypto,uuid,time}) so the file-source
and AST-source stay provably in lockstep. Import-gating preserved; none of the 4 are in `MODULE_FN_DOCS`
(`module_fn_docs_all_resolve` unaffected). Tests: `enc_crypto_uuid_time_sig_from_file_not_native_module_sig`
(provenance — arms gone, timer license kept), `enc_fn_sigs_exact`/`crypto_fn_sigs_exact`/`uuid_fn_sigs_exact`/
`time_fn_sigs_exact` (sigs byte-equal to the deleted arms; `-> nil` fidelity for sleep_ms/uuid_seed +
`Map[str,str]` for query_encode), `import_timer_from_std_time_still_licensed_both_forms`,
`golden_timer_selective_import_three_engine` (VM==interp==M:N), `phase4e_user_file_native_fn_still_rejected`,
existing goldens (`golden_encoding_crypto_via_run_file`/`golden_uuid_via_run_file`/`golden_timer_chz_matches_expected_and_interp`)
unchanged. `grammar.bnf` unchanged (native-decl grammar exists from 3a; conformance green).

**✅ NATIVE-PRELUDE — phase 4d (five pure-function native modules made file-backed: `std.math` /
`std.io` / `std.os` / `std.rand` / `std.fs`) (2026-07-02).** REFACTOR-ONLY (no new capability — the
proven phase-4b regex pattern applied to pure-function modules): each of the five is now a **real
`std/<M>.chz`** whose members are bodyless `native fn` decls reproducing the EXACT prior sig, instead of
a *file-less virtual* module with a hand-built `native_module_sig` arm. The resolver's import loop loads
each real file (via the new shared authority **`crate::native::is_file_backed_native`** — now covering
`{regex, math, io, os, rand, fs}` — swapped in for the `name == "std.regex"` special-case at
`resolver/mod.rs`), **KEEPING the `native` marker** so runtime member dispatch stays name-keyed via
`native_members("std.M")` — **bytecode + dispatch UNCHANGED**. The checker graph loop harvests any
`is_file_backed_native` module via the existing `harvest_native_module`, then runs the new
**`attach_native_module_metadata(name, &mut sig)`** on EVERY native module to re-attach the three pieces
a `native fn` decl can't express: (a) hover docs (`MODULE_FN_DOCS`, moved out of the deleted arm tail),
(b) module CONSTANT values `math.pi`/`e` (enumerated from `native::native_consts`, no hardcode), and
(c) numeric-poly fns `math.abs` (int→int/float→float) via the new `MODULE_NUMERIC_POLY` side-table
(parallel to `MODULE_FN_DOCS`). The five `native_module_sig` arms are **DELETED** (the fn returns
default-empty for them). **ZERO observable change / three-engine byte-identical** (checker/resolver-only
cut): `math_io_os_rand_fs_sig_from_file_not_native_module_sig` (arms gone),
`math_io_os_rand_fs_representative_sigs_exact` (fn sigs + pi/e values + abs poly byte-equal to the deleted
arms), `math_io_os_fn_hover_doc_preserved`, `math_io_os_rand_fs_runtime_tables_unchanged` (dispatch
tables + `native_consts` untouched), `math_is_file_backed_native` (resolver), and the 3-engine golden
`golden_std_native_4d_chz_matches_expected_and_interp` (`examples/std_native_4d.chz`, VM==interp==M:N).
`module_fn_docs_all_resolve` now builds the effective sig via the graph (the migrated fns are harvested).
`native fn` in a user file still rejected; `grammar.bnf` unchanged (conformance green). **Remaining
`native_module_sig` content after 4d/4e/4f/4c-ffi/4c-net + 4c-concurrency (the last migration):** only
`ffi`'s residual type-license tail (`ptr` + fixed-width `int8..uint64`) + `time`'s `timer` opcode-license.
(`net` migrated in 4c-net, `ffi` fns in 4c-ffi, `concurrency`'s four generic types in 4c-concurrency.)

**✅ NATIVE-PRELUDE — phase 4b (regex module made file-backed: native TYPE + FNs declared in
`std/regex.chz`) (2026-07-02).** NEW CAPABILITY (import-gated native **module members**): `std.regex` is
no longer a *file-less virtual* module — it is now a **real `std/regex.chz`** whose `native struct Match`
+ five `native fn`s (`is_match`/`find`/`find_all`/`replace_all`/`split`) are declared **in-module**,
exactly how `Ref` lives in `std/ref.chz`. The resolver's import loop loads that real file (fallible, like
the always-linked prelude) instead of `visit_native` injecting an empty AST, but **KEEPS the `native`
marker** (`native: Some("std.regex")`) so all runtime member dispatch stays name-keyed via
`native_members("std.regex")` — bytecode + dispatch **UNCHANGED**. The checker's native-module arm now
calls the new **`Checker::harvest_native_module`** (replacing `harvest_native_struct_stub`), which harvests
BOTH the `native struct` (→ `struct_defs`/`types`, `origin=Builtin` forced) AND the `native fn` sigs (→
`sig.functions`, the import-gated module-member surface) from the parsed in-module decls; a two-pass harvest
(transient `struct_names` insert during pass-2 so a fn return like `Result[Option[Match]]` resolves `Match`,
removed after → import-gating preserved). This **RETIRED** both the phase-4a companion stub
(`std/regex.stub.chz` + `harvest_native_struct_stub`, deleted) AND the hand-built `"std.regex" =>` arm in
`native_module_sig` (deleted — it returns default-empty for regex now). Match stays **import-gated**
(bare name licensed only by `import std.regex` / `import Match from std.regex`; `regex.Match(...)`
qualified); the 4 remaining hand-built runtime layout copies (`seed_stdlib_structs`, `Compiler::new`, interp
finalize, `native/regex.rs`) stay, **field-order drift-guarded** by `regex_chz_match_matches_handbuilt_layouts`.
**ZERO observable change / three-engine byte-identical** (checker/resolver-only cut): `regex_fn_sigs_exact`
(the 5 FnSigs + Match StructInfo now come from the file, byte-equal to the deleted arm),
`regex_sig_from_file_not_native_module_sig` (asserts the arm is gone), `std_regex_is_file_backed_with_native_marker`
(resolver), `regex_match_file_backed_three_engine_parity` (produce/field-read/`import Match from`/qualified,
VM==interp==M:N — locks the pure-type `bind_import` skip), existing regex goldens
(`golden_regex_demo_via_run_file`) unchanged. `grammar.bnf` unchanged (native-decl grammar exists from 3a/4a;
conformance green). **Roadmap:** `Response`/`ProcResult` are now DONE too (phase 4f — see the entry above).
net `Socket`/`Listener` are DONE too (phase 4c-net — the first methoded native types, native METHOD
binding built there). Remaining phase-4c = concurrency (`Shared`/`RwShared`/`Atomic`/`Executor`)
file-backed with `native struct` + method binding; of Tier-3, `Option`/`Result`'s variant SHAPE is now
file-backed too (phase 5b — a drift-guarded MIRROR; the `?`/match/construction WIRING stays Rust-wired,
see the entry above), and `Iterator` (a protocol + reserved value type, not an enum) stays native
(deferred to phase 5c).

**✅ NATIVE-PRELUDE — phase 4a (`native struct` syntax + companion-stub loader for file-less native
modules) (2026-07-02) — companion stub RETIRED in phase 4b (above).** NEW LANGUAGE FEATURE (the **type-level** analog of phase-3a `native fn`/`native
ctor`): `native struct Name:` with an indented block of **body-less field decls** declares a native
(Rust-backed) type's **checker signature** (field layout + type params) in Chezzi; the runtime layout +
method dispatch stay **native** (name-keyed). **Fields-only** for this cut (a `fn`/`test` method sig or a
field `= default` in the body is a parse error; bodyless native **method** sigs are phase-4b), **PRELUDE/
STD-ONLY** (a `native struct` in a user `.chz` is a clear checker error — *native struct declarations are
only allowed in standard-library modules*), TOP-LEVEL-only (parser; nesting reuses the existing depth>1
`Token::Native` guard). **COMPANION-STUB LOADER** (the general mechanism for **file-less** native modules):
`std.regex` is a *virtual* module — `resolver::visit_native` injects an empty AST, there is no
`std/regex.chz`. Its `Match` type's signature now lives in a **parse-only companion stub
`std/regex.stub.chz`** (embedded via `include_str!`), which is **never** added to the runnable module graph
(not always-linked, not executed) — `Checker::harvest_native_struct_stub` parses it solely to harvest its
`native struct` decls into `std.regex`'s `ModuleSig` (`struct_defs` + `types`), **replacing** the deleted
hand-built `"std.regex" => export_struct("Match", …)` arm in `native_module_sig` (the regex FUNCTIONS
`is_match`/`find`/`find_all`/`replace_all`/`split` STAY hand-built there; only `Match`'s StructInfo moved).
The harvest **forces `origin=StructOrigin::Builtin`** (load-bearing: drives `imported_builtin_types` on
import → both engines' name-keyed pure-type `bind_import` skip stays correct). Match stays **import-gated**
(bare name licensed only by `import std.regex` / `import Match from std.regex`; `regex.Match(...)` qualified)
— reuses the existing native-types additive pattern; **runtime layout + bytecode UNCHANGED** (the 5 hand-built
layout copies — `seed_stdlib_structs`, `Compiler::new`, interp finalize, `native/regex.rs` — stay, drift-
guarded by `match_stub_matches_handbuilt_layouts`). **ZERO observable change / three-
engine byte-identical** (checker/parser/grammar-only cut): new `regex_match_stub_migration_three_engine_parity`
(produce/field-read/`import Match from`/qualified, VM==interp==M:N), provenance + drift-guard + user-file-
rejected checker tests, parser tests, `grammar.bnf` gains `<nativeStructDecl>` + accept-corpus
`native_struct.chz` (conformance green). **Roadmap:** phase-4b = bodyless-**method**-sig→native binding
(analogous to native fn's proven bodyless-sig binding) + migrate the remaining Tier-2 native types
(`Shared`/`RwShared`/`Atomic`/`Executor`, `Response`/`ProcResult` + the rest of regex) fully out of
`native_module_sig`, and unify the remaining hand-built `Match` layout copies onto the stub. `native enum`
if ever needed. **Tier-1** (`Ref`) already done; **Tier-3** (`Option`/`Result`/`Iterator`) INTENTIONALLY
stays native (documented carve-out).

**✅ NATIVE-PRELUDE — phase 3a (`native fn`/`native ctor` syntax + always-linked `std/prelude.chz`)
(2026-07-02).** NEW LANGUAGE FEATURE (the north-star for FUNCTIONS made concrete): the internal analog of
`extern "lib":` (FFI). `native fn NAME(params) -> ret` declares a **first-class** universe-function
intrinsic (⇒ `Intrinsic::Builtin`, `first_class=true`); `native ctor NAME(params) -> ret` a
**non-first-class** scalar/type constructor intrinsic (⇒ `Intrinsic::Ctor`, `first_class=false`). Bodyless
(like an `extern` sig, NEWLINE-terminated), **PRELUDE/STD-ONLY** (a `native` decl in a user `.chz` is a
clear checker error — a user can't bind a name to a nonexistent intrinsic), TOP-LEVEL-only (parser). The
**eight** universe builtins (`ord`/`chr`/`panic` fns; `int`/`float`/`str`/`bytes`/`bytearray` ctors) now
declare their SIGNATURES in a real **`std/prelude.chz`** that the resolver **always-links** into every
graph (same seam as `std/ref.chz`, injected before the entry DFS so the entry stays LAST; deduped). The
signatures moved OUT of the Rust `make_sig`/`sig_*` fns into the parsed decls (harvested into the checker's
`native_prelude_sigs`, read by `Checker::builtin_sig`); the **hollow** Rust `PRELUDE` table keeps only
name→intrinsic→first_class METADATA (the backends `compiler::is_builtin`/`interp::builtins::is_builtin`
have no graph access and read only that). `print` stays the **one** synthetic function row (variadic).
**DYNAMIC-PARAM CONVENTION** (native-decl-scoped — introduces NO user-facing `any`/`never`): an
UNANNOTATED param = the dynamic "accepts anything" type (`Ty::Unknown`); a decl with NO `-> ret` =
native-controlled/never (`Ty::Unknown` return — how `panic` is spelled). **Backends UNCHANGED**: a `native`
decl compiles to NO bytecode / NO binding (skipped like `StmtKind::Extern` in compiler + interp; never a
callable user fn); direct calls to the eight names emit byte-identical `CallBuiltin`/`CallPrint` and
`vm::do_builtin` dispatch stays name-keyed. **ZERO observable change** — the drift guard
`prelude_table_is_single_source_of_truth` is extended to cross-check the parsed `.chz` decl set/kinds vs
the hollow table AND each parsed `FnSig` vs its historical shape; new `native_prelude.chz` three-engine
golden + parser/checker/resolver tests. `grammar.bnf` gains `<nativeDecl>` (conformance green).
**Roadmap (native-in-Chezzi track):** phase 2b (**DONE** — see below) folded the generic container
ctors' (`range`/`List`/`Map`/`Set`) DISPATCH into the table (type-identity stays in `resolve_type`).
**Phase 4a** (**DONE**) = `native struct` syntax + the companion-stub
loader, with `regex.Match` migrated (fields-only). **Phase 4b** (**DONE** — see the phase-4b entry above) =
`std.regex` made **file-backed** (`std/regex.chz`), native TYPE + FNs declared in-module, companion stub +
`native_module_sig` regex arm RETIRED. **Phase 4f** (**DONE** — see the phase-4f entry above) =
`std.process` + `std.request` made file-backed (`ProcResult`/`Response` + their fns), both `native_module_sig`
fn arms AND `export_struct` arms RETIRED; request's optional `timeout_ms` spelled as a trailing `= 0` default.
**Phase 4c** = bodyless native **method**-sig→native binding + migrate the remaining Tier-2 native
(Rust-backed) TYPES (`Shared`/`RwShared`/`Atomic`/`Executor`, net `Socket`/`Listener`) fully out of the
`native_module_sig` hand-tables (bodies still native), plus `native enum` if needed. **Tier-1** (the `Ref` struct-modeled type) is already done
(always-linked `std/ref.chz`). **Tier-3** (`Option`/`Result`/`Iterator`) INTENTIONALLY stays native —
too deeply coupled to `match`/`?`/generator desugar to express as a plain `.chz` decl; this is a
documented, deliberate carve-out, not a gap.

**✅ `modules.last() == entry` invariant hardened against always-injected prelude stubs (2026-07-02).**
The resolver `build_graph_with_entry_source` always-injects `std/prelude.chz` then `std/ref.chz` BEFORE
the entry DFS, so if the ENTRY file itself IS one of those stubs (`chezzi run std/prelude.chz`) its own
visit is deduped by `visited` and the graph would end mid-list — `graph.entry != modules.last()` — and
the positional-entry consumers (compiler `entry_idx = modules.len()-1`, both engines' `entry_home() =
modules.last()`) would designate the WRONG module as entry (for test-fn discovery / manifest `:function`
invocation). A localized guarded stable reorder in the resolver (right after `ModuleGraph` construction,
before `desugar::run`) now moves the `graph.entry` module to the tail iff it isn't already last — a
strict no-op for the normal case (entry is a user file, already last → zero behavior change), and stable
for all other modules so deps still precede dependents. This **removes the phase-3a latent-contract
follow-up** and **unblocks stacking more always-linked modules safely** in phase 4. Covered by resolver
tests (`entry_is_prelude_stub_still_designated_last` + ref forward-guard) and a three-engine run-clean
parity test (`entry_is_always_linked_stub_runs_clean_three_engine`: cooperative VM / `--parallel` /
interp all Ok, empty stdout, byte-identical). Behavior-preserving; three-engine parity.

**✅ NATIVE-PRELUDE TABLE — phase 2b (refactor-only, generic container ctors) (2026-07-02).** Folded the
**four GENERIC / reserved-type container CONSTRUCTORS** (`range`/`List`/`Map`/`Set`) into the `PRELUDE`
table as `Intrinsic::Ctor` rows with `first_class: false` — a mechanical mirror of phase 2a applied to
the last synthetic-table carve-outs, completing the goal that **every universe builtin's `CallBuiltin`
DISPATCH + name-set flows through the one table**. `compiler::is_builtin` + `interp::builtins::is_builtin`
drop the hard-coded `matches!(name, "range"|"Set"|"List"|"Map")` and become **pure table reads** (the
`prelude_fn` direct-call arm now emits their `CallBuiltin`, byte-identical to the old hard-coded arm —
type-args are type-erased before the compiler, so `List[int]()` == `List()` at the opcode level). Unlike
the scalars, these are **generic / carry reserved-type identity**, so — as the task required — they are
**table-sourced for DISPATCH ONLY, deliberately NOT `.chz`-declared** (native ctor generic-decl support
is a later, maybe-never concern): their generic **TYPE-IDENTITY** (`List[int]` → `Ty::List(Int)`, the Map
hashable-key check, range arity/overload) is **NOT a flat `FnSig`** and stays in
`resolve_type`/`infer_named_call`, with `builtin_container_sig` supplying only a flat display/placeholder
sig. Cross-link comments pin the split (table = dispatch, `resolve_type` = generic identity) and the drift
guard `prelude_table_is_single_source_of_truth` now **asserts it can't rot**: the table surface MINUS the
four container ctors equals the eight `.chz` decls + `print`, and each container ctor is a non-first-class
`Ctor` row that is NOT in the parsed `.chz` decl set. **ZERO observable change** — `range(5)`,
`range(1,10,2)`, `List()`/`List[int]()`, `Map()`, `Set([1,1,2])`, generic inference (`xs := List[str]()`),
reserved-type errors (a user `struct List` still rejected), value-position rejection (`f := List`/`f :=
range` still checker errors), and `range[int]()` still errors (Ctor membership is orthogonal to
`name_is_generic`) — all identical, identical bytecode. New `container_ctor_parity` two-engine test +
`container_ctor_not_firstclass_value` checker test + extended bytecode pin; `vm::do_builtin`
`builtin_range/list/map/set` dispatch **UNTOUCHED** (name-keyed). Container ctors are now table-sourced for
dispatch **though not `.chz`-declared** (generics).

**✅ `Ref` promoted to a RESERVED GLOBAL backing the `ref` keyword — import-free (2026-07-01).** The
`ref T` binding modifier and the explicit `Ref[T]` box now work with **no `import std.ref`**. `Ref`
joins `Result`/`Option`/`Iterator`/`Channel` in the reserved-global class (`is_reserved_type`) — the
sanctioned set that backs core syntax — so a user `struct Ref` is always rejected as reserved. Mechanism
(minimal, three seams, NOT a native rewrite — the `.chz` stays the single source): (1) the resolver
**always-links `std/ref.chz`** into every program's module graph (injected as `order[0]` before the entry
DFS in `build_graph_with_entry_source`, deduped if already imported; entry stays LAST so
`modules.last()==entry` holds); (2) the checker **caches std.ref's real `StructInfo`** (layout +
`get`/`set`/`update` from the checked module — `ref_seed`) and **re-seeds it bare** in every module's
`seed_stdlib_structs` (import-free `struct_names`/`bare_types`), with `is_reserved_type += "Ref"` and a
`current_module_is_stdlib` exemption so std.ref's own `struct Ref[T]` decl stays legal; (3) the compiler
and interpreter each expose `Ref` **bare in every module's `bare_types`** (guarded on the struct being
registered) so the ctor lowers import-free on all engines. `import std.ref` is now a **harmless no-op**
kept for compatibility (idempotent `bind_import` inserts — no dup/shadow error). Three-engine
byte-identical parity via new golden `examples/ref_no_import.chz` (ref keyword + explicit `Ref[int]` +
closure-capture aliasing; `run_file` == `interp` == `run_file_parallel`); checker tests
`ref_keyword_and_type_work_without_import` / `import_std_ref_is_harmless_noop` /
`user_struct_named_ref_now_reserved`. `ref T` semantics (Rc<RefCell> box, persists through closure
capture) unchanged — only the import requirement removed. Docs: `docs/syntax.md`/`docs/stdlib.md`.

**✅ Swift-style KEYWORD ARGUMENTS through a function VALUE (2026-07-01).** Named arguments now work
through a first-class **function value**, not just a direct call: `g := greet; g(name="Bob",
greeting="Hi")` prints `Hi Bob`, keywords may be reordered, and a `fn(name: str)->nil` **HOF parameter**
accepts `f(name="X")`. `Ty::Func`/`Type::Func` gained a `labels` field (parallel to `params`) — built
from a user fn's / closure's param names and from an annotation's optional `IDENT:` labels (parser
`parse_fn_type_param`). Labels are **SURFACE-ONLY** (Swift SE-0111): a new equality-neutral `FnLabels`
wrapper makes the derived `Ty` `PartialEq` ignore them, and `compatible`/`assignable`/`unify`/`Display`/
`sendable` all `..`-ignore them, so `fn(str)->nil` ≡ `fn(name:str)->nil` — **zero** regression to
HOF/callback/protocol/subtyping and no Display/snapshot churn. Resolution is a checker-recorded
**side table** (`KeywordTable = HashMap<KeywordKey, Vec<usize>>`, `KeywordKey = (module idx, fragment-ctx span, fragment ordinal, first-named-arg span)`) mirroring the `extern_sigs`
precedent EXACTLY: `resolve_keyword_calls{,_standalone}` run the same deps-first pass and harvest a slot
**permutation** over the combined `[positional ++ named]` arg list, populated in BOTH the single-module
(`ok`/`check_src`) and multi-module (`check_graph`) paths; both backends read it in `compile_call` /
`eval_call` to lower a value+keyword call to a **plain positional `Op::Call`** — the runtime ABI stays
positional and UNCHANGED (`src/vm` untouched — the `DeferCall`/`SpawnCall` lowerings consult the same
table so `defer d(name=…)` / `spawn s(name=…)` reorder too, no check-passes-then-traps hole). **SCOPE-CUT** (SE-0111): a value call must supply every
parameter — declaration-site **defaults do NOT fill through a value** (`h := hasdefault; h()` errors,
direct `hasdefault()` still fills); a first-class **built-in** value takes no keywords. Direct-call
keyword resolution (desugar), struct ctor/method named+default args, and `print` `sep=`/`end=` are all
UNCHANGED — desugar just stops rejecting value+keyword calls (Ident/expr callee) and defers them to the
checker. Positional value calls are untouched (the table is read only when `named` is non-empty → no
hot-path cost, `benches/run.chz` unchanged). Three-engine byte-identical parity
(`examples/keyword_value.chz` + a cross-module `keyword_value_xmod/`); grammar/`docs` updated
(`<fnParam>` optional label, conformance green).
  - **Fix (post-review):** two soundness holes in the above. (1) **Chained value keyword calls**
    (currying: `g(a=…)(b=…)` where a value returns another value) aliased one `KeywordTable` slot —
    the parser gives every link of a postfix chain the SAME call-node span (`parse_postfix`'s
    `let span = e.span;`), so the later permutation overwrote the earlier and the compiler/interp
    applied the wrong perm (out-of-range index → panic, or silent mis-route). The table is now keyed
    by a per-call-unique span (`checker::keyword_key_span` = the first named-arg VALUE expr's span,
    always present when recording), computed identically at the record site and all six backend
    lookups. (2) The **spawn airlock** sendability gate iterated only positional `args`, so a
    non-sendable value passed by LABEL to a spawned function value (`spawn h(f=cb)`, `spawn h(r=box)`)
    crossed unchecked while the positional form was rejected — the gate now chains `named` too.
    Regression tests: `golden_keyword_value_chz*` (chained curry line), `spawn_non_sendable_keyword_arg_rejected`,
    `spawn_non_sendable_ref_keyword_arg_rejected`.
  - **Fix (post-review #2):** the first-named-arg span above is unique only *within one lexed source*.
    Every `{…}` **string-interpolation** fragment is re-lexed from a fresh source, so its sub-expression
    spans restart at `(1,1)`; two value+keyword calls in different fragments whose first named-arg value
    lands at the same fragment-relative column (`"{a(y=1, x=10)} {b(p=3, q=2)}"`) collided on one
    `KeywordTable` slot and the earlier call was lowered with the WRONG permutation on all three engines.
    The key gained two **fragment discriminators** — the whole-string span + the fragment's 0-based
    ordinal — maintained identically by the checker (`check_interpolation`), compiler (`compile_str`),
    and interp (`interpolate`) at the interpolation boundary (inert defaults outside interpolation, so
    non-interpolation keying and the positional hot path are unchanged). `examples/keyword_value.chz`
    grew a colliding-offset interpolation line; regression test
    `keyword_value_interpolation_fragments_do_not_alias`.
  - **KNOWN LIMITATION (interp-only, accepted 2026-07-01):** the interp's `kw_frag_ctx`/`kw_frag_ord`
    are live mutable state set per interpolation fragment; they **leak** into callee bodies invoked from
    a `{…}` fragment and across a `recover:`-caught fault (save/restore only on the Ok path), so a
    value+keyword call reached that way is looked up under the wrong key → interp mis-resolves while the
    VM (static, resolves at compile time) is correct. **The user-facing engines (default M:N + `--serial`,
    both VM) are correct**; the divergence is only against the **deprecated interp parity oracle**, in the
    narrow `recover:`+interpolation+value-keyword combo, and no current golden test exercises it. Accepted
    rather than fixed because **interp is slated for removal** (decision: don't harden a dying engine).
    **When interp is deleted, also strip the frag-context machinery** (`kw_frag_ctx`/`kw_frag_ord` in
    `checker`/`compiler`/`interp`, ~47 refs) — the `KeywordTable` key simplifies to
    `(module, first-named-arg span)` since fragment discriminators only existed for the interp lookup.

**✅ First-class universe builtin FUNCTIONS `print`/`ord`/`chr`/`panic` (2026-07-01).** These four
universe functions are now **first-class values**: `defer print("World")` works as a bare call (the old
gate error *"built-ins and constructors must be wrapped in a function"* is gone for these names), and
they can be bound / passed like any function (`f := ord; f("a")`, HOF arg). Scope is **exactly** those
four — `len` stays method-only (`xs.len()`), and **type / container / runtime constructors** (`int`,
`str`, `List`, `Map`, `Channel`, `range`, …) plus user struct/enum ctors remain **non-first-class**
(still wrapped, uniform with `f := Point`). A new dedicated runtime value variant carries them:
`Obj::Builtin(Box<str>)` (VM) / `Value::Builtin(Rc<str>)` (interp) — pure-code, **SENDABLE** (crosses
the spawn airlock by cloning the name: cooperative VM via `SnapValue::Builtin`, M:N OS-thread engine via
the by-value `WireValue::Builtin`). Checker: `is_firstclass_builtin_fn` whitelist relaxes the `defer`
gate + types `infer_ident` in value position as a **dedicated `Ty::BuiltinFn { params, ret }`** (from
`builtin_sig`) for ALL FOUR uniformly. `BuiltinFn` is distinct from `Ty::Func` so it is BOTH sendable
(`sendable_rec => true` — a plain `Func` is conservatively non-sendable) AND, unlike `Ty::Unknown`,
rejected by `expect_bool` (so `if print:` is a type error, not a VM-truthy/interp-fault divergence); it
is HOF-compatible with a matching `fn(...)` param via `compatible`. Because `BuiltinFn` carries a fixed
signature, the **value form of `print` is a fixed 1-arg call** — the variadic/`sep=`/`end=` surface
stays direct-call-only (a bound value can't reach `CallPrintSep`). A **user binding shadows** these
names in value position: `is_reserved_name` bans only `fn`/type/import-alias decls (NOT `ord := 5`,
`fn f(ord: int)`, `for chr in xs`), so both runtimes match the checker by resolving
locals/captures/globals BEFORE the first-class arm (compiler `compile_ident` guards `LoadBuiltin` on
`resolve_local`/`captures`/`globals` misses; interp `eval` Ident tries `env.get` first); a same-named
**module global read before its definition line** is a use-before-def error (checker suppresses the
first-class arm when the name is in `module_global_lets`), matching a non-builtin `x := y` before `y`
— this closes a VM(`nil` slot)/interp(`Value::Builtin`) divergence. Compiler emits `Op::LoadBuiltin`
**only** for unbound value-position uses — DIRECT calls (`print(x)`, `ord(c)`) are intercepted before
the value fallthrough and keep their specialized `CallPrint`/`CallPrintSep`/`CallBuiltin` opcodes, so
the hot path + benches are untouched (no bench run needed). VM/interp `invoke_value`/`call_value` route
the value by name into the SAME logic direct calls use: `print` → space-join + trailing `\n` (arg kept
**GC-rooted on the operand stack** while stringifying, mirroring `do_print`); `ord`/`chr` →
`builtin_ord`/`builtin_chr`; `panic` → the recoverable `RuntimeError` (`Err`, never `Ok`) so defers
still unwind through a `panic()` value. Builtin-value **equality compares by name** on both engines (VM
`values_equal_guarded` gained an `(Obj::Builtin, Obj::Builtin)` arm — each `LoadBuiltin` allocs a fresh
handle, so identity was wrong; interp already name-compares via derived `PartialEq`). Two-engine
(three-engine incl. M:N) parity is byte-identical. Golden `examples/defer_builtin_value.chz` (+
`.expected`) exercises the behaviors on VM == `.expected` == interp == M:N; unit tests: rewrote
`defer_builtin_rejected` → `defer_builtin_accepted`, kept `defer_constructor_rejected`, added
`defer_type_rejected` / `type_name_not_firstclass_value` / `firstclass_builtin_fn_value_position` /
`panic_as_value_uncaught_raises_both_engines` / `ord_chr_as_value_both_engines` + regression guards
`print_value_not_usable_as_bool_condition` / `print_value_form_is_fixed_arity` /
`use_before_def_global_shadowing_builtin_rejected` / `user_binding_shadows_firstclass_builtin_typechecks`
(checker), `builtin_value_equality_both_engines` / `builtin_value_sendable_across_airlock_both_engines` /
`user_binding_shadows_firstclass_builtin_both_engines` / `print_as_value_arg_rooted_under_gc_stress`
(VM==interp==M:N). Docs: `docs/syntax.md` §`defer` (first-class list + value-form 1-arg limit +
sendable + shadowing + use-before-def). **Post-review parity fixes (2026-07-01):** (1) a first-class
builtin spawned as a **call callee** (`f := ord; spawn f("a")`, and bare `spawn print("hi")`) faulted
`spawn: 'function' is not an isolable task` on the **M:N engine only** — `prepare_worker`'s
`PendingCall::Call` arm handled only `Closure`/`Func`; added a `Lowered::Builtin` arm (crosses by name,
worker re-allocs `Obj::Builtin`), restoring three-engine parity. (2) The `spawn` gate now **accepts**
first-class builtins (symmetric with `defer`), and (3) `sep=`/`end=` on a deferred/spawned `print` are
a **type error** (the value form can't carry them) instead of being silently dropped. Tests:
`spawn_builtin_fn_value_as_call_callee_both_engines` / `spawn_bare_builtin_print_both_engines`
(VM==interp==M:N), `spawn_firstclass_builtin_accepted_like_defer` / `defer_spawn_builtin_named_args_rejected`
(checker). **What's next:** unchanged — M19 perf Tier-1 (method-call IC,
`run_until` trim, `Op::Call` specialization).

**✅ Resolver — deep-import-chain host-crash backstop (pre-JIT audit, 2026-07-01).** A pathological
*acyclic* linear import chain (~8-10k modules deep) recursed the resolver's DFS `Builder::visit`
(`src/resolver/mod.rs`) with no depth limit → host **stack overflow / SIGABRT** (`check` exited 134;
`run` printed `thread 'main' has overflowed its stack / fatal runtime error`). Import *cycles* were
already caught cleanly; this closed the acyclic-but-very-deep hole. Added `const MAX_IMPORT_DEPTH =
2000` (test-overridable via a `Builder.max_depth` field) guarding `visit` **after** the cycle+visited
checks — so cycle detection and diamond dedup are unregressed, and only DEPTH (`on_stack.len()`) is
bounded, not breadth. Exceeding it returns a clean `import chain too deep (exceeds 256)` diagnostic
attributed to the offending import (same shape as the cycle/missing-module arms). The checker's module
walk (`run_graph_pass`) iterates the resolver's already-flattened `graph.modules` linearly — no
independent recursion, so the single resolver guard covers both `check` and `run` (they funnel through
`resolver::build_graph`). Verified end-to-end on the 8MB main thread: a generated 2100-deep chain now
prints the clean diagnostic and exits 1 (no 134/SIGABRT) on both paths. TDD with an injected small
limit (the test-harness worker stack is far smaller than main — a real 2000-deep test would overflow
the *test* thread, per `parser::MAX_DEPTH`). Docs: `docs/spec.md` §Imports.

**✅ Turbofish construction on the value-first concurrency boxes `Shared`/`RwShared`/`Atomic`
(checker-only, 2026-06-30).** `Shared[int](0)` / `RwShared[Map[str, int]]({…})` / `Atomic[int](0)`
now type-check; the turbofish is **optional** (value-first inference still works with no type arg) and
when present **pins the element type, checked against the value** — `Shared[str](0)` is a type error
(`Shared[str]() expected element type str, found int`), and arity > 1 (`Shared[int, str](0)`) is
rejected. Reverses the prior "left OUT — `Shared`/`RwShared`/`Atomic` reject a `[T]` type arg" stance
of the container-ctor turbofish work. Two edits in `src/checker/mod.rs`: add the three names to the
`name_is_generic` whitelist (so a turbofish call clears the `'…' takes no type arguments` gate), and
route each value-first ctor arm through a new `concurrency_turbofish_elem` helper that mirrors the
`List[T]([…])` element-check pattern. **Runtime ctor/opcode dispatch UNCHANGED** (checker-only), so
VM↔interp (+ `--parallel`) parity holds by construction: `examples/shared.chz`/`atomic.chz`/
`rwshared.chz` converted to the turbofish form with **unchanged `.expected`**, exercised on both
engines by the existing goldens; value-first runtime stays covered by `examples/parallel_shared.chz`.
Out of scope (untouched): the global `Result`/`Option` ctors `Some`/`Ok`/`Err`, and `Executor`
(non-generic, stays rejected). Docs synced: `docs/stdlib.md`, `docs/syntax.md`, `docs/concurrency.md`.

**✅ Closure-capture model documented + golden-locked, pre-JIT (docs + golden only, 2026-06-30).**
No engine/checker change — the engines already implement the rule (`src/compiler/mod.rs:1604-1620`
`emit_load`: a local → `GetCaptured` snapshot, a global → `GetGlobalSlot` live read). Pinned the
**capture-by-binding-kind** rule before the JIT can freeze it: a **plain local** is captured **by value**
(snapshot at closure creation → `10`), a **global** is **not captured** but **referenced live** (current
value each call → `20`), and a **`ref` local** is captured **by reference** (shared box → `20`). New
three-engine golden `examples/closure_capture_scopes.chz` (+ `.expected` `10/20/20`) and its twin
`#[test] golden_closure_capture_scopes_chz_matches_expected_and_interp` in `src/vm/mod.rs` (VM ==
`.expected` == `interp::run_file` == `run_file_parallel`; runs via `run_file` through the real module
graph because the example uses `import std.ref` for the `ref int` annotation, which
`compile_module_standalone`/`run_capture` does not resolve). Reworded the over-claiming uniform-"snapshot"
header in `examples/closure_capture.chz` (and narrowed the `examples/edge_cases.chz` capture comment) to
the precise local/global/`ref` rule. Docs: a **capture subsection** (3-row table + example pointer) in
`docs/syntax.md` next to `ref T`. **Plus two doc-only clarifications:** (a) **float formatting never uses
scientific notation** — a plain `print`/`str`/`{x}` always renders the full decimal expansion
(`1.0e20` → `100000000000000000000.0`, `1.5e-9` → `0.0000000015`), shortest-round-trip-correct but verbose;
an intended Python-feel divergence, with `:e` available when an exponent is wanted (`docs/syntax.md`).
(b) **single project root** — `find_root` runs once on the entry and governs every import in the graph;
a nested `chezzi.toml` in a subdirectory is silently ignored (not a second root), so a root-level file
silently shadows a same-named subdir file (`docs/spec.md`).

**✅ Import-alias reserved-name gate + entrypoint-segment trim (diagnostic-only, 2026-07-01).** Two
disjoint checker/CLI-only fixes; no engine code, so VM↔interp parity holds by construction.
**FIX A — import-alias forms now honor the reserved-builtin-name guard** (`src/checker/mod.rs`,
`bind_import`). The guard that rejects `fn int()` / an extern named `int` as `reserved (builtin)` was
NOT applied to either import-alias form, so a reserved builtin *callable* could be silently rebound:
`import sqrt as int from std.math` was accepted, then the builtin `int()` conversion silently won and
the `as int` binding was dead (a SILENT WRONG RESULT — `print(int(9.0))` printed `9`, not `3.0`); and
`import std.math as int` was accepted, then failed with the confusing `module int is not callable`.
Both alias targets (`import M as X` and `import Y as X from M`) now run `is_reserved_name` and reject
`import alias 'X' is reserved (builtin)`. BOUNDARY held: value-level local shadowing (`range := 5`, a
fn param named `range`) goes through `declare` not `bind_import` and stays legal; the `a != member`
guard keeps a reserved member imported UN-aliased (`import Shared from std.concurrency`) / self-renamed
legal. Tests: `import_alias_to_reserved_int_from_rejected`, `import_module_as_reserved_int_rejected`,
`reserved_name_local_shadow_still_ok` (all via the `entry_*` build_graph→check_graph path).
**FIX B — entrypoint path segments are now whitespace-trimmed** (`src/main.rs` `entrypoint_file`).
Segments were trimmed only for the emptiness check but the RAW segment fed `module_file`, so
`entrypoint=" app "` slipped through to `<root>/ app .chz` → `cannot read ' app .chz'`. Each segment is
now trimmed before the path is built, so `" src . main "` resolves to `src/main.chz`; a whitespace-only
segment (`"a. .b"`) still trims to empty and is rejected. Test: `entrypoint_file_validates_dotted_path`
extended with the trim asserts.

**✅ Resolver diagnostic-quality fixes (diagnostic-only, 2026-06-30).** Two message/JSON fixes in the
module resolver error path; the accept/reject set is unchanged (resolve errors fire before any engine
runs, so two-engine parity is structurally untouched). **Bug 1 — missing-module / bare-`std` errors now
name the importing module.** A bad `import` inside a NON-entry module (e.g. `deep.chz` imported by
`main.chz`) previously printed `cannot find module 'x' (line N)` with no hint which file `line N` is in;
now it carries the same `in module 'deep':` prefix the parse/type errors use (via the existing
`prefix()` helper keyed on `on_stack.last()` = the importer). Entry-level imports stay unprefixed
(matches type-error attribution). **Bug 2 — `check --errors=json` resolve-error shape now matches
type-error JSON.** It previously emitted `{"message":"resolve error (line N, col M): ..."}` (the Display
prefix doubled into the message, redundant with the `line`/`col` fields); now the JSON `message` is the
clean body (with the Bug-1 `in module 'X':` attribution), while plain-text output keeps the
`resolve error (...)` Display prefix byte-identical. Implemented by carrying a clean `message` field on
`CheckOutcome::Fatal` alongside the rendered `text` (`src/main.rs`), JSON uses `message`, plain uses
`text`. New tests: `resolver::{missing_module_in_imported_module_names_importer,
bare_std_in_imported_module_names_importer}` + a negative entry-level guard on
`missing_module_is_clean_error`, and integration `tests/check_errors_json.rs` (CLI JSON shape +
plain-text via `env!("CARGO_BIN_EXE_chezzi")`).

**✅ Qualified type as static-method receiver + two-level-path diagnostics (additive, 2026-06-29).**
Two small ADDITIVE qualified-path improvements, break nothing.
**Part 1 — `module.Type.static_method()` now works** for cross-module struct AND enum statics
(`counter.Counter.zero()`, `col.Color.first()`), closing an arbitrary asymmetry (qualified
*construction* `module.Type(args)` already worked, but the qualified *static call* errored "module has
no member 'Type'"). Mirrors the bare `Type.static_method()` path exactly: checker adds a qualified
struct-static arm + reuses the qualified-enum-variant arm's no-variant fallthrough → `infer_static_call`
(variant-first preserved); compiler adds a Field-over-Field arm emitting the SAME `Op::CallStatic` keyed
by `type_key`; interp extracts `lookup_static_method_by_key` and adds the parity twin. Negative
`module.Type.no_such()` → "type 'Type' has no static method 'no_such'". Newtype statics stay unsupported
(struct/enum-gated); declaring one is now **rejected with a clear "not supported yet" error** at the
decl site + any `Newtype.method()` call site (was a cryptic "unknown name" — see the checker-leniency
note in Current focus).
**Part 2 — clear two-level-path diagnostics** for the natural 3+-level mistake (import paths *are*
multi-level, so users assume type refs are too). TYPE position (`x: std.concurrency.Shared[int]`): the
parser detects a third `.` after a qualified type and emits the targeted hint instead of cryptic
"expected '=', found '.'". EXPR position (`std.concurrency.Shared(0)`): a new checker `import_path_heads`
map (head segment → dotted path + bound name, populated in `bind_import`) turns the misleading
"unknown name 'std'" into the two-level hint; narrow — fires ONLY for a literal import-path head, never a
real typo. No grammar.bnf change (both surfaces stay two-level; Part 1 is an existing parse, Part 2 is
error-text only) — conformance green. Tests: `src/checker/mod.rs` graph_tests
(`qualified_type_struct_static_ok` / `_enum_static_ok` / `_unknown_rejects` + 3 KEEP-WORKING regressions +
`multilevel_expr_*` positives/negatives), parser `multilevel_type_path_two_level_hint`, three-engine
golden `examples/qualified_static/` (VM/interp/M:N byte-identical). Docs: `docs/syntax.md`.

**✅ First-class native (Rust-implemented) types — qualified / aliased module-member path (additive,
2026-06-29).** The import-gated native types/ctors — `Shared`/`RwShared`/`Atomic`/`Executor`
(std.concurrency), `Socket`/`Listener` (std.net), the FFI widths `int8`..`uint64` + `ptr` (std.ffi),
and `timer` (std.time) — are now reachable by the **two-level qualified / aliased module path**, exactly
like a `.chz` module type (`geo.Point`) or `regex.Match`: `concurrency.Shared[int]` / `concurrency.Shared(0)`,
`import std.concurrency as c` → `c.Shared(0)`, `type S = concurrency.Shared[int]`,
`newtype MyS[T] = concurrency.Shared[T]`, `net.Socket` annotation, `ffi.int32` (incl. inside an `extern`
signature), `time.timer(0)`. **ADDITIVE** — the existing bare-after-import licensing
(`imported_concurrency`/`_net`/`_ffi_types`/`_time`) is byte-unchanged, examples/grammar.bnf untouched,
and the import gate stays sound (qualified access to a non-imported module is still `unknown module`).
Implementation (all small/localized): (1) checker `resolve_type` `Type::Qualified` arm maps a
`sig.types` builtin name → its builtin `Ty` (shared helper `qualified_builtin_ty` + arity check; `timer`
in type position → "function, not a type"); (2) `resolve_qualified_ro` mirrors it for the RO export path
(exported alias/newtype bodies); (3) `resolve_ctype_d` `Type::Qualified` arm maps `ffi.int32`/`ffi.ptr` →
`CType::Int32`/`Ptr` for extern sigs; (4) `infer_call` Field-callee qualified-ctor arm delegates to
`infer_named_call` (Socket/Listener/widths/ptr → "has no constructor" reject); (5) compiler Field-callee
arm lowers `module.Ctor(args)` to the SAME opcode as the bare name, keyed on
`program.modules[tidx].native` (NewShared/NewRwShared/NewAtomic/NewExecutor/NewTimer); interp gets a
parity twin (`construct_native_ctor`). `bind_import` skips (VM + interp) untouched — a qualified ctor
lowers to an opcode, no runtime module-member lookup. **TWO-LEVEL ONLY** (parser is two-level for every
module; `std.concurrency.Shared` is out of scope). **Future:** retiring the bare-name licensing is its
own later milestone (one-way ratchet). Tests: `src/checker/tests.rs`
(`qualified_native_type_annotation_resolves` / `qualified_native_ctor_call_infers` /
`alias_and_newtype_over_qualified_builtin` / `ffi_qualified_width_in_extern_sig` + unlicensed/timer/Socket
negatives), three-engine goldens `examples/native_qualified.chz` (VM/interp/M:N) and
`examples/ffi_qualified.chz` (CLI-verified libc `abs(ffi.int32)`). Docs: `docs/syntax.md`, `docs/stdlib.md`.

**✅ Checker — Closure-parameter type inference (v1) + structural-match-over-`Unknown` soundness close
(2026-06-28).** Checker-only, three-engine-parity-safe by construction (rejected programs never run;
accepted programs byte-identical). **Supersedes** the earlier `MatchKind::Skip`/`OpenScrutinee`
exhaustiveness patch (that `OpenScrutinee` variant is **removed**). Unannotated closure params used to
infer as `Ty::Unknown` — the only place `Unknown` reached a runtime value — so call sites went
unchecked and a structural `match` over such a param **check-passed then trapped** on BOTH the VM and
`--serial` (`g := fn(x): match x: E.A: …; E.B: …` then `g(5)` → `cannot match on int`); a trailing `_`
could not rescue it (the destructure runs first). **Fix (5 phases):** (1) `infer_closure` gained an
`expected: Option<&Ty>` checking-mode — an unannotated param binds to the slot's param type — wired
through every `fn`-typed slot: call args (`check_args_range_w` → covers `Shared.update`/`RwShared`),
native list HOFs (`map`/`filter`/`fold`/`sort_by`/`sort_by_key`), and generic ctor/variant/fn/method
arg loops (`infer_generic_arg_tys` + `check_generic_arg`, re-inferring the closure against the
substituted field/param type, first-pass body errors suppressed). (2) the remaining slots —
`fn`-typed `let`/`:=`, struct `fn`-field assignment, `fn`-typed return. (3) **free**-closure inference:
a shallow body scan pins a param from (source #2) a `match` whose scrutinee is the **bare param**
(first concrete arm) or (source #3) a member access **uniquely owned by one type** — a `str`/`bytes`
method (`fn(x): x.upper()` → `x: str`) or a field/method exactly one user struct declares — not from
arithmetic/comparison/indexing or any member shared by >1 type (`x.len()` on `str`/`list`/`map`/`set`
never pins; they fit many types, so they're *checked*, never pin). (4) **§4.1
structural-over-`Unknown` reject** at `bind_subpattern` (nested tuple elements + variant/`Ok`/`Err`/
`Some`/`None` payloads, inherited by or-alts/guards) PLUS `match_kind`/`reconstruct_unknown_kind` (the
top-level residual-`Unknown` scrutinee arm) — `cannot match a <tuple|variant> pattern on a value of
un-inferable type; annotate it`; literal/range/`_`/binding sub-patterns over `Unknown` stay allowed
(value-compare/bind never traps). This **flips** the old `OpenScrutinee` accept-heterogeneous-literals
behaviour: the first literal arm now pins the scalar, so `1` + `"b"` rejects. (5) a genuinely
unresolved free closure param errors `cannot infer type of parameter 'x'; add a type annotation`.
**Soundness follow-up:** a closure passed to a **generic** slot whose type param only *it* binds
unifies that param to `fn(Unknown) -> Unknown`, so the substituted expected param type is `Unknown` —
an `Unknown` expected param is **not** a pin (it would re-open the launder-to-runtime hole:
`store(fn(a): a + 1)` for `fn store[T](x: T) -> T` check-passed then trapped on both engines). It now
falls through to the body scan / annotation rule and rejects at `check`. (Unification's first-pass
`infer_generic_arg_tys` keeps closure params `Unknown` via a `generic_arg_prepass` guard so the free
scan can't corrupt unification, e.g. `Mapped(int_iter, fn(x): x.upper())` still errors `no method
'upper'`, not an element-type mismatch.)
End-to-end verified on both engines: every reject errors at `check` (never executes — no trap); every
accept runs byte-identically (corpus: `iterable`/`iter_adapters`/`shared`/`parallel_shared`/`rwshared`/
`fn_field`). Tests in `src/checker/tests.rs` (real `build_graph`+`check_graph` CLI path) + migrated
graph_tests. Docs: `docs/syntax.md` (Closure-parameter inference + §match).

**✅ Docs + resolver polish (2026-06-28).** Two low-severity fixes: (1) `docs/syntax.md` "Generic
newtypes" `Stack[T].top` example used a Python-style **postfix** ternary
(`return None if … else Some(…)`) that does not parse in Chezzi (only the **prefix** `if c: a else: b`
conditional-expression form exists) — rewritten to `return if xs.len() == 0: None else: Some(…)`,
verified `Some(3)`/`None` on both engines (syntax.md code blocks are not conformance-executed, hence
the slip). (2) `src/resolver/mod.rs` — a bare `import std` routed through `module_file` to
`<install>/std.chz`, ignoring any project-local `std.chz` and leaking the internal install path; now
emits `'std' is a reserved namespace (import a submodule, e.g. 'std.math')` (narrow guard, submodules
like `std.math`/`std.x.y` unaffected). TDD: new `bare_std_import_is_reserved_namespace` resolver unit
test (RED → GREEN). No checker changes; parity-safe by construction.

**✅ Checker — operator overloading + protocol satisfaction on GENERIC structs/enums (2026-06-28).**
A generic type that defined an operator method (`add`/`sub`/`mul`/`div`/`mod`/`neg`/`compare`) could
**call it directly** but could NOT use the matching operator (`a + b`, `-a`, `a < b`), satisfy the
protocol (`Add`/.../`Comparable`), or flow into a protocol-bounded generic (`twice[T: Add]`) — `check`
*and* both engines rejected with `cannot apply + to Box[int] and Box[int]` / `does not satisfy Add
(method 'add' has the wrong signature)`. Non-generic types worked, and `Stringable`/`Hashable` worked
on generics (their sigs never mention the type param) — the exact asymmetry that proved it was a
generic-substitution bug, not a missing feature. **Root cause:** `satisfies_methods` (checker, shared
front-end) substituted only the protocol's own params (`pmap`) + `Self` into the comparison; the
RECEIVING type's own param→arg map (e.g. `T→int` from `Box[int]`) was never threaded, so the user's
stored method `add(self, o: Box[T]) -> Box[T]` (params kept UNsubstituted) failed
`compatible(Box[int], Box[T])`. **Fix:** build `tymap` from `ty` itself (struct via `struct_param_map`,
enum via `enum_param_map`, newtype via `newtype_type_params`) and pre-substitute it into the ACTUAL
(user) method signature before `method_matches`. Only the actual side is bound, so a genuinely wrong
sig (`add(self, o: int) -> int`) STILL fails — no laundering. The newtype operator-soundness gate
(generic newtype operators stay intentionally method-only/unreachable) is untouched (the fix lives
after that early-return). Parity-safe by construction (one shared checker; no per-engine logic). TDD:
new checker tests (generic struct/enum add/neg/compare, multi-param, wrong-sig boundary) + twin golden
`examples/generic_operator_overload.chz` (run byte-identical on VM, interp, parallel). `docs/syntax.md`
already documented this as working — the bug was the gap between spec and checker; now closed.
**Two soundness boundaries hardened in the same change** (adversarial-review findings): (1) the operator
now requires **matching type ARGS**, not just the same type name — `op_overload_result`/`ordering_allowed`
test `compatible(l, r)` (name + pairwise targs, `Unknown` still unifies) instead of `name == name`, so a
heterogeneous `Box[int] + Box[str]` / `Box[int] < Box[str]` is REJECTED (admitting it would infer result
`Box[int]` for a value built from a `Box[str]` → runtime type confusion). (2) `Comparable` is added to the
newtype operator-soundness gate: a same-newtype `<` ALWAYS auto-flows to the underlying's NATIVE ordering
(`compare_op`'s `same_newtype_keys` fast path), never a user `compare`, so a **generic newtype**'s `compare`
stays unreachable as an operator and must NOT claim `Comparable` (else check-ok / run-divergent). Both
boundaries covered by new failing-first rejection tests.

**✅ Checker — import+same-name-struct collision soundness hole closed (2026-06-28).** Checker-only,
three-engine-parity-safe by construction (rejected programs never run; accepted programs byte-identical).
The four native **struct-modeled** types (`Ref`/`std.ref`, `Match`/`std.regex`, `Response`/`std.request`,
`ProcResult`/`std.process`) slipped through the decl guard: (**NOTE (2026-07-01):** `Ref` has since been
promoted to a **reserved global** — always import-free, a user `struct Ref` is now *always* reserved; see
the top "reserved global backing the `ref` keyword" entry. The other three stay import-gated as below.) a program that BOTH imported one AND declared a
same-named `struct` passed `check` clean then **trapped at runtime on both engines** (e.g. `no field 'v' on
Ref(value=5)`) — the user layout overwrote the Builtin seed in the hoist while the runtime kept constructing/
returning the native shape. Root cause: the struct-hoist `already_defined` test (`mod.rs`) only treats a
*User*-origin prior as defined, so a name IMPORTED as a Builtin-origin layout was silently overwritten; the
enum/newtype/typealias decl paths were already closed via their `struct_names` collect-name guards. Fix
(approach (b), minimum-correct — NOT full reservation, which would break the module-owned bare-decl intent +
the origin-keyed sendability check): a new per-module `imported_builtin_types` set, populated at the two
struct-import insert sites (whole-module `import std.regex` + selective `import Match from std.regex`) keyed on
`info.origin == StructOrigin::Builtin`, consulted in the struct-hoist reserved-name gate → a same-named user
`struct` is now rejected `type 'X' is reserved (builtin)` (Socket/Shared precedent). Generalized: the gate also
closes the identical latent hole for every other import-gated std struct (Token/Parser/Heap/Deque/…). A bare
unimported `struct Ref` (no import) and a merely-similar name (`struct RefBox` with `import Ref`) both stay
legal. Tests: `import_plus_same_name_struct_decl_rejected` (all four + whole-module form),
`import_does_not_over_reject_distinct_struct_name`, `bare_struct_procresult_without_import_ok`; existing
`user_struct_response_without_import_ok` / `user_struct_match_without_import_ok` / `user_struct_named_ref_is_sendable`
/ `from_import_licenses_bare_response` stay green. Docs: `docs/syntax.md` module-owned note.

**✅ Manual feature-audit sweep — 3 correctness bugs found + fixed, playbook documented (2026-06-27).**
A structured adversarial hand-audit of the feature domains the *automated* oracles can't reach
(generics, `match`/enums, closures, protocols, namespace/import gating — `src/difftest/generate.rs`
emits none of these). Fanned out parallel agents per domain, each probing edge cases on BOTH engines +
`check`, evidence-gated. Found and fixed: **(1)** a `match` **exhaustiveness soundness hole** — guarded
arms (`A if c`) and refutable payloads (`Some(0)`, `Pair(0,y)`) wrongly closed a variant, so a
non-exhaustive `match` passed `check` then **faulted at runtime** (commits in
[`src/checker`](src/checker/mod.rs) `bind_match_arm`/`bind_subpattern`; a nested **single-variant**
payload stays irrefutable, verified); **(2)** the namespace name-leak (entry above); **(3)** polish —
NaN `-NaN` via the format-spec path, and a misleading "earlier push" diagnostic for an un-inferred
type param. All TDD, two-engine-parity-green, merged via `auto-task` → `post-merge-gate` (2821 tests).
The repeatable method — domains, per-agent protocol, bug taxonomy, and the procedure gotchas (verify the
CLI via `cargo run --bin chezzi` not a hardcoded `target/` path; an `ok()` unit test passing ≠ the CLI
is correct; adversarially verify every fix) — is now the **"Manual feature-audit playbook"** in
[`docs/bug-discovery.md`](docs/bug-discovery.md) (lever #9). Run it every pre-freeze session.

**✅ Checker — namespace/import-gating, two more holes closed (2026-06-28).** Checker-only, parity-safe
by construction (rejected programs never reach the VM/interp; accepted programs are byte-identical — NO
runtime/opcode change). **HOLE A — protocol-name type decls:** the 15 prebuilt PROTOCOL names
(`Comparable`/`Stringable`/`Hashable`/`Error`/`Add`/`Sub`/`Mul`/`Div`/`Mod`/`Neg`/`Arithmetic`/`Iterable`/
`Index`/`IndexSet`/`Slice` — `Iterator` was already blocked incidentally via `is_reserved_type`) could be
declared as `struct`/`enum`/`newtype`/`type` alias because the five type-DECL guards consulted only
`is_reserved_type`/`ffi::TYPE_NAMES`, never `is_reserved_protocol`. A `struct Comparable` silently shadowed
the protocol and produced a self-contradictory diagnostic (*type Comparable does not satisfy Comparable
(missing method 'compare')*). Added `|| is_reserved_protocol(name)` to all five decl guards (NewType,
TypeAlias, Struct, Enum, NewType-with-methods) → now reject `type 'X' is reserved (builtin)`, uniform with
every other reserved type. The protocol BOUND (`[T: Comparable]`) and a type-PARAM named like a protocol
stay legal (only the standalone TYPE decl is reserved). **HOLE B — bare `owned_str`:** `resolve_type` mapped
`owned_str => Ty::Str` UNCONDITIONALLY, so `fn f(x: owned_str) -> owned_str` checked clean and silently
collapsed to `str` with no import (its sibling `ptr` correctly errors). `owned_str` is a RETURN-ONLY extern
marshalling form, not importable — gated by CONTEXT (not import): an `in_extern_sig` flag set around the
extern fn signature loop licenses the arm there; a bare non-extern use now errors *'owned_str' is a
return-only extern marshalling type and cannot be used as a general type annotation*. Extern returns (no
import) + the `id[owned_str]` type-param shadow + the extern-param surface guard all unchanged. Tests:
`protocol_named_types_rejected_at_decl` (15 names × 4 decl forms + the literal repro),
`protocol_bound_and_typeparam_named_protocol_still_ok`, `bare_owned_str_outside_extern_rejected`,
`extern_owned_str_return_still_ok_no_import` (graph path). Mirrors the 7241b5e/1fde673 reserved-type
precedent.

**✅ Checker — builtin-type namespace name-leak, two holes closed (2026-06-27).** Checker-only,
parity-safe by construction (mirrors the landed std.concurrency/std.time/std.ffi gates; NO runtime/opcode
change beyond two byte-identical `bind_import` skips). **HOLE A — decl-guard incomplete:** `is_reserved_type`
blocked only 8 names while `resolve_type` maps ~16 bare names to builtins, so `struct int` / `enum List` /
`struct Socket` type-checked clean then silently shadowed the builtin at the use-site. Extended
`is_reserved_type` to the full builtin scalar (`int`/`float`/`bool`/`str`/`bytes`/`bytearray`/`nil`),
container (`List`/`Set`/`Map`/`Channel`/`range`), and handle (`Socket`/`Listener`/`ptr`/`owned_str`) set,
and added the `native::ffi::TYPE_NAMES` (FFI width names like `int32`) check to the struct+enum decl guards
(mirroring NewType/TypeAlias) — all four decl forms now reject with `type 'X' is reserved (builtin)`.
**HOLE B — std.net types ungated:** `Socket`/`Listener` resolved to `Ty::Socket`/`Ty::Listener`
UNCONDITIONALLY (no `import std.net`), unlike `Executor`/`Shared`/`ptr`. Added an `imported_net` per-module
licensing set wired from the whole-module `import std.net` arm + a per-name `import Socket from std.net`
branch (rename rejected), with a `net_licensed` helper gating the `resolve_type` arm; unlicensed bare use now
errors `unknown type 'Socket' (import it from std.net: ...)`. Runtime `bind_import` skip added to BOTH vm +
interp so `from std.net import Socket` doesn't fault (the type carries no module-member value). Production
blast radius zero (all std.net examples already import it). Tests: `reserved_builtin_type_names_rejected_at_decl`,
`bare_net_type_without_import_hints_import`, `net_type_with_import_ok`,
`net_type_from_import_partial_does_not_license_other`, `net_type_rename_rejected`,
`vm::tests::net_from_import_runs_both_engines` (both engines).

**✅ Checker — reserved builtin TYPE names rejected as generic type-PARAMETER names (2026-06-30).**
Checker-only, three-engine parity by construction (rejected programs never reach codegen/runtime; no
vm/interp edits). Closed the last reserved-name discipline hole: the five decl guards applied
`is_reserved_type` only to the declared type NAME, so a type-PARAMETER named after a builtin type
(`struct Box[int]` / `[List]` / `[Result]`, `enum E[int]`, `newtype N[List]`, a method's own `[U]`,
`protocol P[int]`, the FFI width `[int32]`) type-checked clean and then shadowed kind-dependently — a
scalar param was dead/unreferenceable (the scalar wins in `resolve_type`), a container/enum-builtin
param silently SHADOWED the builtin as a real generic. This **reverses commit 9829f94** (which had
deliberately made such params shadow-and-run) to honor the one-way-ratchet rule (*a reserved builtin
type name must error `reserved (builtin)`, not silently shadow*). New `reject_reserved_type_params`
helper (predicate = `is_reserved_type` + `ffi::TYPE_NAMES`, span = the param-name token) called once
per decl at the five hoist sites (struct/enum/newtype/fn_sig/protocol — hoist-only so fired exactly
once, no double-report). Scope: reserved builtin TYPE names only — a type-param named like a prebuilt
PROTOCOL (`fn id[Comparable]`) and a protocol BOUND (`[T: Comparable]`) stay legal (unchanged,
guarded by `protocol_bound_and_typeparam_named_protocol_still_ok`). Normal `[T]` / `[K, V]` / word
params untouched. Tests: `reserved_builtin_type_names_rejected_as_type_params`,
`type_param_named_like_reserved_type_rejected` (inverted from the old not_shadowed guard),
`reserved_typeparam_fix_does_not_overreject` (boundary), `vm::parity_tests::
type_param_named_like_reserved_rejected_at_check` (inverted; full build_graph+check_graph CLI path).

**✅ Editor tooling — LSP hover for the FREE-FUNCTION decl name (2026-06-30).** Closes the lone
decl-site gap left by Tier-A point (5), which recorded a decl-name hover only for METHODS
(`record_method_decl_hover`, called from the struct/enum/newtype arms) — a FREE function name
(`fn foo(...)`) still hovered nothing, even though its params, return type, and the call site already
did. Fix is one probe-gated block in `check_fn_body` (the single funnel every free fn AND method routes
through): build `Ty::Func { params: sig.params.clone(), ret: sig.ret.clone() }` and
`hover_record_at(decl.name_span, &fty, HoverKind::Func, sig.doc.clone())` at the existing
runtime-inert `FnDecl.name_span`. For methods this is a harmless no-op — `record_method_decl_hover`
latches the receiver-stripped sig FIRST (first-hit-wins in `hover_record_at`), guarded by the unchanged
`hover_method_decl_name` test asserting `fn() -> int` with `self` stripped. No `!generic_arg_prepass`
gate is needed: `check_fn_body` never runs under the generic-arg prepass (proven green by
`hover_generic_free_fn_decl_name`, which Displays `fn(T, T) -> T` with no `?` latch). Checker/editor-only,
probe-gated → zero runtime/typecheck/codegen/VM/interp change, two-engine parity green, goldens
byte-identical, conformance unchanged (no syntax/grammar change). Tests:
`editor::tests::hover_free_fn_decl_name`, `hover_free_fn_decl_name_shows_doc`,
`hover_generic_free_fn_decl_name`, and end-to-end `lsp_smoke::hover_fn_decl_name_round_trip`.

**✅ Editor tooling — LSP hover for the ENUM-VARIANT decl name (2026-06-30).** Sibling of the free-fn
decl-hover note above: hovering a variant name at its declaration (`Val` in `enum Col:\n    Val(int)`)
showed nothing, while the USE site (`Col.Val(3)`) already hovered its ctor signature. Root cause: the
`Variant` AST node carried no `name_span` (unlike its sibling `Field`), so the token position was
unrecoverable at check time. Fix is additive + runtime-inert, mirroring `Field.name_span` /
`FnDecl.name_span`: (1) add `pub name_span: Span` to `ast::Variant` (diagnostic-only, never read by
desugar/compiler/vm/interp; derived `PartialEq` kept — identical source ⇒ identical spans); (2) the
parser captures the variant-name token span; (3) the `StmtKind::Enum` arm, under the existing
`hover_probe` guard, loops the variants and records `hover_record_at(v.name_span, &Ty::Func { params:
<resolved payload>, ret: Ty::Enum(name, targs_disp) }, HoverKind::Func, None)` — reusing the EXACT
type construction `infer_variant_call` uses at the use site, so decl-site and use-site displays agree
(`Val(int)` → `fn(int) -> Col`, generic `Full(T)` → `fn(T) -> Box[T]`, nullary `Red` → `fn() -> Col`).
No `!generic_arg_prepass` gate needed (this arm runs in `check_stmt`, not the localized inference
prepass — proven by `hover_generic_enum_variant_decl_name`). A variant has no doc field, so doc=None
(only the signature surfaces, NOT variant doc-comments). Checker/editor-only, probe-gated → zero
runtime/typecheck/codegen/VM/interp change, two-engine parity green, goldens byte-identical, conformance
unchanged (the AST-only field adds no surface syntax). Tests:
`editor::tests::hover_enum_variant_decl_name`, `hover_generic_enum_variant_decl_name`,
`hover_nullary_enum_variant_decl_name`, and end-to-end `lsp_smoke::hover_enum_variant_decl_name_round_trip`.
**Reinstall the LSP snapshot to serve it: `cargo install --path . --features lsp --bin chezzi-lsp`.**

**✅ Editor tooling — LSP hover for IMPORTED user types + GENERIC annotation heads (Tier-C follow-up) (2026-06-30).**
Closes the two "No information available" type-name hover gaps the Tier-C entry below flagged. (a) An IMPORTED
user type (`import Heap from std.collections`): its own decl docstring now crosses the module boundary — added an
editor-only `doc: Option<String>` to `StructInfo`/`EnumSigInfo`/`NewTypeSigInfo`, populated in `capture_sig` from
the defining module's `name_docs`. `bind_import`'s From-arm records the import-line token hover at the bound name
(`record_imported_type_hover`) — the type's own docstring, else a `kind (from module)` fallback (`struct (from
std.collections)`) — AND seeds `name_docs[bind]` so later bare/annotation/generic-head uses surface the same doc.
(b) A GENERIC annotation head (`xs: List[int]`, `h: Heap[int]`): the `Type::Generic` arm now carries the head-name
token span (added an equality-NEUTRAL 3rd `Span` field to AST `Type::Generic`, exactly like `Type::Named`'s span —
the hand-written `PartialEq` ignores it, no engine/codegen ever reads it) and records a probe-gated, `!generic_arg_prepass`
head hover REUSING `builtin_type_doc` for builtin heads (`List`/`Map`/…) and falling back to `name_docs` for user
heads. The existing `Type::Named` hover also gained the `name_docs` fallback, so a non-generic imported type used
as `x: Foo` surfaces its doc too. Checker + 1 AST field + parser plumbing; every doc is an `Option<String>` into
`hover_record_at` (probe-gated no-op off-probe) → ZERO runtime/typecheck/codegen/VM/interp change, two-engine parity
green, goldens byte-identical, conformance unchanged (surface syntax identical). Tests: `editor::tests::hover_generic_
annotation_head_shows_doc`, `hover_imported_type_shows_doc`, `hover_imported_generic_head_shows_doc` (+ Tier-A/B/C
regressions intact). Validated end-to-end against the worktree-built `chezzi-lsp` over JSON-RPC (the protocol the
nvim client speaks): hovering `Heap` (import line + `Heap[int]` head) and `List` in `xs: List[int]` all return the
doc. **Reinstall the LSP** (`cargo install --path . --features lsp --bin chezzi-lsp`) to pick it up.

**✅ Editor tooling — hover on the import-line token for native/reserved TYPE imports (2026-06-30).**
`import Shared from std.concurrency` (and `RwShared`/`Atomic`/`Executor`, `import Socket/Listener from
std.net`, `import ptr`/width-types from std.ffi) showed "No information available" when hovering the
imported NAME on the import line — those per-name branches license the type via the per-module sets and
short-circuit BEFORE the user-struct import arm that records a hover. New
`record_native_type_import_hover` records that token hover with the type's `builtin_type_doc` blurb (else
a `(from <module>)` fallback) and its resolved native `Ty` for display. The bare/annotation use already
worked (the `Type::Named`/`Type::Generic` hover arms read `builtin_type_doc`); this fills the import-line
gap. `import timer from std.time` is handled too — `timer` is a reserved FUNCTION (`timer(ms) ->
Channel[bool]`), so it records a function-style hover (not the type path). Imported MODULE FUNCTIONS
(`from std.rand import randint`) and VALUES already recorded an import-line hover (their signature/type;
doc only where `MODULE_FN_DOCS` covers the module — std.math/io/os today). Probe-gated/editor-only →
parity-neutral, goldens byte-identical. Tests: `editor::tests::hover_native_type_import_shows_doc`,
`hover_timer_import_shows_func_doc`. **Minor remaining gap:** a USER type-ALIAS imported by name
(`from M import Len` where `type Len = …`) records no import-line hover yet.

**✅ Editor tooling — hover Markdown escapes bare bracketed type refs (2026-06-30).** The LSP renders
the hover doc body as Markdown, so a bare type reference in a doc-comment (`Heap[T]`, `List[T]()`,
`xs[i]`) was being eaten as link syntax (`[text]` / `[text](url)` — `List[T]()` is literally an
empty-URL link) and shown as `HeapT`/`ListT`. `chezzi-lsp::escape_brackets_outside_code` now
backslash-escapes `[`/`]` that are OUTSIDE an inline code span and outside a fenced block (so
`` `List[T]` `` and fenced code stay verbatim), applied after `untag_fences` in the hover render path.
Tests: `escape_brackets_outside_code_escapes_bare_type_refs`, `..._leaves_code_spans_and_fences`;
validated end-to-end in headless nvim. **Reinstall the LSP** to pick it up. Also: `install.sh` now
installs `chezzi-lsp` (feature-gated) alongside `chezzi`.

**✅ Editor tooling — LSP hover docs for BUILTIN/STDLIB types & stdlib module fns (Tier C) (2026-06-30).**
Hovering a builtin/stdlib TYPE or ctor (`List`/`Map`/`Set`/`str`/`bytes`/`bytearray`/`Channel`/`Shared`/
`RwShared`/`Atomic`/`Executor`/`Socket`/`Listener`/`range`/`tuple`/`Result`/`Option`/`Iterator`) now shows
a concise one-line usage blurb, and — for a type with a built-in method table — an appended `methods: a, b, c`
line. The method-name lists come from authored `const *_METHODS: &[&str]` slices beside the `*_method_sig`
fns, each drift-guarded by `checker::tests::builtin_method_slices_all_resolve` (every listed name must resolve
from its `*_method_sig`, so the hover can't advertise a method that doesn't exist). New `fn builtin_type_doc(name)`
builds the blurb; it's threaded as the `doc` arg at the by-name CALL-callee hover site (covers `List[int]()` etc.)
and the bare Type-token hover site (covers `str`/`bytes`/`Executor`/bare `Shared`…), both already
`hover_probe.is_some()`-gated so the doc is built ONLY under a probe. Stdlib MODULE FUNCTIONS (`math.sqrt`…)
now hover with a doc too: authored `MODULE_FN_DOCS` slices set `FnSig.doc` (excluded from `fn_sig_eq`) inside
`native_module_sig`, surfaced unchanged via the existing `record_method_hover` — coverage is **`std.math` /
`std.io` / `std.os`** for v1 (drift-guarded by `module_fn_docs_all_resolve`); the other native modules hover
doc-less for now (follow-up). **Skipped (task-sanctioned): protocol per-method docs** — a doc-comment above a
`protocol` method sig still does NOT surface; AST `MethodSig` carries no `doc` field, so it'd need a parser +
`grammar.bnf` + conformance + new hover-site change (multi-file, out of Tier-C scope). **Known v1 gaps:**
`list.sort` and `bytes`/`bytearray.extend` are real methods handled in `infer_method_call` (not the `*_method_sig`
tables), so they're intentionally absent from the hover `methods:` lists. (The generic-annotation-head and
imported-user-type hover gaps noted here in the original entry are now CLOSED — see the follow-up entry directly
above.) Checker+editor only; every doc is an `Option<String>` passed to
`hover_record_at` (probe-gated no-op off-probe) → zero runtime/typecheck/codegen/VM/interp change, goldens
byte-identical. Tests: `editor::tests::hover_builtin_type_list_shows_methods`, `hover_builtin_type_token_str_shows_doc`,
`hover_module_fn_sqrt_shows_doc`, `hover_builtin_does_not_break_user_doc` (Tier-A fallback intact) +
`hover_struct_decl_name_shows_doc` (Tier-A regression). **Reinstall the LSP** (`cargo install --path . --features
lsp --bin chezzi-lsp`) to pick it up.

**✅ Editor tooling — LSP hover for TYPE tokens in annotations (Tier B) (2026-06-30).** Hovering a
TYPE token in an annotation now shows the RESOLVED type — `x: Id` (the `Id` → `int` if `type Id = int`),
a param type `fn f(a: int)` (the `int`), a return type `fn f() -> P` (the `P`), a struct field type
`x: int`, a `let` annotation `x: int = 5`. Almost no new code: `Type::Named { name, span }` already
carries a name-token span (its prior reader was the semantic-token overlay) and `resolve_type` already
computes the resolved `Ty` for every annotation — so the fix is a single probe-gated `hover_record_at(
*name_span, &resolved, HoverKind::Type, None)` in the `Type::Named` arm, recording at the inner
name-token span (NOT the enclosing-annotation `span` param). Gated `self.hover_probe.is_some() &&
!self.generic_arg_prepass`: the probe gate keeps off-probe checks free in this hot path (resolve_type
runs per annotation per check), the prepass gate stops the generic-arg unification prepass from
first-hit-wins latching an incomplete type. Display follows `Ty::Display`: a transparent
`type Id = int` shows `int` (consistent with the Tier-A alias-decl hover), a struct name shows the
struct, an in-scope type param shows the param. Composite inner names fall out for free — the
`Type::Generic`/`Func`/`Tuple` arms recurse into `resolve_type`, so the `int` in `List[int]` records at
its own span. New `HoverKind::Type` variant. **Known gap (partly CLOSED):** `Type::Qualified` (the `Point`
in `geo.Point`) still carries no name-token span, so it doesn't hover; the OUTER generic head (`List` in
`List[int]`) gap was CLOSED by the Tier-C follow-up (a head-name `Span` field was added to `Type::Generic`).
Inner type args hover via the recursive `resolve_type`. Checker/editor-only, zero runtime/codegen/parity impact (goldens
byte-identical). Tests: `editor::tests::hover_type_alias_transparent`, `hover_param_type_token`,
`hover_return_type_token`, `hover_field_type_token`, `hover_struct_name_type_token`,
`hover_generic_inner_type_token`, `hover_generic_fn_param_type_no_latch` (prepass-latch guard),
`hover_type_kind_is_type`. **Reinstall the LSP** (`cargo install --path . --features lsp --bin
chezzi-lsp`) — the editor binary is a snapshot.

**✅ Editor tooling — LSP hover for the five decl-site NAME tokens (Tier A) (2026-06-30).** Five
decl-site name positions that returned `None` now hover, all via the established additive-`Span`
precedent (`Field.name_span` / `Param.name_span` / `For.var_spans` / `Pattern::Ident(_, Span)`): a new
diagnostic-only span captured by the parser at the name token, then a probe-gated `hover_record_at` in
the checker — every new span is runtime-inert (never read by desugar/compiler/vm/interp), so VM↔interp
parity + all goldens stay byte-identical. (1) **type-decl name** — `struct P:` / `enum Col:` /
`newtype UserId = int` / `type Id = int` / `protocol Bar:` add `name_span: Span` to the five
`StmtKind` decl variants; the checker pass-2 arms record the decl's own `Ty` (`struct`/`enum`/`newtype`
self-ty, the aliased ty for `type`, `Ty::Protocol` for `protocol`) + the decl's doc-comment at the
name token (`HoverKind::Struct`, now PRODUCED). (2) **generic type-param decl** — `fn id[T]` /
`struct Box[T]` / method `[U]` add `name_span: Span` to `TypeParam`; the single `enter_type_params`
funnel records `Ty::Param("T")` (the bound suffix `T: Comparable` is not representable through the
`Ty`-only hover channel — bare param name only). (3) **import bound name** — `import std.math` (the
`math`), `import std.math as m` (the alias), `import sqrt from std.math` (the `sqrt`) add
`name_span`/`name_spans` to `Import::Module`/`Import::From`; `bind_import` records `Ty::Module` for the
module name and the imported fn/value type for `from`-members. `Import` gets a hand-written
equality-neutral `PartialEq` (the bound-name spans don't flip equality — `Type::Named` precedent).
From-import **type-only** members (e.g. `import Point from geo`) are not hovered (only fn/value
members) — a deliberate scope cut. (4) **assign-LHS** — `i = i + 1` records the target's type at the
simple-`Ident` lvalue span (no AST change). (5) **method decl name** — `fn dbl(self) -> int:` records
the call signature (receiver stripped for instance methods, kept for statics) at the method-name token,
matching the call-site method hover. New parser helper `parse_dotted_path_spanned` (allowlisted in
conformance — same `dottedPath` grammar). Tests: `editor::tests::hover_struct_decl_name`(`_shows_doc`),
`hover_enum_decl_name`, `hover_newtype_decl_name`, `hover_type_alias_decl_name`,
`hover_protocol_decl_name`, `hover_type_param_decl_fn`/`_struct`, `hover_assign_lhs`,
`hover_method_decl_name`, `hover_import_module`(`_alias`), `hover_from_import_name`. **Reinstall the LSP
snapshot to serve it: `cargo install --path . --features lsp --bin chezzi-lsp`.**

**✅ Editor tooling — LSP hover for the two remaining binding decl-sites (2026-06-29).** The last
two binding decl-sites that returned `None` now report their inferred type, closing out the
decl-site-hover batch (after the for-loop/param/field work): **(A) tuple-destructure** (`a, b := (1,2)`
→ hover `a` or `b` = `int`) and **(B) match-pattern binds** (`Col.Val(n)` → hover `n` = the payload
type; tuple pattern `(a, b)` → each element's type). Both follow the for-loop `var_spans` precedent
EXACTLY — purely additive, runtime-inert span metadata. (A) adds `name_spans: Vec<Span>` to
`StmtKind::Let` (parallel to `names`; `Span::default()` for synthesized/desugar lets), captured by the
parser at each binding token; `check_destructure` zips it and `hover_record_at`s each tuple-element type
(single-name let path unchanged — no regression). (B) changes `Pattern::Ident(String)` →
`Pattern::Ident(String, Span)` (the binding token's span), captured in `parse_subpattern`; the checker's
`bind_subpattern` `Pattern::Ident` arm records the hover at the binding's OWN span before `declare`. The
new `Span`s are never read by either engine (patterns route by NAME / lets lower by `names`/`value`/`ty`),
so VM↔interp parity and every golden stay byte-identical; the grammar is syntax-only (`IDENT` lists /
pattern idents) so conformance is untouched. Tests: `editor::hover_destructure_first`/`_second`,
`hover_single_let_regression` (guard), `hover_match_variant_bind`, `hover_match_tuple_bind`.

**✅ Editor tooling — LSP hover for param + struct-field DECL sites (2026-06-29).** Hovering a
parameter at its DECL site in a signature (free fn, method, OR closure) and a struct field at its DECL
site previously returned `None` (only the body USE / field-access resolved); now both report the
declared type, checker-only and probe-gated (each addition is a `hover_record_at` call — a no-op when
no probe is armed — or inside `if self.hover_probe.is_some()` → NO type-check/codegen/VM/interp change,
two-engine parity untouched). (1) **fn/method param decl** (`fn f(a: str)` → hover `a` = `str`) — one
`hover_record_at(param.name_span, …, HoverKind::Param, …)` in `check_fn_body`'s param loop, covering
free fns AND methods (both route through it); (2) **closure param decl** (`fn(a: int): …` → `a` = `int`)
— same call in `infer_closure`'s param map; (3) **struct field decl** (`struct P:\n  x: int` → `x` =
`int`) — a probe-gated loop in the `StmtKind::Struct` arm reading already-resolved field types from
`self.structs` (no re-resolve → no duplicate errors). `HoverKind::Param` is now PRODUCED (a param's
body-USE still reports `Local` — different span, first-hit-wins). The **qualified-static receiver**
(`module.Type.method()` → method sig) and **container ctors** (`List[int]()` / `List()` / `Map[K,V]()` →
display sig) were already covered (qualified-static threads through the same `infer_static_call` record;
`List[int]()` parses as a bare-`Ident` callee reaching `callee_display_ty`→`builtin_sig`) — added
regression guards. Tests: `editor::tests::hover_fn_param_decl`, `hover_method_param_decl`,
`hover_closure_param_decl`, `hover_struct_field_decl`, `hover_container_ctor_turbofish_callee`,
`hover_container_ctor_bare_callee`, `hover_map_ctor_turbofish_callee`. **Reinstall the LSP snapshot to
serve it: `cargo install --path . --features lsp --bin chezzi-lsp`.**

**✅ Editor tooling — LSP hover for value-producing call/ctor/static/receiver sites (2026-06-27).**
Hover on four call categories previously returned `None`; now they report a signature, checker-only and
probe-gated (every addition is inside `if self.hover_probe.is_some()` or routes through `hover_record_at`,
a no-op when no probe is armed → NO type-check/codegen/VM/interp change, two-engine parity untouched):
(1) **newtype constructor** (`UserId(10)` → `fn(int) -> UserId`) — a newtype branch in `callee_display_ty`
(the existing bare-Ident callee record site fires it, symmetric with the struct-ctor branch);
(2) **enum-variant constructor** (`Col.Val(3)`: variant name `Val` → `fn(int) -> Col`) — `infer_variant_call`
records the variant's ctor sig at the variant-name span (threaded a `name_span` through it + `infer_named_call`);
(3) **static method** (`Foo.default()`: `default` → `fn() -> Foo`) — `infer_static_call` records the declared
sig at the method-name span (threaded `name_span` through all four call sites);
(4) **receivers** of `Col.Val(..)` / `Foo.default()` → the enum/struct type name (`Col` / `Foo`). The
bare-builtin callee case (`print`/`range`/`chr`/…) was ALREADY covered via `callee_display_ty`→`builtin_sig`;
`len(...)` is method-only (not a free fn) so it stays an undefined-name error (out of scope). Tests:
`editor::tests::hover_newtype_ctor_callee`, `hover_enum_variant_callee`, `hover_enum_variant_receiver`,
`hover_static_method_callee`, `hover_static_method_receiver`, `hover_builtin_callee_chr`. **Reinstall the LSP
snapshot to serve it: `cargo install --path . --features lsp --bin chezzi-lsp`.**

**✅ Editor tooling — LSP hover on the for-loop binding decl-site (2026-06-27).** Hovering the loop
variable at its declaration (`for i in …` — the `i` right after `for`) now reports its inferred element
type (e.g. `int`), matching the body use-site that already worked. Root cause: `StmtKind::For` stored
`vars: Vec<String>` with no source span, so the checker had no token position to record a hover at. Fix
is purely additive metadata: a parallel `var_spans: Vec<Span>` field on `StmtKind::For` (one span per
name, mirroring `Param.name_span`/`Field.name_span`), captured by the parser via `cur_span()` before each
binding ident; the checker zips it with the declare loop and calls `hover_record_at` (a no-op unless a
probe is armed → zero overhead on normal checks). `var_spans` is never read at runtime by either engine,
so VM↔interp parity and every golden are byte-identical; comprehension-synthesized `for`s use
`Span::default()` (no decl-site hover — out of scope, intended). Tests: `editor::hover_for_binding_decl`
(decl-site) + `editor::hover_for_binding_body` (use-site regression guard).

**✅ Editor tooling — doc-comments on LSP hover (2026-06-27).** A plain `#` comment block *immediately
above* a declaration is now its DOC-COMMENT, rendered on LSP hover ABOVE the existing `chezzi` type
fence. No new marker (`#`, not `##`/`///`); multiline via stacked `#` lines (join with `\n`, one leading
`# ` stripped); **attachment rule**: the doc is the *contiguous* run of comment lines with NO blank line
between the last one and the decl — a blank line detaches earlier comments; an inline trailing comment on
the decl line is never a doc. **Lexer side-channel** (NOT new tokens): `tokenize_with_comments` captures
`(line, stripped_text)` for each comment-only line on the side, so the token stream + `chezzi tokens`
output stay byte-identical (only `resolver::parse` opts in via `parse_with_docs`; every other
`tokenize`/`parse` caller gets `doc = None`). **Coverage:** `doc: Option<String>` on `FnDecl` (covers
free fns + every method/static/associated fn since they all reuse `FnDecl`) and `StmtKind::{Let, Struct,
Enum, Protocol, NewType, TypeAlias}` (the doc is *parsed + attached* for all of these; top-level bindings
carry it and surface on hover, local bindings carry the field but are inert).
**Inert/parity:** the doc is purely informational — never read by desugar/compiler/vm/interp, so two-engine
VM==interp parity is untouched (front-end-only). **Hover wiring:** `FnSig.doc` (free fns + methods) +
`Checker.name_docs` (struct constructors + top-level bindings, simple-name keyed, entry-module-scoped like
`self.functions`) feed a 3rd element into `hover_result`/`HoverInfo.doc`; `chezzi-lsp` renders the doc as
plain markdown lines above the untagged fence. **Shadow-safe:** a `name_docs` doc surfaces only when the
hovered name actually resolves to the module top-level (scope 0) — a param/local that shadows a documented
global's name shows no doc, not the global's. **Fence-safe:** user doc text is run through `untag_fences`
before rendering, so a fenced block (```` ```lang ```` or `~~~lang`) inside a doc-comment can't reintroduce
the language-tagged fence the type fence avoids (Neovim injection crash, commit `0f36a59`). **Known v1 limit:** only `fn`/method, struct-constructor, and top-level-binding docs actually surface on
hover. The doc is parsed + attached for `enum`, `protocol`, `newtype`, and `type` aliases too, but does
NOT yet reach the popup — enum-variant constructors (`Field`-access form) and newtype constructors record
no callee hover signature (`callee_display_ty` has no enum/newtype branch), and protocol/type-alias names
have no value/expression form to hover. Separately, protocol METHOD signatures
(`MethodSig`, not `FnDecl`) get no per-method doc — only the protocol container does. Builds on the
builtin-hover plumbing below. **Reinstall the LSP snapshot to serve it: `cargo install --path . --features
lsp --bin chezzi-lsp`.**

**✅ Editor tooling — LSP hover for builtins (2026-06-27).** Hover on a builtin callee/method/stdlib-fn
previously returned `None`; it now reports a signature, via three reuse-driven cases (no flat
hand-table), checker+editor only (NO VM/interp/runtime touch → no two-engine parity risk):
(1) **builtin methods** (`str`/`list`/`map`/`set`/`Channel`/`Shared`/`RwShared`/`Atomic`/`bytes`/
`bytearray`/`Executor`/`Socket`/`Listener`) record their CALL signature off the SAME `*_method_sig`
helpers that drive inference (zero drift) via a new `record_method_hover` probe helper in each
`infer_method_call` builtin arm; (2) **stdlib-module fns** (`math.sqrt`, …) record off
`native_module_sig(module).functions` in the `Ty::Module` arm; (3) **free/ctor builtins** (`print`,
`range`, `int`/`float`/`str`, `ord`/`chr`, `panic`, `List`/`Set`/`Map`/`bytes`/`bytearray`,
`Channel`/`Shared`/`RwShared`/`Atomic`/`timer`/`Executor`) get a NEW DISPLAY-only `builtin_sig(name)`
(mirrored by hand from `docs/stdlib.md §1`; polymorphic-input slots render `?`, the concrete return is
the payload, e.g. `print`→`fn(?) -> nil`, `range`→`fn(int) -> List[int]`) consulted by
`callee_display_ty` before its `None`. **Drift guard:** `is_reserved_name` is refactored onto a
`const RESERVED_CALLABLE` slice (behavior-identical) and a test asserts every name in it has a
`builtin_sig` entry, so a future reserved builtin can't silently lose hover. `Ok`/`Err`/`Some` are NOT
reserved (user-shadowable) → still hover `None` for v1. Signature-only (no docstrings — a separate
follow-up). **Reinstall the LSP snapshot to serve it: `cargo install --path . --features lsp --bin
chezzi-lsp`.**

**✅ M22 — operator protocols (Div/Mod/Neg) + protocol embedding + `Arithmetic` (2026-06-26).** Three
new per-operator protocols wired exactly like `Add`/`Sub`/`Mul`: **`Div`** (`div(self, o: Self) ->
Self`, powers `/`), **`Mod`** (`mod`, powers `%`), **`Neg`** (`neg(self) -> Self`, powers UNARY `-`).
`int`/`float` satisfy all three intrinsically; structs/enums via the method; scalar newtypes get
`Div`/`Mod` auto-flow (Neg out of scope). Soundness: a newtype operator overload defined as a *method*
is never dispatched at runtime (the same-newtype arm always auto-flows to the underlying's native op),
so the checker does NOT satisfy `Add`/`Sub`/`Mul`/`Div`/`Mod`/`Neg` on a newtype structurally — only
via the numeric auto-flow — closing a `check`-ok / `run`-faults hole. C-style `/` truncates / `%` int-remainder, so `Div`/`Mod`
are `Self -> Self` (no float-return surprise). **Protocol embedding (super-protocols)** — a protocol
body line is now EITHER an `fn` sig OR an embed line (`Add + Sub`, order-free, interleaved); reuses
`Bound`. `ProtocolInfo`/`StmtKind::Protocol` gained `embeds: Vec<Bound>`. Satisfaction flattens
transitively (memo-free recursion, depth-cap 64) — a type satisfies P iff it satisfies every embed AND
has every OWN method; a pure bundle (embeds, no methods) short-circuits. Bound-site flattening via a
new `bound_provides` helper makes `+ - * /` legal inside an `[T: Arithmetic]` body and lets an
`Arithmetic`-bound value forward into `[U: Div]`. **Collision rules** validated declare-time (second
hoist pass, after all protocols registered so forward/cyclic refs resolve): own-fn-vs-embed = error;
same-method same-sig embed diamond dedups silently (`Arithmetic + Add` legal); differing-sig embed =
error; cyclic embed = error. Builtin **`Arithmetic`** bundle = `Add + Sub + Mul + Div`, built with the
same `embeds` field (no special-casing). `Div`/`Mod`/`Neg`/`Arithmetic` + the previously-omitted
`Error` are now reserved protocol names. Both-engine operator dispatch (vm `struct_arith` + `Op::Neg`;
interp mirror); golden `examples/arithmetic_protocol.chz` runs byte-identical on vm/interp/parallel;
grammar.bnf `protocolDecl` updated (+`tests/corpus/accept/protocol_embed.chz`, conformance green).
Surface in [`docs/syntax.md`](docs/syntax.md), [`docs/spec.md`](docs/spec.md) (M22 row).

**✅ Editor tooling — LSP server + VSCode TextMate grammar (2026-06-26).** Two highlight/diagnostic
paths, both single-sourced from the lexer so language changes flow through with no separate grammar to
maintain. (1) **`chezzi-lsp`** — a `tower-lsp` stdio language server (`src/bin/chezzi-lsp.rs`, primary
target neovim) providing **diagnostics** (lexer+parser+checker over the live buffer via
`chezzi::editor::diagnostics`, `CheckError`/`ResolveError` 1-based spans → 0-based LSP ranges, pushed on
open/change/save), **semantic tokens** (lexer `Tok` stream → base
`keyword/operator/string/number/comment/variable` legend, then an **AST-derived overlay** refines each
ident to `function` (fn-decl names + call/ctor/method callees), `type` (plain named-type references —
struct/enum/scalar names — in annotations/returns/fields/payloads/bounds; generic-constructor heads
and module-qualified names are span-less and stay `variable`, see limits), `property` (struct field
decls + field accesses), or
`parameter` (fn/closure params) — the legend is extended with those four names and a `chezzi-lsp`
`#[cfg(test)]` test asserts the server legend and `editor::SEMANTIC_TOKEN_TYPES` agree index-for-index;
a buffer that doesn't parse yields an empty overlay and degrades to lexer-only highlighting, never
erroring), and
**hover** (`K` / `textDocument/hover` → `chezzi::editor::hover`: reverses the UTF-16 cursor column to a
char column, finds the lexer token under the cursor, re-runs the SAME resolve→desugar→check pipeline as
diagnostics with a single-position checker PROBE on the entry module, and returns the inferred type of
the smallest leaf/identifier/field-name **or the signature of a call's callee** (free fn / struct ctor /
generic fn / user method — receiver stripped → `fn(int) -> int` — **and builtins**: free/ctor builtins
(`print`→`fn(?) -> nil`, `range`→`fn(int) -> List[int]`, …) + builtin-collection/concurrency methods
(`"x".upper()`→`fn() -> str`) + stdlib-module fns (`math.sqrt`→`fn(float) -> float`)) as a
```` ```chezzi <type> ```` MarkupContent — `None` only when the position has no type, lands on a bare
enum-variant callee (or `Ok`/`Err`/`Some`, which are non-reserved), or the program
doesn't check). The probe is a minimal, behavior-preserving checker
introspection (`checker::hover_type` + a `HoverKind` classification); diagnostic-only AST spans carry
the token positions — `ExprKind::Field { name_span }` plus new `FnDecl`/`Param`/struct-`Field`
`name_span`s and a `Type::Named { span }` (the latter with a hand-written equality-neutral `PartialEq`
so position never flips type equality) — all runtime-inert and parity-neutral.
**Reinstall (`cargo install --path . --features lsp --bin chezzi-lsp`) to pick up hover.** The
async deps (tower-lsp + tokio) are OPTIONAL behind a `lsp` feature with `[[bin]] required-features`, so
they never touch the default `cargo build`/`cargo test`; build on demand:
`cargo build --features lsp --bin chezzi-lsp`. (2) **VSCode TextMate grammar**
(`editors/vscode/syntaxes/chezzi.tmLanguage.json`) **generated** from the lexer's new
`KEYWORDS`/`PUNCTUATION` tables + `Token::lexeme()` — `tests/editor_tmlanguage.rs` is generator +
CI drift-guard (`UPDATE_EDITOR_ASSETS=1 cargo test --test editor_tmlanguage` regenerates; a plain run
fails if stale). Architecture: **`src/lib.rs` is the crate of record** (`pub mod editor` + the front-end module set);
both binaries are thin shims that link it — `src/main.rs` (the `chezzi` CLI) and
`src/bin/chezzi-lsp.rs` declare no front-end modules, they `use chezzi::{…}`, so the front-end
compiles **once** (its unit tests + two-engine parity + grammar `conformance` run once, in the lib
test target — no more lib+bin double-compile/double-run) — plus a
behavior-preserving `resolver::build_graph_with_entry_source` (entry from a live buffer, imports from
disk). Editor logic (`src/editor/`) is dep-free and unit-tested in the default build; the LSP server has
a `cargo test --features lsp --test lsp_smoke` JSON-RPC round-trip. Setup docs: [`editors/README.md`].
No parity risk (front-end only; never runs VM/interp). v1 limits: unsaved edits to an *imported* module
aren't reflected until saved; interpolated strings highlight as one literal (no nested `{expr}`); hover
covers the entry module only, resolves leaf idents/literals/field-names + single-name bindings +
call callees incl. **user-struct** method names, **reserved free/ctor builtins**, **builtin-collection/
concurrency methods**, and **stdlib-module fns** (generic/enum/newtype user methods,
desugared `?.`/`??`, and non-first destructuring-bind names return no type; the semantic-token overlay's
`type` role skips generic-constructor heads (`List[int]`'s `List`, `Map`/`Set`/`Box`, …) and
module-qualified type names, both span-less).

**✅ Bug-discovery lever #1 — front-end panic-fuzzer (2026-06-26).** `src/panicfuzz/` feeds
adversarial / malformed inputs to `chezzi check` (the full front-end: lexer + parser + checker) and
flags any crash. A **stable, dependency-free SUBPROCESS harness** structurally mirroring
`src/difftest/` (own `xoshiro256**` RNG copy; same reader-thread + `try_wait` + kill-on-timeout
machinery) — *not* `cargo-fuzz` (no nightly / rustup / cargo-fuzz here) and *not* in-process
`catch_unwind` (the crate is binary-only — no `[lib]` — and shelling out catches more crash classes
incl. **stack overflow**, the most likely deep-parser crash). Invariant: malformed input ⇒ a clean
diagnostic, never a Rust panic (`panicked at` on stderr) or a signal kill (exit code `None` =
SIGSEGV/SIGABRT/stack-overflow); a wall-clock timeout is **not** a finding. Three bounded (≤2 KB),
deterministic generators (`generate.rs`): random UTF-8-ish bytes; a token-alphabet sampler (Chezzi
keyword/punct/op spellings + idents/numbers/indent); raw-byte mutation of the `examples/*.chz` corpus.
A finding reports the seed + raw triggering input, reproducible via `panicfuzz --seed N` (the input is
the artifact — no shrink pass in v1). Wired as `tests/panicfuzz.rs` (classify/clean/determinism unit
guards + fuzz seeds `0..2000`) and `src/bin/panicfuzz` (`--seeds A..B`/`--seed N`/`--quiet`,
unattended). Parity is N/A (front-end crash-safety only — never runs VM/interp). `cargo test --test
panicfuzz` green (8); release sweep `0..100000` (overflow-checks OFF) and debug sweep `0..20000`
(overflow-checks ON) both **0 findings** — front-end crash-safe so far. NOTE: a *release* `chezzi` has
overflow-checks OFF so arithmetic-overflow wraps invisibly there; the debug CI gate catches overflow
panics, and a full overflow sweep needs `RUSTFLAGS="-C overflow-checks=on"`. Usage + design:
[`docs/bug-discovery.md` "Panic-fuzz harness"]. Next: Tier-1 done (#1 + #2); Tier-2/3 (proptest,
grammar-accept fuzzer, TSan/loom, coverage) remain.

**✅ Bug-discovery lever #2 — CPython differential oracle (2026-06-26).** `src/difftest/` generates
random semantically-equivalent programs over a cross-language safe subset (literals, bounded-int
arithmetic, bool/str ops, `if`/`for`/`while`, non-recursive funcs, list/map/index/len), renders each
as both Chezzi and Python from one typed IR (`ast.rs`; `emit_chezzi` + `emit_python`), runs both, and
diffs stdout (`run.rs`). The Python backend prepends a **spec shim** (`_chz_str`/`_chz_div`/`_chz_mod`)
that absorbs only the by-design surface/semantic diffs (`true`/`false`/`nil`, raw nested strings,
truncate-toward-zero `/`,`%`) — so a divergence means the impl deviated from its own contract, not a
formatting artifact. Correct-by-construction generator (`generate.rs`): well-typed, in-scope, non-zero
divisors, in-range indices, and provable i64-bound tracking so generated programs never overflow (a
Chezzi fault ⇒ real bug). Wired as `tests/difftest.rs` (P0 formatting probes + bench-pair smoke +
non-tautology guard + fixed-seed fuzz) and `src/bin/difffuzz` (unattended; `--seed N` reproduces).
3000-seed release sweep clean; manually confirmed it flags the i64-overflow class (the June-2026
`sum()` blind spot). `cargo test --test difftest` green, clippy clean. Usage + design:
[`docs/bug-discovery.md` "Differential oracle"]. Lever #1 (panic-fuzzer) now also built — see above.

**✅ DSA known-answer harness — `judge/` (2026-06-27).** A third bug-discovery oracle, complementary
to panic-fuzz (#1) and the CPython differential (#2). Where the differential generator is correct
*by construction* (safe int window, no recursion, cross-language subset) — and so is blind to exactly
those edges — this runs **hand-written competitive-programming solutions** (`judge/problems/<slug>/
solution.chz`, reading stdin) against **known-correct CSES answers**, catching *shared wrongness* both
co-developed engines agree on, with an oracle independent of both engines and of CPython. Seeded with
12 problems (11 CSES + 1 Codeforces) across distinct stress paths: `weird_algorithm` (loop/bigint
Collatz), `distinct_numbers` (Set), `missing_number` (sum bigint), `playlist` (Map sliding window),
`coin_combinations_i` (DP+mod), `counting_rooms` (grid flood-fill), `repetitions` (string iteration),
`bit_strings` (modular loop), `trailing_zeros` (true-factorial differential), `stick_lengths`
(sort+i64), `apple_division` (recursion, 2^20 calls), `cf_theatre_square` (near-i64 multiply). Cases:
committed public **samples** (`samples/*.in`/`.out`) + the
gitignored full hidden suite (`judge/data/`, the authors' IP — gated/solve-first, never committed;
drop official cases there by hand if you have them). **The harness is written in Chezzi** (`judge/run.chz`, dogfood; mirrors
`benches/run.chz`): shells out per case under `timeout`, classifies `PASS`/`WRONG`/`FAULT`/`PANIC`/
`TIME`, whitespace-normalized compare (token-sequence, CSES-accurate). Not part of `cargo test` (a
`.chz` driver). **Self-contained generated-oracle mode** (`judge/generate.py` + per-problem `gen.py` +
**independent** `reference.py` — union-find vs flood-fill, enumeration vs DP, etc. — brute force on
small inputs, fast path on large): a Chezzi-vs-Python differential needing **no download**. New
problems scaffold from their public statement via `judge/fetch_problem.py <url>` (statement + samples
+ meta; CSES/Codeforces). Generated in-domain cases run clean; negative checks confirm
`WRONG`+diff / `FAULT`+exit-code detection. **Edge-case coverage
(2026-06-27):** each problem also ships an optional `edges.py` (index protocol: no arg → count,
`argv[1]=k` → k-th input) emitting the deterministic corners random `gen.py` misses — min/max sizes,
all-equal, value extremes, exact multiples, empty/full grids (incl. `counting_rooms` 1000×1000 deep
flood-fill and `cf_theatre_square` 1e18 i64-product boundary). `generate.py` writes them as
`e{k}.in/.out` through the same oracle; 318 cases (random + edges) across 12 problems run clean. Adversarial-reviewed (4 findings
fixed: token-insensitive compare, NOEXP-as-skip, fetch stale-clear, stem-collision warn). Usage +
design: [`docs/bug-discovery.md` "DSA known-answer harness"].
Remaining: P5 (IR shrinker + corpus dump + opt-in overflow-metamorphic mode).

**✅ Oracle coverage widened (2026-06-26).** The differential oracle's IR + both emitters + generator
now cover four more construct families (granular `Features` flags `string_methods`/`slicing`/
`membership`/`tuples`, all on in `full()`): (a) the eight ASCII-identical string methods
`upper`/`lower`/`replace`/`split`/`join`/`starts_with`/`ends_with`/`contains` (`contains` renders as
Python `sub in recv`); (b) Python-style slicing `xs[a:b:c]` and negative scalar indexing on lists/
strings (both engines clamp identically — no shim); (c) `in` membership (list elem / map key /
substring); (d) tuples — literals, `.N` fields, and `a, b := t` destructuring. Only one new shim arm
(tuple stringify in `_chz_str`, kept honest by `oracle_detects_tuple_render_divergence`); every other
by-design diff is absorbed by a generator restriction, **no new allowlist entry**: `replace` `old` /
`split` `sep` forced non-empty, slice step never 0, negative index kept in `[-len,-1]`, tuple arity ≥ 2.
i64-no-overflow invariant preserved — the one new int seam (tuple-field read) inherits per-element
`tuple_bounds` and is skipped inside in-loop accumulators; method/`in`/slice results carry no int value
and `split`/slice results carry `len: None` so they're never scalar-indexed. New P0 probes + per-construct
coverage + fuzz sweeps; `./target/release/difffuzz --seeds 0..5000` clean (0 findings).

**✅ global-namespace cleanup — task 5/5 (FINAL): `list`/`map`/`set`→`List`/`Map`/`Set` HARD rename
(2026-06-25).** The three builtin container TYPE **and** constructor names are now PascalCase
`List`/`Map`/`Set` everywhere — type annotations (`List[int]`, `Map[str,int]`, `Set[int]`, nested),
turbofish, struct fields, fn params/returns, and the free-fn ctors (`List(it)`/`Set(it)`/`Set()`/
`Map(it)`). **HARD rename, no alias:** lowercase `list`/`map`/`set` as a type name now falls to the
checker's unknown-type branch (REJECTED for free — the lowercase strings simply stop matching any
`resolve_type`/`infer_named_call` arm), and as a bare name they are ordinary identifiers again.
These names were never lexer keywords nor a `Type::Named` arm — they were plain string-literal matches
in the checker (`resolve_type`/`resolve_ty_ro_d` Generic arms, `is_reserved_name`, `is_builtin_type`,
`infer_named_call` ctor arms, `newtype_aggregate_cast`), compiler/interp/vm builtin dispatch +
`is_builtin` + float-widening hints, and `json_decode` — every such literal flipped to PascalCase.
**Runtime display** flips too: `type(x)` and error text now print `List`/`Map`/`Set`, the empty-set
display is `Set()` (was `set()`), and `Ty`'s `Display`/`ref_display` render `List[…]`/`Map[…]`/`Set[…]`
(so every type-mismatch message says PascalCase) — flipped in vm + interp + checker in lockstep so
VM↔interp parity stays byte-identical. **Untouched (NOT the container type):** the `.map`/`.filter`/
`.fold` list HOF methods, the `.set` method on `Shared`/`RwShared`/`Ref`, the std.iter `map(xs, f)`
free function, `tuple` (left lowercase — possible later follow-up), internal `Ty::list/map/set`
helpers, and list/map/set **literal** syntax (`[…]`/`{…}`). TDD: `pascal_containers_resolve` +
`pascal_ctor_calls` (green) and `lowercase_containers_rejected` (lowercase now "unknown"). Migrated
~52 examples + their `.expected` goldens (empty-set `set()`→`Set()`), all `std/*.chz`, the conformance
corpus, `docs/grammar.bnf` prose, and all docs. `cargo test` (2711) + conformance + clippy clean;
three-engine parity green. **Global-namespace cleanup batch COMPLETE (5/5).**

**✅ global-namespace cleanup — `timer`→`import std.time` (2026-06-25).** The opcode-backed `timer(ms)
-> Channel[bool]` builtin is no longer global — it now requires `import std.time` (whole-module) or
`import timer from std.time` (per-name); bare use otherwise is `unknown function 'timer' (import it from
std.time: \`import std.time\`)`. Mirrors the `std.concurrency` gate but for a SINGLE opcode builtin and a
REAL native module: a NEW per-module `imported_time` set (parallel to `imported_concurrency`), populated
in `bind_import` (whole-module on the exact `[std, time]` len-2 path; per-name on the from-import,
rename-rejected), gates ONLY the `infer_named_call` `"timer"` arm via `time_licensed` (`current_module_is_stdlib`
exempts std/* — `std/cancel.chz` keeps bare use). `timer` is added to `native_module_sig("std.time")`'s
`sig.types` (NOT `func()` — opcode-backed, no runtime member) so `import timer from std.time` validates
membership. **Enforcement is checker-only** — compiler/interp/vm opcode dispatch untouched, so three-engine
parity is preserved by construction. **Two baked-in fixes:** (1) `timer` STAYS a reserved name — added to
`is_reserved_type` (`struct timer`/`enum timer` rejected) AND a NEW reserved-name guard in the `fn` hoist
(`is_reserved_name` — closes a pre-existing silent-shadow hole where `fn timer()` was dead code shadowed by
the opcode). The import gate and the reserved-name gate are SEPARATE and BOTH apply. (2) a `timer`-SPECIFIC
runtime `bind_import` SKIP on BOTH engines (vm + interp) — `module=="std.time" && member=="timer"`, NOT a
blanket std.time skip (now/monotonic/sleep_ms/format DO bind normally) — so `import timer from std.time`
(type-checks green, no runtime member) binds nothing instead of faulting `module 'std.time' has no member
'timer'`. New tests RUN both engines (not check-only): whole-module + from-import `timer(50).recv()`→`true`
byte-identical VM↔interp; plus require-import / per-name-rename-reject / still-reserved checker tests.
Examples `examples/timer.chz` + `examples/wait_select.chz` now `import std.time` (byte-identical goldens both
engines). Docs (stdlib/syntax/concurrency/CLAUDE.md) updated. `cargo test` + conformance + clippy clean.

**✅ global-namespace cleanup — task 4/5: `Shared`/`RwShared`/`Atomic`/`Executor`→`std.concurrency`
(2026-06-25).** The four runtime concurrency ctor/TYPE names are no longer global builtins — they now
require `import std.concurrency` (whole-module licenses all four) or `import Shared from std.concurrency`
(per-name); bare use otherwise is `unknown type 'Shared' (import it from std.concurrency: \`import
std.concurrency\`)`. Mirrors the FFI `ptr` machinery: a NEW per-module `imported_concurrency` set
(parallel to `imported_ffi_types`), populated in `bind_import` (whole-module on the exact `[std,
concurrency]` len-2 path; per-name on the from-import), gates the `resolve_type` arms (`Executor` +
generic `Shared`/`RwShared`/`Atomic`) and the `infer_named_call` ctor arms (`current_module_is_stdlib`
exempts std/* — `std/cancel.chz`, `std/concurrency/collection.chz` keep bare use). `std.concurrency` is
a NEW **file-less native module** (`native_name` maps len-2 `[std, concurrency]`; len-3 `import
std.concurrency.collection` still loads the file — no collision) with EMPTY callable members; its
`native_module_sig` exports ONLY the four in `sig.types`. **Enforcement is checker-only** — compiler/
interp opcode dispatch is untouched, so three-engine runtime parity is preserved by construction.
**Two baked-in fixes over the prior rejected attempt:** (1) the four STAY reserved names — `Executor`
was already in `is_reserved_type`; `Shared`/`RwShared`/`Atomic` joined it, so `struct Shared`/`struct
Executor` is now a clean at-declaration `reserved` error instead of the confusing silent-hijack (the
import gate and the reserved-name gate are SEPARATE and BOTH apply). (2) a runtime `bind_import` SKIP
on BOTH engines (vm + interp) for `std.concurrency` member ∈ the four, so `import Shared from
std.concurrency` (which type-checks green but has no runtime module member) binds nothing instead of
faulting `module 'std.concurrency' has no member 'Shared'`. New tests RUN both engines (not just
check): whole-module construct+use of all four, and the from-import case that crashed the prior
attempt; plus reserved-still + per-name-licensing + len-3-does-not-license checker tests. Examples that
used the four bare now `import std.concurrency` (atomic/executor/executor_pool/executor_autodrain/
demo_executor/shared/rwshared/parallel_shared/parallel_cancel/ref_airlock/cancel_cpu + the two
concurrent_collection*). Docs (stdlib/syntax/concurrency) updated. `cargo test` (2708) + conformance +
clippy clean. (FINAL cleanup task — list/map/set→List/Map/Set — landed as task 5/5 above.)
**Checker polish (2026-06-25, follow-up to 4/5):** (a) a BARE (no `[T]`) `Shared`/`RwShared`/`Atomic`
annotation now hits a dedicated `resolve_type` arm instead of falling to the catch-all — unlicensed →
the SAME `unknown type '…' (import it from std.concurrency: …)` hint the `Shared[T]` arm gives;
licensed → the missing-type-arg error `type '…' expects 1 type argument(s), got 0` (matches the
user-generic struct/enum/newtype precedent). Mirrors the bare `Executor` arm. (b) the
`current_module_is_stdlib` stamp at `check_program` now calls the canonical `LoadedModule::is_std()`
(resolver) instead of an inline `dotted.first()==Some("std")` half-reimplementation that dropped the
`native.is_some()` clause — behavior-preserving (native std modules carry no concurrency annotations),
de-dups to ONE definition. Checker-only → three-engine parity by construction. New failing-then-green
tests: bare-without-import → hint; bare-with-import → missing-type-arg.
**Checker fix (2026-06-25, follow-up to 8fcbb3c — reserved-name-as-type-param hijack):** commit
8fcbb3c established the rule "a user generic type param named like a reserved/builtin type resolves as
the type param, not the builtin" but only patched the `Shared`/`RwShared`/`Atomic` arm in `resolve_type`
with an inline `if !self.type_params.contains_key(n)` guard. Five OTHER reserved-name arms still
preceded the `type_params` fallthrough and short-circuited it: `Socket`/`Listener`/`owned_str` silently
hijacked a same-named type param to the builtin (→ later type-mismatch), and the license-gated
`Executor`/`ptr` arms emitted a bogus `unknown type '…' (import …)`. Fix: HOISTED the `_ if
self.type_params.contains_key(n) => Ty::Param(n.clone())` arm to sit just below the scalar-primitive
literals (`int`/`float`/`bool`/`str`/`bytes`/`bytearray`/`nil`) and ABOVE every reserved/module arm, so
an in-scope type param uniformly shadows them all (kept below the scalars so `fn id[int](x: int)` still
resolves `x` to `int`, unchanged). The now-redundant inline guard on the `Shared`/`RwShared`/`Atomic`
arm was removed (one source of truth). Checker-only name resolution — runtime ctor/opcode dispatch
untouched, three-engine parity by construction. `is_reserved_type`/declaration-site reservedness
unchanged (`struct Executor` still reserved; `struct Socket` still allowed). New tests: extended
`type_param_named_like_concurrency_type_not_shadowed` to all five names, new
`bare_reserved_type_without_typeparam_still_errors` (negative cases preserved), new RUN parity test
`type_param_named_like_reserved_runs_both_engines` (check_graph + cooperative VM + OS-thread engine +
interp all agree).

**✅ global-namespace cleanup — task 2/5: FFI `ptr` gated behind `import std.ffi` (2026-06-25).** The
opaque C-ABI `ptr` type is no longer a global builtin — it now requires an import, **consistent with
the fixed-width integer types `int8`..`uint64`**. The `"ptr"` arm in `resolve_type` (checker) is gated:
it resolves to `Ty::Ptr` only if the module imported it (`imported_ffi_types`) or via a licensed alias
body, else `unknown type 'ptr' (import it from std.ffi: \`import std.ffi\`)`. Gating fires for ordinary
annotations AND `extern` param/return signatures (both go through `resolve_type`). Licensing: `ptr` is
added to `native_module_sig("std.ffi").types`; whole-module `import std.ffi` licenses `ptr` (keyed on
the exact `[std, ffi]` path — extern blocks use `ptr` pervasively, so whole-module licensing is the
default, UNLIKE the per-name-only widths), and `import ptr from std.ffi` licenses it per-name; `import
ptr as P` is rejected (no rename — backends key off the literal surface name). The runtime from-import
member check (interp + VM) skips `ptr` like the width names (type-only import, no runtime value). The
ungated C-marshalling paths (`resolve_ctype_d`, `resolve_ty_ro_d`) are untouched. `examples/ffi_ptr.chz`
now imports `ptr`; docs (stdlib/syntax/spec) updated. New tests + VM↔interp parity green. (3 cleanup
tasks remain: Match/Response/ProcResult→modules, Shared/RwShared/Atomic/Executor→std.concurrency,
list/map/set→List/Map/Set.)

**✅ global-namespace cleanup — task 3/5: `Match`/`Response`/`ProcResult`→modules (2026-06-25).** The
three synthetic native-module structs (`Match`/`std.regex`, `Response`/`std.request`,
`ProcResult`/`std.process`) are no longer global-reserved type names — they are now MODULE-OWNED. Built
native-module struct-type export: `native_module_sig` now populates `sig.struct_defs` + `sig.types` for
the owning module (the SAME field lists as the layout seed), and the existing is_std whole-module +
`import Name from module` import paths flow those into `struct_names`/`bare_types`, so the BARE type name
(`m: Match` / `Match(...)`) and qualified `regex.Match(...)` resolve ONLY when the module is imported.
The layout stays globally present (`StructOrigin::Builtin`) so FIELD ACCESS on a native return
(`regex.find(...).text`) keeps working with **no import**; the unconditional `struct_names` (bare-name)
reservation in `seed_stdlib_structs` is dropped. The hoist's already-defined gate now exempts a
`Builtin`-origin seed, so a user `struct Response` (without `import std.request`) shadows the seed and is
their own `User`-origin type. The names are now user-constructible once imported, so the compiler + interp
register the synthetic struct under its bare name in `module_types` (+ the interp seeds the `StructDef`)
to lower the ctor identically (VM↔interp parity). Unknown-type errors hint the owning module
(`types_by_name`). New checker + VM↔interp parity tests; docs (stdlib/syntax/spec) updated. (2 cleanup
tasks remain: Shared/RwShared/Atomic/Executor→std.concurrency, list/map/set→List/Map/Set.)

**✅ global-namespace cleanup — task 1/5: free `len()` dropped (2026-06-25).** The free `len(x)`
builtin is removed from all four stages (checker `is_reserved_name` + free-len arm, compiler
`is_builtin`, interp `builtins::is_builtin`/dispatch/`fn len`, VM dispatch + `fn builtin_len`); `len(x)`
now resolves as a plain `unknown name 'len'`, and `len` is no longer reserved (a user may declare
`fn len`). The `.len()` METHOD is kept everywhere (str/list/map/set/bytearray/Channel) and **added to
`bytes`** (checker `bytes_method_sig` + VM `bytes_method` + interp bytes-method arm, byte count,
VM↔interp parity). All free-len call sites in `examples/` migrated to `.len()`; docs (stdlib/syntax/
spec) updated. (4 more namespace-cleanup tasks queued: ptr→std.ffi, Match/Response/ProcResult→modules,
Shared/RwShared/Atomic/Executor→std.concurrency, list/map/set→List/Map/Set.)

**✅ runtime — `RwShared[T]`: the cross-task read-write box (2026-06-24).** New VM-core primitive
pairing with `Shared[T]`: **MANY concurrent readers OR one exclusive writer** (`RwSharedCore` wraps
`std::sync::RwLock<WireValue>` exactly where `SharedCore` wraps `Mutex`). Constructed value-first
(`RwShared(v)`, `T` inferred). Methods: `get() -> T` (shared read guard, snapshot), `set(x) -> nil`
(exclusive write guard, replace), `read(f: fn(T) -> R) -> R` (**shared** read guard — runs `f` against
the current value and returns its result, R-polymorphic in the closure's return, **no** write-back;
many `read`s run concurrently), `write(f: fn(T) -> T) -> nil` (**exclusive** write guard — `Shared.update`
under the write lock). Mirrored `Shared` end-to-end across BOTH engines: `Op::NewRwShared`,
`Obj::RwShared`/`WireValue::RwShared` (crosses the airlock as a SHARED `Arc` handle, NOT deep-copied —
the spawn/Channel airlock + GC trace + `to_wire`/`from_wire` twins), `Ty::RwShared` (sendable, new
reserved name), checker `rwshared_method_sig` + the `read` R-polymorphism recovered at the dispatch
seam, interp `Value::RwShared` + `eval_rwshared_method`. **`write`'s RMW is atomic across threads** via
a separate `update_lock` held for the whole write under `--parallel` (the `RwLock` write guard alone is
NOT enough — it's dropped across the user closure, so two writers could otherwise lose an update; same
discipline as `Shared.update`). Reentrancy limit (documented, mirrors `Shared.update`): a closure that
re-acquires the **same** box's write lock deadlocks. Golden `examples/rwshared.chz` (N tasks each
`write` a distinct key into one `RwShared[map]`, join, parent `read`s — order-independent →
byte-identical on VM/`--serial`/`--parallel`/interp). Docs: `docs/concurrency.md` §6c, `docs/stdlib.md`
§3, `docs/spec.md`/`docs/syntax.md` reserved-name + sendable enumerations. 2618+ tests + conformance
green, clippy clean.
**✅ stdlib — `std.concurrency.collection`: thread-safe collections over `RwShared` (2026-06-24).**
The capstone of the concurrency-collections work: pure-Chezzi ergonomic wrappers over the just-landed
`RwShared[Map[...]]` primitive, in the **first nested std module** (`std/concurrency/collection.chz` —
the dotted path resolves generically, no resolver special-casing). Two generic structs:
**`ConcurrentMap[K: Hashable, V]`** (`get`/`contains`/`len`/`snapshot` concurrent reads; `set`/`remove`/
`get_or_insert` exclusive writes — `get_or_insert` is COMPOUND-ATOMIC, check-and-insert in one write
lock) and **`ConcurrentCounter[K: Hashable]`** (`count`/`total` concurrent reads; `increment`/`add`
exclusive writes doing their read-modify-write in ONE closure → N tasks incrementing one key total
EXACTLY N, the classic race-free counter). Proven by live probe before building: (1) the nested path
resolves, (2) a struct whose only field is an `RwShared` crosses the spawn/`parallel:` airlock as a
SHARED `Arc` handle (NOT a deep copy) — 100 spawned `.increment` + 1 pre-bind → parent reads 101 on
VM/`--serial`/`--parallel`, (3) the single-write-lock RMW is race-free (exact-100 on `--parallel`,
5/5 deterministic). Construction is direct (`ConcurrentMap(RwShared({}))` — no `new_*` factory, since
turbofish can't bind `K`/`V`; same as `Counter({})`). Pure-Chezzi → 3-engine parity automatic; only
Rust touched is the two golden-test registrations (no engine code). Golden
`examples/concurrent_collection.chz` (deterministic: 100-task counter race → exactly 100, each-own-key
map → 285) byte-identical on VM/`--serial`/`--parallel`/interp. Tests: `examples/concurrent_collection_test.chz`
(6 `test fn`s incl. the airlock-sharing crux guard + `counter_race_exact`), VM
`golden_concurrent_collection_via_run_file` + interp twin. Docs: `docs/stdlib.md` §5 new
`### std.concurrency.collection`, `docs/concurrency.md` §6f pointer, `gaps.md` resolved. Resolves the
concurrent-collections / data-structures-concurrency gap (queue = `Channel`, atomic scalar = `Atomic`;
no `ConcurrentList`/`Set`/`Queue`). Full suite + conformance + clippy clean.
**✅ fix — FFI callback SIGSEGV (dangling `Cif`) (2026-06-24).** `chezzi run examples/ffi_qsort.chz`
segfaulted (libffi `classify_argument`, reachable via the qsort comparator callback) — a use-after-move:
`ffi_prep_closure_loc` stores a raw pointer to the callback `Cif`'s inner `ffi_cif`, but the `Cif` was
held **by value** in `CallbackClosure` (`src/native/cffi.rs`) and then moved into the
`callback_closures` `Vec`, relocating the `ffi_cif` and dangling that pointer. Layout-dependent, so the
3-engine `ffi_qsort` goldens (cooperative VM + interp + M:N `--parallel`) all passed while the CLI binary
crashed deterministically. Fix: `Box` the `Cif` (`_cif: Box<Cif>`) so its address is pinned across the
moves — exactly what the sibling `ctx: Box<TrampolineCtx>` already does. Regression guard:
`native::cffi::tests::boxed_callback_cif_address_is_stable_across_moves` (a compile-time check that the
field still derefs to `Cif` + the address-stability property). Full suite + conformance + clippy clean.

**✅ stdlib — `std.request` nit closed: per-call timeout + query builder (gaps.md "std.request nit") (2026-06-24).**
Two small independent additions. (A) **Per-call timeout override:** `std.request`'s `get`/`post`/`request`
now take an OPTIONAL trailing `timeout_ms: int` (mirrors the `std.net` `Socket.read(.., timeout_ms?)`
idiom) — a positive value applies ureq's per-request `.timeout(Duration)` (a TOTAL deadline overriding
the agent's hardcoded connect/read/write caps for that one call); `<= 0`/omitted falls back to the
defaults. A timeout surfaces through the existing `Error::Transport → Err` path (recoverable, never a
panic). New `expect_args_range(h, name, min, max)` helper in `src/native/mod.rs` (runtime mirror of
`FnSig::optional_tail`); `read_timeout` reads the guarded optional int. The checker's module-member
call path (`infer_method_call` `Ty::Module` arm) + the from-imported bare-fn path now route through
`check_args_range_w(.., min_params, .., widen=true)` so optional-tail arity is honored uniformly for
every native module fn (behavior-preserving — plain sigs have `min_params == params.len()`). std.request
`get`/`post`/`request` sigs → `optional_tail(.. + [Int], .., 1)` (installed post-match in
`native_module_sig` since the `func` closure borrows `sig`). The offload seam needs ZERO change (the
optional int crosses the airlock via `extract_native_args` generically → 3-engine parity by construction).
NO network golden for the timeout (non-deterministic); plumbing is asserted by a `do_get(.., Some(Duration))`
unit smoke + checker arity tests. (B) **Query builder:** `std.encoding.query_encode(params: Map[str,str]) -> str`
builds a `k=v&k2=v2` query string — both key and value percent-encoded (factored a shared `percent_encode`
helper reused by `url_encode`, no duplicated escaper), **keys sorted by RAW value** for a deterministic
golden, empty map → `""`. Lives in `std.encoding` (NOT `std.request`) because a native module name shadows
a same-named `std/<name>.chz` (the rand-task lesson) — no clean place for a pure-Chezzi request helper.
Pure CPU → NOT `is_blocking`. Golden `examples/encoding.chz` extended (sorted-key + empty + URL-compose
cases), 3-engine parity verified. Docs: `docs/stdlib.md` (§std.request timeout note + §std.encoding
query_encode), `gaps.md` (std.request nit struck → ✅ resolved). 2602 tests + conformance green, clippy clean.

**✅ stdlib — `std.collections` pure-Chezzi generic data structures (gaps.md "data structures
(heap/PQ, deque, counter, ordered map)") (2026-06-24).** New pure-Chezzi module `std/collections.chz`
(no native Rust, no seam — like `std/datetime.chz`/`std/path.chz`): three generic structs over `T`
built on the builtin `list`/`map`, so identical across all three engines. **`Heap[T]`** — binary
heap over a backing `List[T]` with a comparator **closure field** `less: fn(T,T)->bool` (verified a
generic struct can hold + call a fn-typed field); contract `less(a,b)==true ⇒ a pops first`, so
`a<b`=min-heap, `a>b`=max-heap (any `T`, no `Comparable` needed); `min_heap()`/`max_heap()` int
factories, `from_list(xs, less)` heapify (push-loop O(n log n)); push/pop O(log n), peek/len/is_empty
O(1). **`Deque[T]`** — **two-stack** amortized-O(1) both ends (front/back lists, drain-far-on-empty);
construct `Deque([], [])` (no `deque()` factory — a no-arg generic factory can't bind `T`).
**`Counter[T: Hashable]`** — `Map[T,int]` frequency table; `add`/`add_n`/`count` (0 if absent)/`total`/
`most_common(k)` (top-k by descending count, **stable insertion-order tie-break** via `map.keys()`
order + stable `sort_by`); construct `Counter({})`. **Empty semantics:** every removal/peek returns
`Option[T]` (`None`, never a fault — matches `list.pop()`). **Ordered map intentionally omitted** —
builtin `map` is already insertion-ordered (documented note only). TDD: `examples/collections_test.chz`
(12 `test fn`s — heap min/max/reverse/empty/from_list, deque fifo/lifo/both-ends/interleaved/empty,
counter counts/total/most_common+ties+k-clamp) RED→GREEN; golden `examples/collections.chz` +
`.expected` + `#[test] golden_collections_via_run_file` (VM==interp via `assert_file_parity`),
3-engine parity spot-checked. Docs: `docs/stdlib.md` (new `### std.collections` in §5), `gaps.md`
(data-structures struck → ✅ landed; ordered-map note). cargo test + conformance green, clippy clean.

**✅ stdlib — `std.datetime` pure-Chezzi civil-calendar date/time (gaps.md "duration/date
decomposition") (2026-06-24).** New pure-Chezzi module `std/datetime.chz` (no native Rust, no seam —
like `std/path.chz`) layered on the native `std.time` clock (`time.now()` only); everything else is
pure integer math (Howard Hinnant's branch-free civil-calendar algorithms). Surface: a `DateTime`
struct (`year`/`month`/`day`/`hour`/`minute`/`second`/`weekday`), `from_epoch`/`to_epoch` (round-trip
`to_epoch(from_epoch(e))==e`), `now`, `days_from_civil`/`civil_from_days` (a `(int,int,int)` tuple),
`is_leap_year`/`days_in_month`, `weekday`/`weekday_name`, fixed formatters `to_iso8601`/
`to_date_string`/`to_time_string`/`to_string`, and epoch-int duration helpers `add_seconds`/`add_days`/
`diff_seconds`/`diff_days`. **Contractual semantics** (in `docs/stdlib.md §5`): **UTC-only** (timezones/
DST/tz-database explicitly deferred); **weekday Sunday=0..Saturday=6** (matches native `std.time`:
epoch 0 == 1970-01-01 is Thursday == wd 4, differs from Python's Monday=0); **negative epochs floored**
(Chezzi `/`/`%` truncate toward zero, so internal `fdiv`/`fmod` floor-div helpers split the day/seconds
— `from_epoch(-1)`→1969-12-31 23:59:59 Wed, round-trips). Verified vectors: epoch 0, 1700000000 →
2023-11-14 22:13:20, `days_from_civil(2024,2,29)`==19782, leap 2000/2024, non-leap 1900/2023.
Pure-Chezzi → 3-engine parity automatic; still added `examples/datetime_test.chz` (9 `test fn` TDD
table) + golden `examples/datetime.chz`/`.expected` wired into `golden_datetime_via_run_file` (VM,
`assert_file_parity`) + `golden_datetime_chz` (interp twin). Docs: `docs/stdlib.md` (new `### std.datetime`
in §5), `gaps.md` (duration/date struck from the dogfood list — was falsely listed as landed). Full
suite + conformance + `clippy --all-targets -D warnings` clean.

**✅ stdlib — `std.path` pure-Chezzi path-STRING ops (gaps.md "path ops") (2026-06-24).** New
pure-Chezzi module `std/path.chz` (no native Rust, no seam — like `std/str.chz`/`std/iter.chz`) for
**unix `/` path-STRING manipulation, NOT filesystem I/O** (that stays `std.fs`). Built on the core
`str` methods (`split`/`starts_with`/`ends_with`) + the `str` `join` receiver. Surface:
`is_abs`/`is_rel`, `basename`/`dirname`/`split` (a `(str, str)` tuple = `(dirname, basename)`),
`ext`/`stem`/`with_ext`, `normalize`, `join`. Edge-case semantics match Python `os.path` (basename/
dirname/splitext) and Go `path.Clean`/`path.Join` for `normalize`/`join` (chose Go's simple join, NOT
Python's absolute-resets-earlier footgun) — every case is contractual in `docs/stdlib.md §5` (the
hard ones: `basename("a/b/")`→`""`, `dirname("/a")`→`"/"`, `ext(".bashrc")`→`""`, `ext("dir.d/file")`
→`""`, `normalize("/a/../../b")`→`"/b"`, `normalize("a/../../b")`→`"../b"`, `normalize("")`→`"."`).
Separator policy: `/` only, no Windows `\`. Pure-Chezzi → 3-engine parity is automatic (same `.chz`
on all engines); still added `examples/path_test.chz` (9 `test fn` TDD table, `cargo run -- test`) +
golden `examples/path.chz`/`.expected` wired into `golden_path_via_run_file` (`assert_file_parity` =
VM == interp). Docs: `docs/stdlib.md` (new `### std.path` in §5), `gaps.md` (path ops struck from the
pure-Chezzi dogfood list). Full suite + conformance + `clippy --all-targets -D warnings` clean.

**✅ stdlib — `std.process` polish (gaps.md "std.process polish") (2026-06-24).** `std.process` had
only `cmd(line)` via `sh -c` (injection-prone, stdout discarded on a non-zero exit). Added two
structured forms in `src/native/process.rs`: `run(line) -> Result[ProcResult]` (still `sh -c`, same
shell semantics as `cmd`) and `run_args(prog, args: List[str]) -> Result[ProcResult]` (runs the
program **directly, no shell** → arguments are passed literally, **injection-safe**). The new synthetic
struct `ProcResult { stdout: str, stderr: str, code: int }` carries **both streams + the exit code**: a
non-zero exit is a normal `Ok(ProcResult)` with `code != 0` (stdout NOT discarded), **only a spawn
failure** (no such program / permission) is `Err`; a signal-killed process reports `code = -1`. `cmd`
is unchanged (back-compat — `examples/sys.chz` still green). The `List[str]` argv crosses the off-heap
offload boundary via a NEW seam variant `NativeArg::List(Vec<String>)` + `Host::arg_str_list` (default-
err), implemented on all three hosts (`VmHost` reads the live heap list, `extract_native_args`
snapshots it to `NativeArg::List`, `OffloadHost` serves it back off-thread, `InterpHost` reads the live
list) — a direct clone of the existing `Map[str,str]` triad, so 3-engine parity (interp == cooperative
VM == M:N) holds by construction at the NativeFn seam. `run`/`run_args` wired into `is_blocking()`
(subprocess I/O → offloaded under the OS-thread engine). `ProcResult` is registered with the other
synthetic stdlib structs in the compiler (`src/compiler/mod.rs`, declaration-order field names) and
seeded in the checker (`seed_stdlib_structs` + `native_module_sig` std.process arm). Golden (VM ==
interp via `assert_file_parity`, byte-identical under run/--serial/--parallel):
`examples/process_polish.chz` — proves nonzero-is-Ok-with-code, the `$(...)`/`;`/`&&` injection-safety
of `run_args`, and the spawn-failure `Err` path. Docs: `docs/stdlib.md` (§std.process extended +
`ProcResult` reserved), `gaps.md` (std.process polish → ✅ RESOLVED). **Deferred:** stdin piping,
output streaming, per-process env/cwd overrides. Full suite + conformance + `clippy --all-targets -D
warnings` clean.

**✅ stdlib — encoding/crypto/uuid native modules (gaps.md "Encoding/crypto") (2026-06-24).** Three
new native modules, all hand-rolled with **zero new crates** (repo dependency-free policy):
`std.encoding` (`src/native/encoding.rs`) — base64 std + URL-safe (RFC 4648), hex, RFC 3986 URL
percent-encode/decode; `std.crypto` (`src/native/crypto.rs`) — `sha256` (FIPS 180-4) + `md5` (RFC 1321),
both validated against published test vectors + cross-checked vs `sha256sum`/`md5sum`; `std.uuid`
(`src/native/uuid.rs`) — `v4` (random, RFC 4122) + `uuid_seed` (deterministic), with its OWN
process-global SplitMix64 stream that reuses `rand::next_u64` (the RNG step is not duplicated) and
auto-seeds from OS entropy. The native seam carries only `str`, so every fn is `str`-in and
`str`/`Result[str]`-out: encoders/digests are infallible `str`; base64/hex/url `decode` UTF-8-validate
their output and surface malformed input OR non-UTF-8 bytes as a catchable `Err` (never a panic). All
members are pure CPU transforms → NOT in `is_blocking()` (run inline on every engine), giving 3-engine
parity (interp == cooperative VM == M:N) by construction at the NativeFn seam. Wiring mirrors std.rand/
std.fs: `MEMBERS` table per file, `src/native/mod.rs` (`pub mod` + `native_name`/`native_members` arms +
the uniqueness/non-blocking test lists — `uuid` reseed is named `uuid_seed`, not `seed`, to keep bare
member names unique since `std.rand` owns `seed`), `src/checker/mod.rs` `native_module_sig` arms.
Goldens (VM == interp via `assert_file_parity`): `examples/encoding.chz` / `crypto.chz` (deterministic
round-trips + digests) and `examples/uuid_shape.chz` (`uuid_seed`-deterministic stream + shape check,
serialized on `TEST_UUID_LOCK`). Docs: `docs/stdlib.md` (new §std.encoding/§std.crypto/§std.uuid),
`gaps.md` (Encoding/crypto → ✅ RESOLVED). **Deferred:** the str-only seam can't return raw bytes, so
binary round-trip (image → bytes) needs a bytes-arg/return seam expansion; `sha512`/`sha1`/`uuid-v7`
not added. Full suite + conformance + `clippy --all-targets -D warnings` clean.

**✅ stdlib — `std.fs` filesystem mutations (gaps.md "fs mutations") (2026-06-24).** `std.fs` was
read-only; it now writes. Six new natives in `src/native/fs.rs`, each mirroring `std.io.write_file`'s
fault idiom (`Ok(NativeRet::Ok(Nil))` / `Ok(NativeRet::Err("{path}: {e}"))`) so an I/O failure is a
catchable `Err`, never a panic — and all are `Result[nil]`: `mkdir(path)` (recursive via
`create_dir_all`, mkdir -p, idempotent on an existing dir), `remove_file(path)`, `remove_dir(path)`
(**empty-only / non-recursive** — faults on a non-empty dir, no silent `rm -rf`), `rename(from, to)`,
`copy(from, to)` (file contents; byte count dropped for `Result[nil]` parity with `write_file`),
`append(path, contents)` (`OpenOptions` create+append — creates if absent, **never truncates**,
complementing `write_file`'s overwrite). 3-engine parity is by construction at the NativeFn seam (interp
/ cooperative VM / M:N all call the same `fs.rs` fn). Wired into `is_blocking()` (std.fs arm) so the M:N
engine offloads them like the read ops; checker `native_module_sig` std.fs arm gains the six sigs
(`mkdir`/`remove_file`/`remove_dir`: `str -> Result[nil]`; `rename`/`copy`/`append`: `str, str ->
Result[nil]`). **Limit (documented, deferred):** recursive dir removal (`rm -rf`) is intentionally not
provided — `remove_dir` is empty-only to avoid an accidental wipe. Tests (RED-first): 2 `fs.rs` unit
(roundtrip mkdir→append→rename→copy→remove + recoverable-error cases via a temp-dir `Host` mock), the
`is_blocking` offloadable-set + uniqueness-guard lists, 2 checker tests (the six sigs typecheck as
`Result[nil]`; wrong-arity rejected), and the self-cleaning golden `examples/fs_mutations.chz`
(VM + interp twins, serialized via `FS_SCRATCH_LOCK` on the shared `examples/.fs_scratch`; gitignored;
fixed status lines + read-back contents, no absolute paths) — manually verified byte-identical under
run / --serial / --parallel and leaves no scratch behind. No grammar change (plain import + member
calls; conformance clean). Docs: `docs/stdlib.md` (§std.fs split into Queries/Mutations + the
non-recursive/never-truncate limits), `gaps.md` (fs mutations → ✅ RESOLVED). Full suite + conformance +
`clippy --all-targets -D warnings` clean.

**✅ stdlib — `std.rand` native RNG (gaps.md highest stdlib gap) (2026-06-23).** A SplitMix64 PRNG.
**Native module `std.rand`** (`src/native/rand.rs`) exposes scalars only: `seed(n: int) -> nil`
(deterministic reseed), `float() -> float` in `[0, 1)`, `int(lo, hi) -> int` (half-open `[lo, hi)`;
faults `rand.int(lo, hi): hi must be > lo` if `hi <= lo`, unbiased via rejection sampling), `bool()`.
State is a single **process-global** `OnceLock<Mutex<u64>>` (NOT thread-local / NOT Host-side), so all
three engines (interp / cooperative VM / M:N `--parallel`) share one stream at the NativeFn seam →
any *sequential* draw sequence is byte-identical across engines (3-engine parity by construction).
Auto-seeds from OS entropy (`libc::getrandom` on Linux, with a time/address/counter SplitMix64-mix
fallback) on first use; `seed(n)` makes it deterministic. Draws are inline CPU → **not** in
`is_blocking()`. **Generic helpers in `std.iter`** (pure Chezzi, call native `rand.int`): `shuffle[T]`
(new Fisher–Yates permutation, non-mutating), `choice[T] -> Option[T]` (`None` on empty), `sample[T]`
(`k` without replacement, `k` clamped to len). The split is **forced**: the native seam carries only
engine-neutral scalars (cannot return a generic `List[T]`), and a native module name short-circuits a
same-named `std/<name>.chz` in the resolver — so scalars + helpers cannot co-inhabit a `rand`
namespace. **Limit (documented, not a bug):** under `--parallel`, *concurrent* draws from multiple
tasks interleave nondeterministically on the shared global RNG (engines may diverge) — the goldens draw
strictly sequentially to stay deterministic on all three engines; this is the same class as the existing
cooperative-vs-MN timing escape hatches. Tests (RED-first): 5 `rand.rs` unit (SplitMix64 golden vector
in isolation, float/int/bool range + half-open + empty-range fault + auto-seed shape), native wiring +
non-blocking + uniqueness lists, and 3 run-file goldens (`rand_seeded` all-four-fns seeded,
`rand_shape` unseeded range-only "ok" lines, `rand_iter` shuffle/choice/sample) run as ONE serialized
test (shared global RNG) + `assert_file_parity` (VM == interp); manually verified VM == `--serial` ==
`--parallel` byte-identical on the seeded goldens. No grammar change (plain import + member calls;
conformance clean). Docs: `docs/stdlib.md` (new §std.rand + std.iter shuffle/choice/sample),
`gaps.md` (std.rand → ✅ RESOLVED). Full suite + conformance + `clippy --all-targets -D warnings` clean.

**✅ DX — print `sep=`/`end=` + assert message format (gaps.md DX gaps #5 + #6) (2026-06-23).** Two
cohesive builtin-ergonomics fixes. **print (#5):** `print` is now special-cased to accept exactly two
named arguments — `sep` (default `" "`, joins the positional args) and `end` (default `"\n"`, appended
after). Both must be `str` and may be runtime expressions (not just literals). `print("a","b")` → `a b\n`
(unchanged), `print("a", end="")` → `a` (no newline → incremental output), `print("a","b", sep="-",
end="!")` → `a-b!`. Wired through **desugar** (`print` keeps only `sep`/`end` on its Call un-rewritten,
rejecting any other kwarg / a dup with "print() only accepts the named arguments 'sep' and 'end'"),
**checker** (each `sep`/`end` value must be `str`, else "print() sep/end must be str, found <T>"),
**compiler** (new `Op::CallPrintSep{argc}` that pushes `sep`+`end` after the args; a plain `print(...)`
with no kwargs still emits `Op::CallPrint` → output byte-identical to before), and **both engines**
(`vm::do_print_sep` + the interp print branch, same join-with-`sep`/append-`end` order: positional args →
sep → end). **assert (#6):** the `assert cond, "msg"` STATEMENT form already existed end-to-end; the fix
is the **fault wording** — a failing `assert false, "boom"` now faults as `assertion failed: boom` (was
the raw `boom`), bare `assert false` keeps exactly `assertion failed`, and `msg` is still evaluated lazily
on the failing path only. Two fault sites (`vm/mod.rs` `Op::Assert` + `interp/mod.rs` `Assert`),
byte-identical across engines. Tests (all RED-first): 4 desugar (sep/end kept, unknown/dup kwarg rejected),
3 checker (sep/end str ok, sep/end non-str rejected), 7 VM behavior (end="", sep=, both, default unchanged,
runtime expr, only-end), 1 VM↔interp print parity (8 forms), updated assert tests + new lazy-on-pass guards
on both engines, and golden `examples/print_kwargs.chz` (VM == interp == `.expected`). Docs:
`docs/syntax.md` (assert fault wording + lazy msg), `docs/stdlib.md` (print signature with `sep=`/`end=`),
`gaps.md` (gaps #5/#6 → RESOLVED log). No grammar change (print kwargs are ordinary call named-args;
conformance clean). Full suite + conformance + `clippy --all-targets -D warnings` clean.

**✅ DX — stepped / reverse range (gaps.md DX gap #4) (2026-06-23).** `range()` gained a 3-arg
`range(start, end, step)` form (the 1-arg/2-arg forms are byte-unchanged). `step` is a **non-zero int**:
positive counts up half-open `[start, end)`, negative counts down half-open (excludes `end`), e.g.
`range(10, 0, -1)` → `[10, 9, …, 1]`, `range(0, 10, 2)` → `[0, 2, 4, 6, 8]`. A wrong-direction step or
`start == end` → `[]`; `step == 0` raises a recoverable fault `range() step cannot be zero`. All the
element-count / cap math runs in **i128** so a huge span or an `i64::MIN` bound/step can't overflow or
panic (`i64::MIN.abs()` would); the 10M result cap is unchanged. The materialization is a single shared
`slice::range_values(start, end, step) -> Result<Vec<i64>, String>` called by **both** engines (interp
`builtins::range` + VM `builtin_range`) so the values and fault text are byte-identical. **SECONDARY
(landed): a range literal is now sliceable** like a list — `(0..10)[::2]` → `[0, 2, 4, 6, 8]`,
`(0..5)[::-1]` → `[4, 3, 2, 1, 0]` — by materializing the (ascending, step-1) range via the `range`
builtin then reusing the **existing** `Op::GetSlice` / `slice::slice_indices` `::step` machinery (compiler
Slice arm emits `CallBuiltin("range", 2)` when the obj is a `Range`; interp `eval_slice` mirrors it). A
bare range still has no value anywhere else (`x := 0..10` keeps its compile error). **Decision: `a..b`
stays ascending — no auto-reverse** (`for i in 10..0` yields nothing, the lazy for-loop path is
untouched); the down-count idiom is `range(start, end, -1)`. No grammar change (the `..` syntax is
untouched; conformance clean). **Parity by construction** (shared helper). Tests (all RED-first): 3
`slice::range_values` unit tests (up/down/by-N, empty + zero-step, overflow/INT_MIN edges) + interp +
VM runtime tests (up/down/step-zero/empty/range-slice) + 2 checker tests (1/2/3-arg accept, 0/>3 reject,
non-int reject; range-slice infers `List[int]`) + golden `examples/range_step.chz` (VM == interp ==
`.expected`). Docs: `docs/syntax.md` (range section + slicing note), `docs/stdlib.md` (range signature),
`gaps.md` (gap #4 → RESOLVED log, open DX items renumbered 1..3). Full suite + conformance +
`clippy --all-targets -D warnings` clean.

**✅ DX — collection operators (gaps.md DX gap #3) (2026-06-23).** List `+` (concat) / `*` (repeat)
and set `| & - ^` (union / intersection / difference / symmetric-difference) now work as operators,
behaviour **identical to the existing methods** (`.concat`, `.union`/`.intersection`/`.difference`;
`^` symmetric-difference has no method form). Implemented as **runtime-opcode dispatch** (NOT compiler
desugar — the compiler has no operand type info): new value-typed match arms in `vm::arith` +
`vm::bitwise` (a shared `Vm::set_op` + `Vm::list_repeat`), mirrored byte-for-byte in
`interp::eval_binary` (free-fn `set_op`/`list_repeat`), plus the type arms in checker `infer_binary`
(list/set element types must match — a mismatch is the existing `cannot apply …`/`bitwise operator …
requires int operands or two sets` error; `[] + [1]` infers `List[int]` via `merge_unknown`).
`list * int` is **commutative** (`3 * [0]` too, Python-style); `n <= 0` → `[]`; a giant `n` raises a
recoverable `list repeat capacity overflow`, never a process abort. The guard is two-layered: an
`isize::MAX` byte-size check (overflow-safe `checked_mul`) **plus** a `Vec::try_reserve_exact`
allocation-feasibility check — the latter catches huge-but-representable counts (~1e17..5.7e17 for a
1-element list) that pass the byte bound yet abort `Vec::with_capacity`; `str.repeat` carries the same
two-layered guard. Set results preserve insertion order (union = mine-then-other; intersection/difference =
mine-filtered; symmetric-difference = mine∉other then other∉mine) so both engines print identically.
Plain int bitwise + `<< >>` are unchanged (`<< >>` stay int-only). **Parity:** golden
`examples/collection_ops.chz` runs VM == interp == `.expected` (via `assert_file_parity`), confirmed on
`--serial` and `--parallel` too. Tests: 11 checker inference/rejection tests + VM eval-correctness +
list-repeat overflow recoverable-fault + the golden parity test (all RED-first). Docs:
`docs/syntax.md` §4 operator table + collection-operators note, `docs/stdlib.md` (list/set method
operator forms), `docs/grammar.bnf` (bitwise cascade note — same tokens, no grammar change; conformance
clean), `gaps.md` (gap #3 → RESOLVED log, open DX items renumbered 1..4). Full suite (2517) +
conformance + `clippy --all-targets -D warnings` clean.

**✅ DX — chained `else if` in expression-`if` (gaps.md DX gap #2) (2026-06-23).** `a := if p: 1
else if q: 2 else: 3` parses without parentheses. Parser-only (~10 lines): `parse_if_expr`
(`src/parser/mod.rs`) now branches after consuming `Else` — if the next token is `If` it captures the
inner `if` span and recurses into `parse_if_expr` for the else-branch (right-associative nested
`ExprKind::IfElse`), else the existing `else: <expr>` tail. Final `else` stays mandatory (the recursion
ends in its own `expect(Else)`). No checker/compiler/interp/VM change — the nested `IfElse` is the same
AST shape the hand-parenthesized workaround produced, so both engines already evaluate it byte-identically.
**Parity by construction.** Tests: 2 parser unit tests (chain nests right-associatively; chain still
requires final else) + golden `examples/expr_else_if.chz` (VM == interp == `.expected`). Docs:
`docs/grammar.bnf` (`<ifExpr>` + new `<ifExprTail>` tail rule), `docs/syntax.md` (chained example),
`gaps.md` (gap #2 → RESOLVED log, others renumbered). Full suite + conformance + `clippy --all-targets
-D warnings` clean.

**✅ Feature — FFI C-buffer alloc layer `std.ffi.alloc`/`alloc_zeroed`/`free` (feasibility-ladder
tier 3) (2026-06-22).** Allocate raw C-laid-out memory to hand to a C array/buffer API (`qsort`,
`bsearch`, `fread`-into-buffer): `alloc(nbytes) -> ptr` (`malloc`; garbage bytes),
`alloc_zeroed(nbytes) -> ptr` (`calloc`; zeroed), `free(p)` (`free`; returns nil). Fill/read with the
already-shipped `store_*`/`load_*` deref builtins — **no** bulk-copy helper (the loop idiom is the
surface). **Allocator:** direct `unsafe extern "C"` `malloc`/`calloc`/`free` (the **libc** allocator,
NOT Rust's `GlobalAlloc`), so a buffer may be handed to a C fn that reallocs/frees it and it pairs with
the same allocator `cffi`'s `owned_str` free path uses; extern decls resolve at link time, zero
per-call dlsym/libffi overhead. **Manual free** (`defer ffi.free(p)`) — a `ptr` is never auto-freed
(consistent with the FFI-ptr rule); forgetting **leaks**. **Faults (recoverable, never segfault/abort):**
`nbytes < 0` → `ffi.alloc: negative size`; `malloc`/`calloc` returning NULL for `nbytes > 0` →
`ffi.alloc: out of memory` (OOM checked only when `n > 0`, so a legitimate NULL from `malloc(0)` is not
mis-reported); `free(ffi.null())` is a **no-op** (does NOT route through `base_addr`); `nbytes == 0`
passes through (impl-defined). Double-free / use-after-free / OOB store_/load_ are the user's
responsibility (documented UB, no bounds/lifetime tracking — that's the deferred auto-buffer type).
`#[cfg(unix)]`-gated (non-unix registers the names but every call errors, mirroring the deref builtins).
**Parity by construction:** pure-additive on the engine-neutral `Host`/`NativeFn` seam — no VM/interp
edit — so VM == interp == M:N. **Wiring:** 3 new `MEMBERS` entries (now 59) in `src/native/ffi.rs` +
`native_module_sig`'s `std.ffi` arm (`src/checker/mod.rs`: `alloc`/`alloc_zeroed`:int→ptr,
`free`:ptr→nil). **Tests:** 5 ffi unit tests (roundtrip+free, zeroed-reads-zero, negative-size error,
free(null) no-op, MEMBERS coverage) + 1 checker sig test + 2 cffi two-engine parity tests (alloc+fill+
read+free; alloc_zeroed) + the **capstone `examples/ffi_qsort.chz`** golden on BOTH engines (sort a
Chezzi `int` list via libc `qsort` with a Chezzi `fn(ptr,ptr)->int` comparator that `load_int64`s both
sides — the marquee proof callbacks + deref + alloc all compose; also verified on `--parallel`). Full
suite + conformance + `clippy --all-targets -D warnings` clean. Docs: `docs/stdlib.md` (new alloc
surface + qsort idiom), `docs/ffi-and-packaging.md §1b` (tier 3 → LANDED; `qsort`/`bsearch` of a Chezzi
list now fully works; honest about what remains deferred: stored/cross-thread callbacks + variadics +
a GC-tracked owned-buffer), `docs/spec.md` + `docs/syntax.md` (FFI limits: manual C-buffer alloc now
available).

**✅ Feature — FFI memory-deref builtins `std.ffi.load_*`/`store_*` (feasibility-ladder tier 2)
(2026-06-22).** Read/write the **C-owned memory behind an opaque `ptr`** — for struct fields, return
buffers, event payloads, and C output-params a library hands you. Two-form API (fixed-arity native
fns, no variadic/optional machinery): a base form at byte offset `0` and an `_at(p, off)` byte-offset
form (the `_at` *store* takes the offset *before* the value). **Loads** (`-> int/float/bool/ptr/str`):
`load_int` (C `long`), `load_int8`..`load_int64` (sign-extend), `load_uint8`..`load_uint64`
(zero-extend), `load_float` (C `double`), `load_float32` (C `float`, widened), `load_bool`, `load_ptr`
(deref `void**`), `load_str` (copy a NUL-terminated C string, not freed). **Stores** (`-> nil`,
natural C width) mirror every width except `str` (`store_str` deferred — unbounded-write footgun).
**Reuse, not re-derive:** the loads/stores delegate to `cffi::read_field`/`write_field` (made
`pub(crate)`) — the *same* sign/zero-extend + truncation rules the callback/struct paths already use —
over a transient byte slice (`slice::from_raw_parts[_mut]`) at the natural width; `float32`/`str`
hand-roll (no f32 arm in `read_field`; `CStr::from_ptr` for the string). **Safety:** every fn rejects
a **NULL** base pointer with a *recoverable* `HostError` (`ffi.<fn>: null pointer`) **before** any
deref — the only cheaply-checkable guard; a dangling/misaligned/OOB *non-null* pointer is documented
UB (like `ctypes`). Mitigation `ctypes` lacks: a `ptr` is opaque and **cannot be forged from an int**
(provenance is C-sourced). Deref bodies are `#[cfg(unix)]`-gated (a non-unix build registers the names
but every call errors). **Parity by construction:** pure-additive on the engine-neutral `Host`/
`NativeFn` seam — no VM/interp edit — so VM == interp == M:N. **Wiring:** all 56 `std.ffi` members in
`MEMBERS` (`src/native/ffi.rs`) + `native_module_sig`'s `std.ffi` arm (`src/checker/mod.rs`).
**Tests:** 13 ffi unit tests (width/extend boundaries, `_at` offset, store→load round-trip, natural-
width store, NULL-error, MEMBERS coverage) + 3 checker sig tests + 3 cffi two/three-engine parity
tests (a `cc`-built `mkrec()` returning a `ptr` to `{int32 a@0; int64 b@8; double c@16}`, read/written
field-by-field). Full suite (2478) + conformance + `clippy --all-targets -D warnings` clean. Docs:
`docs/stdlib.md` (new `std.ffi` surface), `docs/ffi-and-packaging.md §1b` (tier 2 → LANDED; the
remaining gap at the time — `qsort`/`bsearch` of a Chezzi *list* needing a C-buffer alloc layer — has
**since landed**, see the tier-3 entry above), `docs/spec.md` (FFI v1 limits: `ptr` memory now
readable/writable), `docs/syntax.md`.

**✅ Feature — FFI sync scalar callbacks (callbacks #4, sync subset) (2026-06-22).** An `extern "lib":`
fn can now take a **function-typed parameter** spelled with the *existing* `fn(a, b) -> r` type (no new
grammar) whose params + return are all C scalars (`int`/`float`/`bool`/`ptr`/`int8`..`uint64`; no
`str`/struct/nested callback) — a Chezzi closure passed to C as a C function pointer that C calls
*back* synchronously, on the same thread, during the extern call. **Pipeline:** `CType::Callback{params,
ret}` + an `is_scalar()` helper (`src/native/cffi.rs`); the checker's `assert_marshallable` accepts a
scalar `Ty::Func` in **param** position only (a func-typed *return* is rejected) and `resolve_ctype_d`
lowers `Type::Func` → `CType::Callback`; `Cffi::call` builds a libffi `ffi_closure` trampoline (raw
`ffi_prep_closure_loc` + `low::closure_alloc`/`closure_free`) whose userdata holds a `*mut dyn Host` +
the arg index + the signature + a fault slot, pushes the trampoline's code address as the `void*` arg,
and frees the closure when `call` returns (**sync scope ⇒ no GC rooting**). **The one new engine seam**
is `Host::invoke_callback(arg_index, &[NativeRet]) -> NativeRet` (keyed by arg index so no engine
`Value` leaks across the FFI layer): the VM host re-enters via `guarded`+`invoke_value`; the interp
host gained a callback-capable `InterpCallbackHost` (holds `&mut Interp`, re-enters `call_value`) used
only by `call_cffi`. **Fault rule (stronger than ctypes):** the trampoline body is `catch_unwind`-
wrapped — a Chezzi fault or panic writes a zeroed C result (clean unwind), stashes the error, and
re-raises it as the extern call's own error (ctypes swallows to stderr + returns 0). **Tests:** a
`cc`-built `.so` fixture (`int apply(int,int(*)(int))` + a `double` variant) drives int/float
round-trips, fault + panic re-raise, and **two-engine + three-engine** (`--parallel`) parity (sync
callback fires on the calling worker thread — no cross-thread hand-off). 7 cffi tests + 6 checker tests
green; full suite (2459) + conformance + `clippy --all-targets -D warnings` clean. Docs: `docs/spec.md`,
`docs/syntax.md`, `docs/ffi-and-packaging.md §1b` (incl. the **feasibility ladder**: (1) sync scalar
done, (2) pointer-deref builtins → `qsort`/`bsearch`, (3) stored/cross-thread = own milestone, needs a
GC-rooting registry + thread-safe re-entry; **biggest caveat:** `--parallel` has **no GIL**, so
cross-thread is strictly harder than Python — needs a mini-GIL or thread-marshalling). `cc` added to
`[dev-dependencies]`.

**✅ Feature — one-way C-like `int`→`float` implicit widening (2026-06-22).** An `int` value now flows
into a `float` SLOT automatically, converted to a real `f64` (the reverse stays a lossy type error).
The design (Architecture C) emits a **real** runtime conversion at each value-DEFINITION boundary,
driven by the static annotation already in the AST — so it is byte-identical on the checked CLI path
AND the checker-bypassing parity harness (two-engine VM↔interp parity by construction; the M:N
`--parallel` engine shares the compiler so it is covered too). **Checker** (read-only): a scoped
`assignable_w(expected, actual, widen)` adds `(Float, Int) => true` only at compiler-coercible sinks
(typed binding, fn/method/closure args via `check_args_w`, returns, struct-field defaults, native/extern
float params) — the type-blind assign targets (`p.x = 3`, `xs[0] = 3`, `m[k] = 3`, tuple-target,
reassign-to-float-local) stay STRICT (no runtime hole); `infer_list`/`infer_map`-value unify an
int/float mix to `float` (one-way). **Compiler**: new cheap inline `Op::CoerceFloat` (mirrors `AsInt`,
reuses `n as f64`), emitted at typed binding, the float-param callee prologue (so an int *variable* widens
at the boundary, any caller), `-> float` returns (incl. inline-expr bodies), per-`float`-field struct
construction, and `float`-annotated / all-literal collection literals. **Interp** (frozen oracle, a
tree-walker — no bytecode): an equivalent `coerce_float`/`coerce_value_to_annotation` helper at the
SAME AST boundaries → parity. **Semantic proof:** `x: float = 3` makes `x / 2 == 1.5` (real float
division), not `1`. **Anti-lossy negatives stay type errors** (`y: int = 2.3`, `-> int: return 2.3`,
`float` into `List[int]`, `int`→`float` across a **newtype**, reassign-int-to-float-local). **Scoped
carve-outs (documented, not holes):** an un-annotated NON-literal mixed collection (`xs := [a, b]`,
a:int b:float) infers `List[float]` but its non-literal int element isn't widened at runtime; a plain
reassign `x = 3` to a float local is a strict (rejected) target. Tests: 9 checker + 11 two/three-engine
runtime (`widen_*`); native `sqrt(16)` / extern `cos(2)` widening confirmed hole-free (host promotes).
Docs: `gaps.md` → RESOLVED log, `docs/syntax.md §3`, `docs/spec.md`, `docs/stdlib.md`.

**✅ Bug fix — `ref` shared-method-name dispatch no longer falsely rejects an EXPRESSION receiver
(2026-06-22).** When ≥2 structs share a method name with differing param ref-ness (the receiver type
disambiguates which signature applies, per `docs/syntax.md §3`), a call with a *named-local* receiver
(`a := A(0); a.apply(r)`) type-checked but the equivalent *inline-expression* receiver (`A(0).apply(r)`,
or `mk().apply(r)` where `fn mk() -> A`) was falsely rejected ("expected Ref[int], found int") — an
over-rejection of valid code (safe, not unsound). Root cause was **desugar-only, pre-type**:
`callee_param_is_ref` resolved the receiver's struct (to pick the right sibling's `ref`-ness) only for a
named-local `Ident`; an expression receiver fell through to the agreement-gated name table, which returned
`None` for disagreeing siblings, so the `ref` arg was wrongly auto-deref'd before the checker ran. Fix:
new `receiver_struct_ty` helper resolves the receiver struct name for a named local, an inline ctor call,
AND a struct-returning free fn (new `ModReg::fn_ret_struct` map from the declared return type), driving
`methods_by_struct` uniformly. Desugar runs once before every engine, so VM == interp == serial ==
parallel is structural (no `src/interp` edit). Tests: `lowers_ref_arg_through_ctor_receiver_typed_method`
/ `..._fn_call_receiver_typed_method` (desugar), `ref_through_shared_method_name_ctor_receiver_ok` /
`..._fn_receiver_ok` + `ref_shared_method_byval_sibling_ctor_receiver_ok` (checker), extended
`examples/ref_indirect.chz` golden (stdout `42`, two-engine parity). Negative guards intact (single-struct
mismatch + by-value-into-ref still error). Docs: `gaps.md` entry → RESOLVED.

**✅ Bug fix — a struct/enum method whose name collides with a built-in method (`add`, `map`, `push`,
`len`, … the `BUILTIN_METHODS` list) now gets named- and default-argument support (2026-06-27).**
Previously the desugar `is_builtin_method(name)` guard (two sites in `src/desugar/mod.rs`) skipped ALL
method resolution for any builtin-colliding name — because the receiver MIGHT be a List/Set/Map/str the
pre-type pass can't see — so `c.add(amount=5)` on a user `Counter` was rejected with the misleading
"named arguments are only supported on functions, struct constructors, and struct methods" (it IS a
method). Fix: on the builtin branch, resolve via the already-existing receiver-type-aware lookup
(`receiver_struct_ty(obj)` → `methods_by_struct[(sname,name)]`) BEFORE bailing — when the receiver's
struct/enum type is statically knowable pre-type (a typed local, an inline ctor call, or a
struct-returning fn call) and that struct defines the method, the user method's spec drives full
named/default rewriting; a genuine builtin receiver (or an unknowable one) still returns None and is
left untouched (no name-keyed fallback that could mis-bind a builtin). `normalize_call`'s `method_spec`
arm + `callee_param_is_ref` both updated; the diagnostic for the unknowable-receiver case is now accurate
("method '…' reuses a built-in method name; named/default arguments need a receiver whose struct type is
statically known — bind it to a typed local or pass positionally"). Desugar runs once before both engines
⇒ two-engine parity is structural (no `src/interp` edit). Tests: 7 new desugar unit tests
(`builtin_named_method_*`, `enum_builtin_named_method_annotated_receiver`, accurate-error +
no-struct-defines guards) + the `real_builtin_set_add_untouched` / `builtin_method_name_not_normalized` /
`ambiguous_method_named_errors` boundary guards stay green; new `examples/builtin_named_method.chz` golden
+ `golden_builtin_named_method_chz_matches_expected_and_interp` (VM == interp). Docs: `docs/syntax.md`
limitation sentence rewritten; the `BUILTIN_METHODS` doc-comment updated. Known boundary (pre-existing,
not introduced here): an inferred enum receiver `m := E.Variant` is Field-shaped so its type isn't
statically known → falls to the accurate diagnostic (annotate the local or pass positionally).

**✅ Soundness fix — two missing duplicate/collision checks in the checker are now rejected (both
checker-only; two-engine parity preserved by construction — rejected programs never reach an engine,
accepted programs are byte-identical).** (1) **Import name collisions.** `bind_import` recorded a value
member via `declare()`, a function member into a separate `self.functions` map, and a module into
`imported_modules`, with **no cross-namespace duplicate check** — so `import v from vmod` (value) +
`import v from fmod` (fn) was UNSOUND (the checker resolved `v` to the value and `v + 1` type-checked,
but the runtime bound the function and faulted `cannot apply Add to function and int`), and `import f
from lib` + `import f from lib2` silently last-won. Fix: a per-module `import_binds: HashMap<String,
Span>` records every import bind-name across ALL namespaces; a second bind of an already-imported name
errors `'<name>' is already imported` (the bind-name = alias when present, so distinct names and `import
mod as alias` still pass; a missing member stays its own error). (2) **Duplicate binder in one pattern.**
`(x, x)` / `E.V(a, a)` was neither rejected nor treated as an equality constraint — it matched ANY
values and the arm was wrongly irrefutable (`f((3,9))` returned 9, not -1). Fix: `bind_match_arm` runs a
new `first_duplicate_binder` over each (non-Or, non-Wildcard) pattern and errors `identifier '<name>' is
bound more than once in this pattern` (Rust's rule); covers tuple / enum-payload / nested patterns. `_`
repeated, a name reused across SEPARATE arms, and an or-pattern `A(x) | B(x)` all stay legal. All in
`src/checker/mod.rs`; tests in `src/checker/tests.rs` (6 reject + 6 `*_ok` regression fences). `gaps.md`
"Import name collisions" + "Duplicate binding in a single pattern" → RESOLVED. Full `cargo test` +
`cargo test conformance` green; `cargo clippy --all-targets -- -D warnings` clean.

**✅ Soundness fix — refine-on-first-use is now PERSISTENT scope-wide first-use pinning (closes the
cross/post-branch `Ty::Unknown` residual).** The earlier design (entry below) was BLOCK-LOCAL: a
refine pin inside a conditionally-run body was snapshot/restored so it did not leak past the branch,
leaving cross/post-branch heterogeneous builds uncaught. Now the FIRST mutating op that fixes an empty
collection's element/key/value type **pins it for the binding's whole scope**, even across sibling
branches/arms — building a heterogeneous collection split across branches is a hard type error, exactly
like the literal `[1, "s"]`. Checker-only fix (`src/checker/mod.rs`): removed the
`snapshot_refinable`/`restore_refinable` barrier at the THREE STATEMENT-position sites — `check_block`
(if/else/while/defer), the `for` body, and statement-`match` arms (`check_match`, Option B: a cross-arm
conflict is a hard error). The pin already targets the binding's OWNING scope (`repin`), so it survives
`pop_scope` (which only removes inner-block-declared bindings — lexical scoping intact). The two
EXPRESSION-position sites (`infer_if_else`/`infer_match`) KEEP their barrier: a value-arm produces a
VALUE, so a pin in one value-arm must not leak to a sibling value-arm (would corrupt branch value
inference). Accepts the zero-trip / always-runs over-approximation by design (`xs:=[]; for i in []:
xs.push(1); xs.push("s")` rejects even though the body never runs — sound static over-approximation).
**New narrow residual** (documented in `gaps.md`): a differently-typed push done as a SIDE EFFECT inside
sibling if-EXPRESSION / match-EXPRESSION value-arms is still not caught (rare — a value-arm is a single
expression, the mutating ops are statements). Checker-only ⇒ VM==interp parity automatic. Tests:
`flow_sensitive_{if_else_int_vs_str,map_if_elif,set_if_else}_rejects`,
`refine_inside_block_persists_then_conflict_rejected`, `refine_{single_arm_then_concrete_use,
conflict_in_second_arm,stmt_match_arm_conflict,loop_body_pin_then_post_loop_conflict,
zero_trip_loop_over_approximation}_rejects`, `expr_arm_pin_independence_ok`; must-stay-green
`refine_inside_block_on_outer_list_ok` etc. All 2444 tests + conformance + clippy clean.

**✅ Soundness + tooling — un-constrained empty collection now errors (PART A) + retroactive hover for a
refined empty (PART B)** (`auto-task/empty-coll-infer`, checker-only, VM==interp parity-neutral). Two
related improvements to empty-collection element-type inference, sharing one end-of-scope finalize seam.
**PART A:** a bare `b := []`/`{}`/`Set()` whose element/key/value slot is NEVER inferred (only read into
an untyped sink — `print(b)`, `b.len()`) used to type-check silently as `List[Unknown]`; it is
now a static error `cannot infer element type of empty collection; add a type annotation`. Mechanism: the
let-handler's un-annotated branch records a pending site `(owning_scope_idx, name, decl_span)` in
`empty_coll_sites` when the declared type is an empty literal shape (`is_unrefined_empty_coll` — a
List/Set/Map whose DIRECT slot is bare `Unknown`; `[[]]`=`List[List[Unknown]]` is NOT empty and excluded,
as are `None`/nullary-variant `Unknown`-in-slot producers), gated `!inferring_ret` so return-inference
passes don't double-record. A later **constraining** op clears it via `drop_empty_site(name)`: the two
refine gates (`refine_receiver`/`refine_index_receiver`, before their speculative-error truncate-returns,
so an erroring mutator arg like `xs.push(undefined)` still drops the site and its exactly-one-error tests
stay green), AND — so the rule never rejects a binding that *is* constrained, just not through a mutator —
a concrete-typed value flowing into the binding: a whole-binding reassignment / compound-assign /
tuple-assign (`check_assign`'s Ident arm, gated on the value being fully concrete so reassigning *another*
empty `b = []` does NOT clear it), or passing/returning it into a CONCRETE collection sink (a typed param
in `check_args_range_w`, a typed `return` in `check_return`). `finalize_empty_coll_sites` runs before
`pop_scope` at the fn-body + module seams and errors on any still-unrefined site owned by the popping
scope. **False-positive guards fall out structurally for the literal sinks** (annotation
`b: List[int] = []`, typed param `f([])`, typed `return []`, turbofish `List[int]()` leave no
`Unknown`-in-slot or bind no local → never recorded) **and are dropped explicitly for the one-binding-away
sinks** (`b := []` then `f(b)` / `return b` / `b = [1,2]` / `a, b = [1], [2]`). A post-merge adversarial
review found the one-binding-away drop missed the case where the empty binding is read as an **RHS value
that escapes** into another binding/structure — `c = b` / `bx.items = b` (assign), `c := b` (alias),
`c := [b]` (nested in a literal) — spuriously erroring on `b` though the program is type-sound; fixed with
`drop_value_escape_sites(value)` at the let + assign seams (drops the source ident's site; the alias
records its own if it stays unrefined, so the requirement *moves* rather than vanishes — no false-negative).
A terminal non-escaping read (`print(b)`, `b.len()`) is intentionally NOT a drop, so the headline error
still fires. Scope coverage is fn-body
+ module (an empty declared inside an if/for/match body that pops before the seam is a documented
residual, matching the refine machinery's block-local limits). **PART B:** retroactive hover — when the
probe lands on an occurrence of a binding whose recorded type still carries `Unknown`-in-slot,
`hover_record_binding` does NOT lock `hover_result`; it stashes `(owning_scope_idx, name, kind, doc)` in
`hover_pending`, and `finalize_hover_pending` (same seam) overwrites `hover_result` with the binding's
FINAL refined type via `lookup`. So hovering the `b := []` decl (or any use before `b.push(0)`) now shows
`List[int]`, not `List[Unknown]`. The owning-scope index gates the finalize (`owning >= idx`, mirroring
`finalize_empty_coll_sites`): a post-merge review caught that without it, an intervening fn/method
`check_fn_body` seam between a module-level empty decl and its refining op would prematurely lock the
hover to the still-unrefined type — so the finalize only resolves at the seam that OWNS the pending
binding (regression test `hover_refined_empty_decl_intervening_fn_shows_final_type`). Entirely
`hover_probe`-gated → parity-neutral by construction. Tests: `checker::tests`
`unconstrained_empty_{list,map,set,at_module_level}_rejected` + full typed-sink ok matrix
(`typed_annotation_*`, `typed_param_empty_arg_ok`, `typed_return_empty_ok`, `turbofish_empty_ctor*`,
`empty_push_then_read_no_false_error`) + the one-binding-away constrained matrix
(`empty_then_{plain_reassign,compound_assign,tuple_assign,reassign_from_call,conditional_reassign}_concrete_ok`,
`empty_binding_into_typed_{param,return}_ok`, and the `empty_then_reassign_still_empty_rejected` guard);
`editor::tests::hover_refined_empty_{decl,pre_use}_shows_final_type`.
Annotated the 3 shipped examples that relied on the old permissiveness and have no later constraint
(`edge_cases.chz`, `map.chz`, `concurrent_collection_test.chz`); `bst.chz`'s `walk := []` stays
un-annotated (its `inorder(root, walk)` call into a `List[int]` param now constrains it). All tests +
conformance + clippy clean.

**✅ Soundness fix — empty-collection / nullary-variant / `None` `Ty::Unknown` slot is now closed via
FULL refine-on-first-use + insertion-site Hashable check + (originally BLOCK-LOCAL, now PERSISTENT —
see the entry above) flow-sensitivity (the
empty-slot half of the `Ty::Unknown`-is-assignable family; sibling to the recursive-return fix below).**
A bare empty literal (`[]`/`{}`/`Set()`), a nullary user-enum variant (`Box.Empty`), or native `None`
typed its element/key/value/type-arg slot as the permissive `Ty::Unknown`, which nothing later refined —
so `x:=[]; x.push(1); x.push("s")` passed `check` then faulted at runtime, and the deliberate
float-key/Hashable ban was bypassed (`m:={}; m[1.5]=...`, `s:=Set(); s.add(nan)`). Fix (checker-only,
`checker/mod.rs`): `refine_receiver` (top of `infer_method_call`) and `refine_index_receiver`
(`check_assign` Index branch) — when a **simple-variable** binding's type carries `Unknown` in a slot
(detected by `contains_unknown_in_slot`, recursing through list/set/map/Option/Result/tuple/Channel/
Shared/Atomic and user generic struct/enum), the FIRST mutating op (`.push`/`.add`/`.insert`/`.extend` /
`x[k]=v`) that supplies a concrete type RE-PINS the binding at that slot via `merge_unknown` (which
recurses into nested type params — `List[Option[Unknown]]` + `Some(5)` → `List[Option[int]]`, `[Box.Empty]`
+ `Box.Full("hi")` → `List[Box[str]]`). A later INCOMPATIBLE concrete type is then a normal `check_args`
mismatch, enriched to hint at annotating for a mixed/protocol collection. Heterogeneous/protocol
collections now REQUIRE an explicit annotation (`shapes: List[Shape] = []`) — intended and clearer.
Non-Hashable keys/elements are rejected by a DIRECT insertion-site `is_hashable_key` check at `m[k]=v`
(fires even while the key type is still `Unknown`) and at set-element concrete-ification. **Flow-
sensitivity** (now PERSISTENT scope-wide first-use pinning — see the entry above; originally block-local
via `snapshot_refinable`/`restore_refinable`): a refine pin at a STATEMENT-position site (`check_block`,
the `for` body, statement-`match` arms) now PERSISTS for the binding's whole scope, so `xs:=[]` + `if c:
xs.push(1) else: xs.push("s")` is **rejected**; the EXPRESSION-position arms (`infer_if_else`/
`infer_match`) keep their restore so value-arms refine independently.
**Residuals** (documented): simple-variable-receiver-only (`obj.field`/`f()`/`xss[0]` unrefined), and
side-effect pushes inside sibling EXPRESSION-position arms (the cross/post-branch STATEMENT leak is now
closed). **Golden-test
checker-bypass fixed:** the golden tests drive `run_capture`, which BYPASSES the Checker, so a checker
regression on a shipped example shipped falsely green — added `checker::tests::all_shipped_examples_typecheck`
(build_graph + check_graph over every `examples/*.chz`, two intentional run-only demos `panic.chz` /
`explicit_type_args.chz` allow-listed) and annotated `examples/poly_method.chz` `List[Shape]` under the
new rule. Checker-only ⇒ VM==interp parity automatic (newly-failing programs fail `check` before either
engine runs; passing programs run byte-identical). All 2394 tests green; clippy + conformance clean.
`gaps.md` updated (empty-collection + generic-nullary-variant producers RESOLVED; all three `Unknown`-in-slot
producers now closed).

**✅ Soundness fix — return-type inference is now ORDER-INDEPENDENT (fixpoint), closing the
recursive/forward-reference half of the `Ty::Unknown`-is-assignable hole.** The checker inferred
function/method return types in a single SOURCE-ORDER pass and bailed to `Ty::Unknown` whenever the
deciding `return` was a call to a not-yet-inferred function (a forward reference, or mutual recursion).
`Unknown` is universally assignable, so a bogus return flowed check-blessed into a typed slot and
faulted at runtime (`fn rec(n:int): if n<=0 return base(0) else return rec(n-1)` + later
`fn base(n:int): return "hello"`, then `v: int = rec(2)` wrongly passed `check` — `rec` really returns
`str`). Fix: `infer_returns` (`checker/mod.rs`) now wraps the per-pass walk (`infer_returns_pass`) in a
bounded FIXPOINT — re-infer every un-annotated fn/method until no stored `FnSig.ret` changes (cap =
un-annotated-count + 1; monotone, a concrete ret is never reverted to `Unknown`, so it converges and the
final ret is order-independent). A self-recursive call still contributes no type; the non-recursive
returns decide (so `fact`/`fib` are unchanged — base-case concrete wins). Divergent CONCRETE returns
stay the user's job to annotate (`-> T` or a protocol existential `-> Stringable`); with no annotation
conflicting concretes are an `expected return type …, found …` error — **no union types**. A genuinely
un-inferable un-annotated fn/method (pure self-recursion, or mutual recursion with no concrete base
anywhere — ret stays `Unknown`) keeps a **permissive** type, NOT rejected: a blanket "leftover Unknown
⇒ require annotation" check over-reaches (bare `Unknown` is also produced by non-recursive paths like
`return x[0]` of an empty collection, and by already-errored bodies), so soundly rejecting only the
recursive-no-base case needs call-graph cycle detection — tracked as a follow-up. Checker-only change ⇒
VM==interp parity automatic. `gaps.md` "Ty::Unknown is treated as assignable" updated (recursive-return
producer RESOLVED; empty-collection = sibling task, generic-nullary-variant remains). Tests green;
clippy + conformance clean.

**✅ Soundness fix — string-interpolation fragments are now type-checked (was a CRITICAL compiler
panic + unsound `check`).** The checker treated an interpolated `str` as opaque `Ty::Str` and never
resolved/type-checked the `{…}` fragment exprs, while the compiler hard-assumed the checker already
rejected undefined names — so `print("{nope}")` passed `check` then panicked the compiler at
`global_slot` (`compiler/mod.rs`), and every type/method/arity error inside `{…}` escaped `check`
entirely. Fix: the `ExprKind::Str` arm now parses the literal with the shared interpolation parser and
`infer_value`s each fragment (`checker/mod.rs::check_interpolation`), so undefined names + type errors
surface as compile errors at the string's span and `global_slot`'s invariant holds (panic impossible).
The compiler's private interpolation parser (`Chunk`/`parse_interpolation`/`parse_expr_str`) was
extracted into a new shared leaf module `src/interpolation.rs` (neutral `InterpError`; compiler and
checker each map it to their own error type) so both engines chunk strings byte-identically — two-engine
parity preserved (no `interp` edit needed; the new check is a pre-run gate). Pinned by
`checker::tests::interpolation_{undefined_name_rejected,type_error_rejected,valid_ok}`. Full `cargo
test` (2365) + `cargo test conformance` green, `cargo clippy --all-targets -- -D warnings` clean.

**✅ `chezzi docs` + `module:function` entrypoint + stdlib reference (tooling/docs).** Three related
changes: (1) **`chezzi docs [topic]`** prints embedded language docs — topics `spec`/`syntax`/`stdlib`,
and a bare `chezzi docs` (or `docs llms`) emits the full reference bundle (spec+syntax+stdlib) for
piping to an LLM. Docs are `include_str!`-embedded so the
binary is self-contained; logic is a pure `render_docs` (unit-tested), `cmd_docs` just prints/maps to
`ExitCode`. (2) **`module:function` entrypoint:** `chezzi.toml`'s `entrypoint` now accepts a
`:function` suffix (`"src.main:main"`) — a bare `chezzi run` runs the module top-level and then calls
that function (missing/non-function = clear error), so the source needs no trailing call and you can
swap which function runs via the manifest. Bare `"src.main"` keeps the old run-top-level behavior;
explicit `chezzi run <file>` is always top-level-only. Implemented via `main::split_entrypoint` +
`vm::invoke_entrypoint` (reuses `invoke_value`/`entry_home`) threaded through a new
`run_file_with_entry`; the old `run_file_with`/`run_file_parallel` became `#[cfg(test)]` parity-test
helpers. Scaffold now writes `entrypoint = "src.main:main"` and a `main.chz` with no trailing call.
(3) **New [`docs/stdlib.md`](docs/stdlib.md)** — the previously-undocumented stdlib/builtin surface
(global builtins, per-type methods, runtime types, native + pure-Chezzi `std.*` modules); `syntax.md
§13` shrank to a pointer + orientation. Docs synced (`spec.md`, `syntax.md §9b`, `CLAUDE.md`,
`manifest.rs`). VM↔interp parity untouched (entrypoint is VM-only; no `examples/*.chz` changed).

**✅ Enum methods (mirrors the struct-method machinery end-to-end).** Enums now accept `fn name(self, …)`
method blocks after their variants, parsed via the same `parse_fn(true)` path structs use; the parser
enforces variants-before-methods. (`test fn` is **rejected** in enum bodies — enum test *suites* are not
wired in the compiler/test-runner, so a `test fn` would silently never run; rejected at parse time as a
follow-up. A `Hashable` enum's `hash(self)` is dispatched at runtime in both engines, so `Set[E]`/`Map[E,V]`
keys work — not just type-check.) The checker gained a name-keyed
`enum_methods` map (+ `EnumSigInfo.methods` ferried across the module boundary on both the whole-module
and `from`-import paths) and a `Ty::Enum` arm in `infer_method_call` (with generic-enum `T`-substitution),
in `satisfies_args` (cloned from the struct arm into a shared `satisfies_methods` helper — unlocks
`Stringable`/`Hashable`/`Add`/`Sub`/`Mul`/`Comparable` for enums and protocol-bound generics), and in
`op_overload_result`/`ordering_allowed`. The desugar collectors (`collect_methods*`, `validate_defaults`,
the walk) now treat struct + enum methods uniformly (name-keyed; `normalize_call` unchanged). Both engines
bind the whole enum value as `self`: the VM added `Program::enum_methods`/`enum_home`, an `Obj::Enum` arm
in `do_method_call`, a shared `resolve_overload_method` used by `struct_arith`/`struct_compare`, and the
`str(self)` Stringable hook in `stringify`; the interp mirrors all of it (`enum_defs` registry, an enum
branch in `call_struct_method`, its own `resolve_overload_method`, the stringify hook) — kept byte-identical
(golden `examples/enum_methods.chz` runs on VM + interp + parallel + `.expected`). **Follow-up lever:** the
method IC is skipped for enums (type-erased → no `tid`); enum-method dispatch uses the slow `run_proto`/
flatten path. **Out of scope (deferred):** `derive` and the multi-bound same-name-method
ambiguity diagnostic (a pre-existing struct-era wart, first-bound-wins). (Nominal `newtype` — once
listed here as deferred — **shipped in M21**; see its section below.)

**✅ Module-scoped user types (struct / enum / `type` alias).** Types are now **private to their
declaring module**, mirroring how top-level functions are namespaced — exported by default (no `pub`),
visible elsewhere ONLY via import. `import core.geo` → `geo.Point(1,2)` / `x: geo.Point` /
`List[geo.Point]` / `geo.Color.Red`; `import Point from core.geo` → bare `Point(1,2)` (rename allowed
for user types). A bare use of a type whose module was imported whole but not named-imported is a
**check-time error** with an import hint. Two modules MAY declare the same type name (no collision).
Enforcement lives in the **checker** (per-module type tables: `structs`/`enums`/`variants`/`aliases`
cleared per module + re-injected via `bind_import`; `ModuleSig` carries resolved struct/enum/alias
defs; reverse `types_by_name` index drives the hint; new `Type::Qualified{module,name,args}` AST +
parser `m.T[args]` production). Runtime keying is the **always-qualified identity key + bare display name** model (ROOT REDESIGN,
2026-06 — replaced the old "Option C" bare-key/disambiguate-on-collision scheme, which was a bug
factory: the key doubled as the printed name, so consumers had to know bare-vs-qualified and several
got it wrong, e.g. `json.decode` decoding a collision-loser against the WRONG layout). The new design
**separates identity from display**: (1) **IDENTITY KEY** is ALWAYS `<module-key>::Name` for EVERY user
struct/enum/variant/alias — no winner/loser, no bare keys, unique by construction (the module key is
the declaring module's dotted path or the entry file's stem, from the shared
`resolver::module_keys(graph)`, deterministic + `#idx`-tiebroken so all three engines derive it
byte-identically). The compiler, checker, both engines, AND the `--parallel` snapshot/wire format key
every layout table (`Program::structs`/`variants`, checker tables, interp `struct_fields`, …) by this
ONE key; the value's runtime tag carries it. (2) **DISPLAY NAME** is the bare `Name`, stored on the
def (`StructDef::display_name`): ALL user-facing output — print/`str`/stringify, errors, `json` ENCODE,
`repr` — renders it, so output is **byte-identical** regardless of module and two colliding `Point`s
both print `Point(...)`. Because there is ONE canonical key, the whole bug class vanishes structurally:
`json.decode` (`json_decode::DecodeEnv`, implemented by both engines) resolves the target — and nested
struct-field types **in their own DEFINING module's scope** — to the qualified key, tags the produced
struct with it, and decode errors render the bare name. RESERVED/NATIVE types
(`Result`/`Option`/`Some`/`Ok`, `Ref`, `Iterator`, `Match`/`Response`, the std type surface on
`import std.*`, and the FFI width names) are **not** module-keyed — they keep their bare name (the
qualification pre-pass skips std/native modules). A match pattern `Color.Red` against a whole-module-
imported enum is resolved **SCRUTINEE-DRIVEN** on every engine: the matched value carries its own
qualified enum identity key (the very enum the checker resolved the scrutinee to), and an arm matches
iff its written qualifier equals that key's BARE form (interp `try_bind`: `bare_display(ty)==en`; VM
`match_arm`: the M19 int-id fast path, with a `bare_display(enum_key)==enum_name` fallback baked into
`Op::MatchArm.enum_name` on an id MISS). It is NEVER re-guessed by iterating the (RandomState-seeded)
import map — doing so ignored the scrutinee and picked nondeterministically (often the WRONG enum when
two whole-imported modules declared a same-named enum); the construction side (`enum_bare_key`) still
resolves against the current module context, which is correct. The same deterministic key map + per-module bare-visible-type set
is computed identically by all three engines, so the cooperative VM, `--parallel`, and the interp agree
on every key (3-engine parity, incl. a genuine collision: field access, method call, `match`, AND
`json.decode` on a colliding type, plus a cross-airlock imported-type value). The runtime `bind_import`
(both engines) binds a
member's value when the TARGET module exports one and skips only a value-less TYPE member (so a
`from`-imported fn named like another module's type still binds); the bare constructor fires only for
a type bare-VISIBLE in the importing module. Imported `type` aliases are **transparent** (body
resolved in the defining module's scope, carrying the FFI-width license; an unlicensed alias embedding
an un-imported width is rejected at import). Reserved/native types (`Result`/`Option`/`Some`/`Ok`,
`Ref`, the std type surface on `import std.*`, FFI widths) stay global/bare always. New grammar
production in `docs/grammar.bnf` (`conformance` green). Docs: `docs/spec.md` + `docs/syntax.md`
(Imports). This is a **pre-JIT sequencing gate**, not a feature freeze — new language work can still land.

**✅ Redesign follow-up — two regressions fixed (2026-06).** The qualified-identity-key redesign
introduced two bugs (caught by adversarial review, reproduced on the built binary), now fixed: (1)
**checker errors leaked the qualified IDENTITY key** (`type single::Point has no field 'nope'`) — the
identity-vs-display split was applied at runtime stringify but NOT in the checker's `format!("type
{ty} …")` paths; fixed at the single choke point — `Ty`'s `Display` for `Struct`/`Enum` now renders
`bare_display(n)`, so every field/method/type-mismatch error (single- and cross-module) prints the
BARE name. (2) **bare match-pattern enum was resolved NONDETERMINISTICALLY** by iterating the
RandomState-seeded import map (scrutinee-blind), alternating wrong-arm / `MatchNoArm` crash across
identical runs and disagreeing between engines — now **scrutinee-driven** (see the match-pattern
resolution note above), deterministic + identical on VM / `--serial` / `--parallel` / interp.

**✅ CLI cleanup + parsed `chezzi.toml` entrypoint (5 scoped changes; no engine/semantic change).**
Quality-of-life + a small manifest reader, zero new deps. (1) **Sample-string rename** `"thuan"` →
`"chezzi"` across docs/examples/tests (input + expected kept in sync; width-10 format examples in
`docs/syntax.md` recomputed for the 6-char name). (2) **Milestone tags removed** from the `chezzi help`
COMMANDS block. (3) **`--interp` CLI flag dropped** — the tree-walk interpreter stays as the FROZEN
two-engine parity oracle (golden VM-vs-interp tests call it directly), but it has no CLI surface; `mod
interp` is now `#[cfg(test)]` (test-only, where every reference lives). (4) **Hand-rolled
`chezzi.toml` parser** (`src/manifest.rs`): a tiny fixed-schema reader — `[section]` headers,
`key = "value"` string pairs, `#` comments; captures `[project]` `name`/`version`/`entrypoint`; an
EMPTY manifest parses to all-`None` (the existing root-marker fixtures stay valid); malformed lines
are a clean `Err`. (5) **Bare `chezzi run` runs the manifest entrypoint**: with no file argument it
walks up from the cwd for `chezzi.toml` (`resolver::find_root_from_dir`), parses it, requires
`[project] entrypoint` (a dotted module path), and resolves it root-relatively via
`resolver::module_file` → e.g. `<root>/src/main.chz`, then runs it on the VM honoring all flags.
Imports stay **root-relative** (`build_graph` walks up to the same marker) — locked by a tempdir test
(`entrypoint_imports_are_root_relative`: `import lib` → `<root>/lib.chz`, `import src.utils.common` →
`<root>/src/utils/common.chz`). `chezzi init` now scaffolds an **active** `entrypoint = "src.main"`,
so a freshly-init'd project runs with a bare `chezzi run`. Verified end-to-end: `init` a tmp project →
bare `chezzi run` (+ `--serial`, + nested-cwd) prints `Hello from Chezzi!`, `chezzi run src/main.chz`
unchanged, `chezzi test .` passes, `chezzi run --interp` → `unknown flag`, `chezzi help` shows no
`(M..)` tags/`--interp`. Docs: `docs/spec.md`, `docs/syntax.md`, `CLAUDE.md`, this file.

**✅ Project tooling — `install.sh` + `chezzi init [dir]`.** Quality-of-life, no runtime/semantic
change, no new deps. `install.sh` (POSIX `sh`, `set -e`, executable) guards for `cargo` on PATH
(hinting https://rustup.rs if missing), then `cargo install --path .` and reminds the user to keep
`~/.cargo/bin` on PATH. `chezzi init [dir]` (new `cmd_init` + pure `scaffold_project` in `src/main.rs`,
unit-tested against a TmpDir) scaffolds `chezzi.toml` + `src/main.chz` (`fn main():` + a top-level
`main()` call — no auto entrypoint) + `src/main_test.chz` (`test fn` + `assert`); `dir` defaults to `.`,
is created if missing, and an existing `chezzi.toml` is refused (no clobber). The manifest is both a
root marker AND a parsed manifest (see the CLI-cleanup entry above): the toolchain reads its
`[project]` keys, and `entrypoint` (scaffolded active as `"src.main"`) drives a bare `chezzi run`;
`run <file>` stays top-to-bottom and `test` still discovers `*_test.chz`. Verified end-to-end:
`chezzi init <tmp>` → `chezzi run <tmp>/src/main.chz`
prints `Hello from Chezzi!` → `chezzi test <tmp>` reports `2 passed`, and re-`init` refuses with a
non-zero exit. Docs: `docs/syntax.md` §9b, `docs/spec.md` (module-resolution section), `CLAUDE.md`.

**✅ Formal `Iterable[T]` protocol + `.iter()` cursor (owner-requested; the decoupled follow-on the
constructors work flagged).** Additive — nothing existing changes behavior; 3-engine parity throughout.
The win: a plain collection now composes into the SAME lazy adapter pipeline as a hand-written struct
iterator (`Take([10,20,30,40].iter(), 2)`, `Mapped([1,2,3].iter(), fn)`) — impossible before, since
you can't call `.next()` on a `list`. Wired (mirroring the `bytes`/`bytearray` Obj/Value pattern):

- **`Iterable[T]` prebuilt protocol** `{ iter() -> Iterator[T] }` — reserved + registered next to
  `Iterator[T]` (unchanged). The looser sibling: `Iterable` promises only a cursor; `Iterator` also has
  `next`, so every `Iterator` IS `Iterable` (`iter()` returns self). Conformance via `iterable_elem`
  (collections + any `Iterator` intrinsically via `iter_elem`, + a struct with structural `iter`).
- **Cursor heap object** — VM `Obj::Iter { items: Vec<Value>, pos }` (32B, 88B-guard green) and interp
  `Value::Iter(Rc<RefCell<IterCursor>>)`. The TYPE is the existing `Iterator[T]` existential — NO new
  `Ty`. GC-**NON-LEAF**: `children()` traces `items` (contrast `Bytes`/`ByteArray` leaves) so a
  not-yet-consumed snapshot element survives a collection. `.next()` → `Some(items[pos])` + advance,
  idempotent `None` past the end. deep_clone → a fresh in-task copy (airlock).
- **`.iter()` dispatch** — on `list`/`set`/`map`(→keys)/`str`(→char)/`bytes`/`bytearray`(→int): a FRESH
  cursor SNAPSHOTTING current contents in EXACTLY `for x in X` order (reuses `drain_iterable` /
  `iter_rows_from_value`, the for-loop's single source of truth). On any `Iterator[T]` value (cursor,
  generator, `next`-struct): returns SELF (idempotent). `List(xs.iter())`/`Set(...)` drain for free.
- **For-loop additive case** — a struct with `iter()` but NO `next()` is for-iterable via a one-time
  `.iter()` then the cursor drains: checker for-bind arm AFTER the `next` arm (a struct with BOTH keeps
  the `next()` fast path — back-compat precedence); VM `Op::IterableToCursor` (one-time, before the
  per-iteration loop — structs-with-`next`/generators pass through byte-identical); interp `exec_for` /
  `drain_value_to_rows` sibling branch. The hot collection / `next`-struct paths are untouched.
- **Sendability** — a cursor IS sendable: it crosses the `spawn`/channel airlock as a DEEP COPY, like a
  `list`. `to_wire`/`from_wire` carry a `WireValue::Iter { items, pos }` (items recursively wired, `pos`
  carried) and `to_snap`/`replay_snap` a `SnapValue::Iter`; the interp's `deep_clone` already deep-copies
  the cursor identically, so all three engines agree. A cursor over a non-sendable element (e.g. a
  generator) faults recoverably via the recursion, exactly as a `list` of that element would. (`sendable_rec`
  is UNCHANGED — a cursor reuses `Iterator[T]`'s type, already sendable; no static change was needed. An
  earlier cut gated the cursor non-sendable like a generator, which panicked the spawned VM worker while
  the interp succeeded — a parity divergence, now fixed.)
- **Generator airlock = graceful runtime error, never a panic** — a frame-holding generator (a value from
  calling a generator `fn`) shares the `Iterator[T]` existential with a cursor, so the checker cannot
  distinguish them; the RUNTIME is the enforcement point. A generator crossing **any** airlock-out site
  raises a catchable `a generator cannot be sent across tasks` error with the real spawn/nursery-site span:
  `to_snap`/`snapshot_modules`/`ensure_snapshot` are now fallible (the choke point re-stamps `to_wire`'s
  placeholder `Span{0,0}` with the nursery span; `ensure_snapshot` memoizes only on success), and the
  smuggle sites (`deep_clone` for `spawn` args/`spawn:` captures, `Op::NewShared`, `new_atomic`,
  `Channel.send`/`try_send`, `Shared.set`/`update`, `Atomic.store`/`exchange`/`cas`, plus `wire_args` /
  `wire_callable` for spawn-method args + `Executor.submit` closure captures) re-stamp via a shared
  `to_wire_at` helper. The **module-global** path was the missed-critical site: the M:N engine eagerly
  snapshots EVERY module global at the first nursery, so a module-level generator + any `parallel:` block
  previously aborted via `to_snap`'s `unreachable!` even when no task touched it — now graceful. (Parity is
  per-engine, NOT `assert_parity`: interp rejects `yield` EARLIER at gen() with a different message; both
  engines still reject the program. Tests `generator_module_global_with_nursery_is_graceful_vm` +
  siblings.)
- **NON-GOALS (documented, not built):** multi-pass/single-pass TYPE SAFETY (unfixable without
  move/ownership — `count_twice([list]) == 6` via two independent cursors vs `count_twice(generator) ==
  3` consumed once; each `.iter()` is fresh, but reusing an exhausted cursor yields nothing); auto-
  `.iter()` inside adapters (v1 requires explicit `xs.iter()`); routing builtin for-loops through
  `.iter()` (the fast path stays); cursor `.reSet()`/`.peek()`/`.rev()`/`size_hint`.
- **grammar.bnf intentionally UNCHANGED** — `.iter()` is the existing method-call production, no new
  syntax (`cargo test conformance` green).
- **Tests/golden:** checker `iter_method_on_collections_types_as_iterator` /
  `iterable_bound_accepts_list_and_generator` / `iter_idempotent_on_generator_and_cursor` /
  `iterable_struct_with_only_iter` / `iter_cursor_drives_existing_adapters`; VM/interp parity
  `iter_next_idempotent_both_engines` / `iter_snapshot_order_matches_for` / `cursor_composes_into_adapter`
  / `for_over_pure_iterable_struct` / `list_of_cursor_roundtrip_both_engines` /
  `cursor_crosses_spawn_airlock_three_engine_parity` / `cursor_crosses_airlock_by_deep_copy` / `generator_iter_returns_self_vm`;
  GC `obj_iter_traces_items_as_gc_children`; `examples/iterable.chz` + `.expected` goldened 3-engine.

**✅ Checker — declared-non-void fn must return a value on every path (Option B).** A function body is a
sequence of **statements**, not an expression, so an inline body `fn a() -> int: 10` parses `10` as a
discarded expr-statement and silently falls off the end to `nil` (this was mis-filed in `gaps.md` as a
"bare fn name not callable / dispatch bug"; the real root cause is a **missing-return check** — dispatch
was always correct). The checker now rejects a function with a **declared non-void return type** whose
body can fall off the end without a value `return`, with a hint to add `return` or use a closure
`fn() -> T: <expr>` (whose body IS an expression and implicitly returns). The analysis
(`checker/mod.rs` `block_terminates`/`block_has_break`) is **sound/conservative** — never false-positives
on valid code: an `if`/`else` where every branch returns, an exhaustive `match` where every arm returns, a
`while true:` with no reachable `break`, and an `exit(...)` tail all count as terminating. A bare
`fn a(): 10` (no annotation → infers `nil`) and closures are **exempt**. `examples/edge_cases.chz`'s 6
inline non-void fns rewritten to multiline `return <expr>` (two-engine golden byte-identical). Docs:
`docs/syntax.md §5`, `docs/grammar.bnf` (comment), `gaps.md` (RESOLVED). All cargo wrapped at MemoryMax=6G;
full `cargo test` (2040) + `cargo test conformance` green, `cargo clippy --all-targets -- -D warnings`
clean.

**✅ Checker/semantics — inline-expr fn body implicitly returns + `nil` rejected in value position
(amends Option B).** Two coordinated changes, both two-engine (VM == interp) parity:
- **PART 1 — inline-expr body implicit return (Option A, inline-only).** A named fn written in the
  **inline** form (`fn a(): <expr>` on one line) whose single statement is a **bare expression** now
  **implicitly returns** that expression — exactly like a closure `fn(x): expr`. `fn a(): 10` returns
  `10` (inferred `-> int`); `fn dbl(x): x*2` works as a value / `.map` arg; `fn a() -> int: 10` is now
  **valid** (Option B's fall-off check is exempted for inline-expr bodies). A **multiline** 1-stmt body
  still does **not** implicitly return, and a declared-non-void multiline body still needs an explicit
  `return`. An inline **non-expression** statement (`fn a(): x = 5`) stays as-is (nil). The parser
  distinguishes the inline-expr body from a 1-stmt indented block (which `Block = Vec<Stmt>` otherwise
  erases) via a new `FnDecl.inline_expr_body` flag (`peek_at(1) != Newline` after the body colon +
  single `StmtKind::Expr`). The compiler (`compile_fn`) and interp (`call`) mirror `compile_closure`/
  `call_closure`: compile/eval the expr and Return its value. Return-type inference (`infer_fn_ret`) uses
  the inline expr's type as the inferred return.
- **PART 2 — `nil` used as a value is a type error.** A `Ty::Nil` (void) expression in **value
  position** — assignment RHS, a call/collection/tuple argument, a binary/unary operand, an index/range
  bound — now errors *"expression returns no value (nil) and cannot be used as a value"*, instead of
  silently propagating (`x := print(...)`, `print(log(...))`, `[log(...)]`, `1 + sort()`). A bare void
  call **as a statement** (`print("hi")` on its own line) stays legal, and returning `nil` from a fn
  (making it void) is **not** "using nil". Implemented as one `Checker::infer_value` helper routed
  through every value-position site (Let/Assign RHS, list/set/map/tuple/comprehension elements,
  `infer_binary`/`infer_unary`, `infer_index`/`infer_slice`, `expect_int`/`expect_bool`,
  `check_args_range`/`infer_all`/`one_arg`, and the builtin/constructor arg paths) — statement-position
  `infer` (`StmtKind::Expr`) and return-position `infer` (the inline-expr body, closure body) are left
  unchanged by design.
- Composition: `fn a(): print("x")` infers `-> nil` (a void fn, OK), but `y := a()` is then rejected.
  No grammar change (both reuse existing productions) → `cargo test conformance` stays green.
  `examples/inline_fn.chz` + `.expected` goldened (VM == interp). Docs: `docs/syntax.md §5`,
  `docs/grammar.bnf` (`<fnDecl>` comment), `gaps.md` (void-discard footgun → RESOLVED, cross-ref the
  bare-fn entry). NOTE (since 2026-06-21 superseded): string-interpolation operands ARE now checked —
  the `ExprKind::Str` arm parses `{…}` fragments and `infer_value`s each (see the soundness-fix entry
  below), so void-call / nil fragments are nil-banned too. All cargo
  wrapped at MemoryMax=6G; full `cargo test` (2104) + `cargo test conformance` green,
  `cargo clippy --all-targets -- -D warnings` clean.
- **Follow-up fixes (2026-06-17).** Two checker bugs in the inline-expr return path, both fixed:
  (1) an inline-expr body with a declared return type was type-inferred TWICE (statement-walk +
  return-assignability check), doubling every error inside the expr — `fn a() -> int: nope(5)` now
  reports exactly ONE diagnostic. The inline-expr stmt is now inferred once (the statement-walk is
  skipped for it). (2) the return-type assignability check was gated `if ret != Ty::Nil`, so a
  **non-nil** inline expr against an explicit `-> nil` was never validated — `fn a() -> nil: 10`
  type-checked clean but emitted `Return(10)` (a void fn returning an int). It is now rejected with the
  multiline path's wording *"function returns nothing, cannot return a value"*; a nil-typed inline expr
  against `-> nil` (a bare void call) stays legal. Tests: `inline_expr_error_reported_once`,
  `inline_nonnil_expr_against_nil_ret_rejected`.

**✅ Built-in conversions — str ↔ bytes (UTF-8) methods + `List()`/`Set()`/`Map()` constructors
(owner-requested; the natural follow-on to the just-landed `bytes`/`bytearray` types).** Two
conversion surfaces, mirroring the `bytes`/`bytearray` builtin-wiring exactly (3-engine parity), with
**no new syntax** — every form is an existing call/method production, so **`docs/grammar.bnf` is
intentionally UNCHANGED** (`cargo test conformance` stays green, proving no new terminal):

- **str ↔ bytes (UTF-8), as METHODS (not constructors — `bytes(x)`/`str(b)` names are already taken):**
  `str.encode() -> bytes` UTF-8-encodes (always succeeds — `str` is UTF-8 internally; copies the bytes
  out into a new immutable `bytes`). `bytes.decode() -> str` and `bytearray.decode() -> str` UTF-8-decode
  via `std::str::from_utf8`, mapping invalid UTF-8 to a **recoverable** `RuntimeError`
  (`"invalid UTF-8 in decode()"`, catchable by `recover:`, **never** a panic — same fault policy as the
  index/overflow faults). `"héllo".encode().decode() == "héllo"` round-trips a multi-byte char;
  `b"\xff\xfe".decode()` faults recoverably. **UTF-8 only** — no encoding-name argument (latin1/utf16 are
  an explicit future non-goal). Only `str` gets `.encode()`; only `bytes`/`bytearray` get `.decode()`.
  Wired through the method-dispatch path: checker `str_method_sig`/`bytearray_method_sig` + a new
  `bytes_method_sig` and a `Ty::Bytes` arm in `infer_method_call`; VM `core_method` Str arm +
  `bytearray_method` + a new `bytes_method` + an `Obj::Bytes` route in `do_method_call`, both decode
  paths sharing `Vm::decode_utf8`; interp `str_method` + `eval_bytearray_method` + a new
  `eval_bytes_method`, both sharing the free `decode_utf8` (error string byte-identical between engines).
- **`List(it)` / `Set(it)` / `Map(it)` constructors over ANY for-iterable** (NOT the narrow
  `Iterator[T]` protocol). Element types resolve through the checker's **`iter_elem`** — the single
  source of truth for "what `for x in X` accepts" — so `List([1,2])`, `List(myset)`, `List(b"hi")`,
  `List("ab")`, `List(range(3))`, `List(bytearray(..))`, and `List(myUserIterator)` all typecheck with no
  new protocol bound. `List(it) -> List[T]`; `Set(it) -> Set[T]` (the EXISTING `Set` broadened from
  list-only to any for-iterable, keeping the 0-arg empty-set form + the `Hashable` gate); `Map(it) ->
  Map[K, V]` where the element is **exactly a 2-tuple** `(K, V)` (a non-2-tuple is a **static** checker
  error), `K` `Hashable`, last-wins on dup keys (like the `{k: v}` literal). `list`/`map` are NEW reserved
  builtin names (added to `is_reserved_name` + both `is_builtin` sites + per-engine dispatch). The
  argument is **required** — an empty `list`/`map` is the `[]`/`{}` literal, so `List()`/`Map()` are
  checker errors pointing there. `Map(pairs)` (free call) and `xs.map(f)` (list HOF method) are separate
  namespaces — verified the parser routes them distinctly; documented in `docs/syntax.md`.
- **Runtime drain helper (the one genuinely new runtime piece).** Built-in collections copy elements
  directly (list/set elems, str→per-char `str`, bytes/bytearray→per-byte `int`, map→keys, range is
  already a materialized list). A user `next(self) -> Option[T]` struct (or a VM generator) is drained by
  looping its `next()` until `None`. **Interp:** extracted `drain_value_to_rows` from the post-eval body
  of `collect_iter_rows` (the for-loop's own materializer) — no duplicated iteration semantics; `set`
  rerouted through it, `list`/`map` added on `Interp::call`. **VM:** new `Vm::drain_iterable` (no runtime
  for-loop exists — it's fully compiled), driving user `.next()` via `run_proto`/`generator_next` with the
  growing accumulator + source **rooted on the operand stack** across every re-entrant call (GC-safe,
  copying the `builtin_set`/`list_hof`/`struct_hash` rooting pattern); `builtin_set` rerouted through it,
  `builtin_list`/`builtin_map` added to `do_builtin`.
- **Tests/golden:** checker `encode_decode_types` / `encode_only_on_str_decode_only_on_bytes` /
  `constructor_iter_types` / `list_zero_arg_rejected` / `map_requires_two_tuple` /
  `set_map_hashable_key_gate_preserved`; VM/interp parity `encode_decode_roundtrip_multibyte` /
  `bytearray_decode_matches_bytes` / `invalid_utf8_decode_recoverable` /
  `constructors_over_user_iterator_and_dupkey`; and `examples/conversions.chz` + `.expected` goldened on
  **VM + `--serial` + `--interp`** (byte-identical; uses a user `.next()` struct, NOT a generator, so all
  three engines agree). +7 tests (2036 green); `cargo test conformance` green (grammar unchanged); clippy
  clean. **Non-goals (stated):** non-UTF-8 codecs (latin1/utf16), base64/hex/sha (separate `std.*` gap),
  `tuple()` constructor (fixed-arity tuples can't be typed from a runtime-length iterable), `bool()`/
  truthiness (`if` stays strict-bool), and a formal user-visible `Iterable[T]` protocol (decoupled into
  its own future milestone — the constructors reuse the internal `iter_elem` union, not a new bound).

**✅ `bytearray` — mutable byte buffer (owner-requested; the second half of binary support — the
mutable sibling of `bytes`, Python `bytearray` / Go `[]byte` model — still a sequence, NOT a scalar).**
A heap byte buffer modeled on `list` (mutation flows through shared references), constructor-only
(no literal), mirroring the just-landed `bytes` variant-for-variant across the whole pipeline:

- **Constructor-only — no `ba"..."` literal** (the `b"..."` literal already owns `bytes`, so no lexer/
  parser/grammar change; `docs/grammar.bnf` is intentionally unchanged — a `bytearray(...)` call is the
  existing IDENT-LPAREN production). `bytearray` lexes as a plain identifier (guarded test). Four forms:
  `bytearray()` (empty), `bytearray(N)` (N zero bytes, Python; an absurd N faults **recoverably** via
  `try_reserve`, never a SIGABRT — same recoverable-fault invariant as `range()`/format-width), `bytearray(b)`/`bytearray(ba)` (mutable
  copy), `bytearray([ints])` (each 0–255). Both `bytes(...)` and `bytearray(...)` are NEW builtins (the
  `bytes` commit shipped no `bytes(...)` constructor — it was literal-only) — the **conversion bridge**:
  `bytes(ba)` snapshots, `bytearray(b)` copies.
- **Type `bytearray`** (`Ty::ByteArray`): `ba[i]`→`int`, **`ba[i] = x`** (`IndexSet`, M15 — the new
  capability `bytes` lacks; value 0–255 + index in range, else a recoverable fault), `ba[a:b:c]`→a new
  `bytearray`, `for x in ba`→`int`, `len`, `.push(int)` / `.pop()->Option[int]` / `.extend(bytes|
  bytearray|List[int])`, `==`/`!=` structural (incl. cross-type `bytes == bytearray` content-equal,
  Python parity). **NOT `Hashable`** (mutable ⇒ not a `map`/`set` key, the deliberate divergence from
  `bytes`, consistent with `list`). Sendable across the `--parallel` airlock by **deep copy** (like
  `list` — `WireValue::ByteArray` rebuilds a fresh independent buffer; no shared mutable view).
- **Runtime, BOTH engines (three-engine parity).** VM `Obj::ByteArray(Vec<u8>)` mutated IN PLACE
  through the `GcRef` heap slot (`heap.get_mut`), exactly like `Obj::List` — two bindings to the same
  `bytearray` observe each other's writes; interp `Value::ByteArray(Rc<RefCell<Vec<u8>>>)` interior-
  mutable like `Value::List` (deep-cloned ONLY across the airlock — a fresh `Rc<RefCell>`, NOT a cloned
  `Rc` like `Bytes`). Display/`str()`/interp = Python `bytearray(b'...')` repr via the shared helper
  `slice::bytearray_repr` (wraps `bytes_repr`), so all three engines are byte-identical by construction.
- **GC:** `Obj::ByteArray(Vec<u8>)` is a **LEAF** — raw `u8`, holds zero `GcRef`, so `children()` traces
  nothing (the difference vs `bytes` is the mutability of the slot, not GC reachability). `Vec<u8>` is
  24B (= `Obj::List`'s `Vec<Value>`), so the `Obj` size-cap (`size_of::<Obj>() == 88`) is unchanged.
- **Tests/golden:** `bare_bytearray_is_identifier` (lexer), `bytearray_*` (checker — incl. unhashable
  map/set-key rejection + conversion bridge), `vm_bytearray_*` + `bytearray_crosses_channel_deep_copy`
  (VM — incl. index WRITE, OOB/bad-value under `recover:`, shared mutation through two bindings,
  `--parallel` deep-copy independence), `interp_bytearray_*`, `bytearray_repr_wraps_bytes_repr` (slice),
  and `examples/bytearray.chz` + `.expected` goldened on **VM + `--serial` + `--interp` + `--parallel`**
  (byte-identical). +18 tests (2023 green); clippy clean. Remaining non-goals: a `byte`/`u8` scalar,
  non-UTF-8 codecs (latin1/utf16) + base64/hex/sha (a separate `std.*` gap), and byte-sequence methods
  beyond push/pop/extend/`decode` + the protocol ops. (UTF-8 `.decode()` has since **shipped** — see the
  conversions section above.)

**✅ `bytes` — immutable byte-sequence type (owner-requested; the Tier-A pre-JIT `Value`/`Obj`-variant
must-do from `gaps.md`, Python `bytes` model — NOT a new scalar).** A heap byte sequence threaded
through the existing `str`-shaped paths, reusing every protocol mechanism (no new ops/abstractions
beyond a `b"..."` literal + the const op):

- **Literal `b"..."` / `b'...'` (lexer-only, like the radix int literals).** `Token::Bytes(Vec<u8>)`;
  prefix fires ONLY when `b`/`B` is immediately followed by a quote (`b + 1` and `by` stay
  identifiers). Escapes: `\xHH` (exactly two hex digits → one byte 0x00–0xFF, the only way to write a
  byte ≥0x80) + `\n \t \r \\ \" \' \0`. **Rejects** `\u{…}` ("\\u not allowed in a byte literal") and a
  raw non-ASCII source char ("non-ASCII byte in byte literal"). Triple-quoted `b"""…"""` supported.
- **Type `bytes`** (`Ty::Bytes`): literal infers `bytes`; `b[i]`→`int` (Index protocol, M15), `b[a:b:c]`
  →`bytes` (Slice protocol over BYTE offsets, `src/slice.rs`), `for x in b` yields `int`, `len(b)` = byte
  count, `==`/`!=` structural, `Hashable` (valid `map`/`set` key). Immutable — `b[i]=x` is a type error
  (no `IndexSet`). Sendable (crosses the `--parallel` airlock by value, `WireValue::Bytes`).
- **Runtime, BOTH engines (three-engine parity is mandatory — this is a new feature landing on both,
  the sanctioned exception to "don't touch interp").** VM `Obj::Bytes(Box<[u8]>)` + `Op::ConstBytes`;
  interp `Value::Bytes(Rc<[u8]>)`. Index/slice/for/len/eq/ordering/hash/Display all reuse the existing
  dispatch with a Bytes arm next to the Str arm. **Display/`str()`/interp = Python `b'...'` repr** via
  ONE shared helper `slice::bytes_repr(&[u8])` called by both engines (parity by construction).
- **GC:** `Obj::Bytes` is a **LEAF** — it holds only raw `u8` (no `GcRef`), so `Heap::children()`
  returns nothing for it (marked reachable, traces no children, like `Str`/`Native`); the generic
  `alloc` path allocates it and `sweep` frees it via `Box<[u8]>`'s `Drop`. `Box<[u8]>` is 16B, so the
  `Obj` size-cap (`size_of::<Obj>() == 88`, `chzstr.rs` guard) is unchanged.
- **Tests/golden:** `byte_string_*` (lexer), `bytes_*` (checker), `vm_bytes_*` + `bytes_crosses_channel`
  (VM, incl. recover: + map key + `--parallel`), `interp_bytes_*`, `bytes_repr_python_style` (slice),
  and `examples/bytes.chz` + `.expected` goldened on **VM + `--serial` + `--interp`** (byte-identical).
  `docs/grammar.bnf` gained the `BYTES` primary terminal (`cargo test conformance` executes it; corpus
  `bytes_literal.chz`). +16 tests (1984 green); clippy clean.
- **Non-goals (v1):** `byte`/`u8` scalar, bignum, non-UTF-8 codecs (latin1/utf16) + base64/hex/sha
  (a separate `std.*` gap), a `{b:spec}` format-spec, and `ConstBytes` interning (allocs per push, like
  a list literal). (Two items once listed here as non-goals have since **shipped**: the mutable
  `bytearray` — see the `bytearray` section above — and UTF-8 `encode`/`decode` — see the conversions
  section above.)

**✅ Scoped enum variants — qualified-only `Enum.Variant` (owner-requested, explicit exception to the
M19/M18 feature freeze).** User-enum variants are now **scoped under their enum** and must be written
**qualified** (`Color.Red`, `Shape.Circle(2)`, `case Shape.Circle(r):`) in every position — value,
constructor, and `match` arm. A **bare** user-variant name is a hard compile error (the message names
the enum: *"'Red' is a variant of enum 'Color'; write it qualified as 'Color.Red'"*). Crucially, the
bare→binding trap is closed: a bare known-variant in a pattern errors instead of silently becoming a
catch-all binding. Because variants are keyed per-enum (`(enum, variant)`), **two enums may now share
a variant name** (`Color.Red` / `Light.Red` are distinct, with distinct dense `variant_id`s). The
**built-in** `Ok`/`Err`/`Some`/`None` (Result/Option) stay **bare** (they're special-cased, not in the
user registry); a user enum that reuses one of those names must qualify its own (`Signal.Err`), and a
bare `Err`/`Some` is always the built-in. The variant registry was re-keyed to `(enum, variant)` in
all three of checker / compiler / interp; the runtime layout is unchanged (the VM already matched on
the dense int `variant_id`). The interp's `try_bind` gained an enum check so a qualified pattern only
matches a value of that same enum (parity with the VM's int compare). `check_pattern_qualifier` also
rejects a qualifier that names the *wrong* enum (`case Light.Red:` over a `Color` scrutinee) — owning
the variant name isn't enough now that names are shared, else the dead arm would be miscounted toward
exhaustiveness and the real value would trap at runtime (regression test
`foreign_enum_qualifier_in_match_arm_is_rejected`). The parser's `[T](…)` type-arg
steal now also fires after `Enum.Variant`, so `Tree.Node[int](…)` works. **Both engines + parity**
(VM/`--serial`/interp byte-identical) via `examples/enum_qualified.chz`/`enum_layout.chz` + goldens +
`shared_variant_name_dispatches_per_enum`; conformance unchanged (semantics-only) plus a new
`tests/corpus/accept/enum_qualified.chz`.

**✅ M20 — In-language test framework (`assert` + `test fn` + `chezzi test`).** Chezzi now has a real
test facility. Three layers, all TDD'd:

- **`assert <cond>` / `assert <cond>, "<msg>"`** — a statement primitive that *faults with its source
  span* when `cond` is false (the headline need: which line failed). `cond` must be `bool`, `msg`
  (optional) `str` — checker-enforced. **Lands in BOTH engines** (parity discipline): the VM op
  `Op::Assert { has_msg }` and the interp `exec_stmt` arm produce a byte-identical message + span
  (default `"assertion failed"`); `examples/assert.chz` goldens this on both engines. Usable in plain
  `chezzi run`, independent of the runner.
- **`test fn` marker** — a `test` modifier before `fn`. A free `test fn` is an independent test; a
  `test fn name(self)` method makes its struct a **suite**. Compiler-*tagged* (`Proto::is_test`,
  `Program::tests`, `StructDef::test_methods`), so discovery is by tag, not a name scan (no
  silent-typo risk). Checker validates the shape: no params (free) / only `self` (method), returns
  nothing; a suite's name-matched lifecycle hook must be `fn name(self)` returning nothing.
- **`chezzi test [path]`** — a **Rust-side**, VM-only runner (forced: `recover:` only hands Chezzi the
  message, not the span, so only Rust catching `RuntimeError` gets `.span` for `file:line`). Collects
  `*_test.chz` files (single file or recursive dir walk; default cwd), compiles each as its own entry
  graph, runs the module top-level once, then invokes each tagged test on a reusable VM. Reports
  `PASS/FAIL name (file:line) msg` + a summary; non-zero exit on any failure. **Suites**: a synthetic
  `__new_<Suite>` thunk builds the instance once (reusing the struct-ctor compile path + default field
  exprs), then `before_all? → [before_each? → test → after_each?(always, like defer)]* → after_all?`,
  with a shared typed fixture (a default-initialized field mutated by hooks via mutable `self`).

Dogfood: `examples/{membership,operators,match_or,suite}_test.chz` author real tests with `assert`
(alongside the existing print-and-golden twins). Out of scope (deferred): `Span` file-id (an assert
faulting inside *imported* code reports the test file, not the library file — a documented MVP limit),
`assert_eq`/value-diff messages, parametrized-test sugar, a Chezzi-side runner, running the runner on
the interp engine. Grammar (`assertStmt`, `testFnDecl`) + corpus + `cargo test conformance` green.

**✅ M21 — Nominal `newtype`.** `newtype Name = <type>` (a new keyword, distinct from the transparent
`type` alias) is a DISTINCT nominal type wrapping the underlying — Go's defined-type model. It does
NOT silently mix with the raw underlying: a bare `int` is not assignable to a `UserId`, and a `UserId`
is not an `int`; only an explicit **construct** (`UserId(10)`, a call with one underlying-typed arg) or
**cast-unwrap** via the existing scalar casts (`int(uid)`/`float(m)`, and `str(n)` for a str-underlying)
crosses the boundary — no `.value`, no auto-deref. For a **scalar** underlying, same-newtype operators
**auto-flow** to the underlying's *native* op (unwrap→primitive-op→rewrap, NOT a user `add`):
`Meters + Meters -> Meters`, `Meters < Meters -> bool`, `==` compares inner; `Meters + float` /
`Meters + Seconds` are rejected (the whole point). A newtype carries its own (non-generic) methods and
satisfies protocols via them — `str(self)` (Stringable override), `hash(self)` (map/set key — opt-in,
*not* inherited), `compare`/`add` — and a numeric newtype satisfies `Add`/`Sub`/`Mul`/`Comparable`
intrinsically, so it flows into `fn twice[T: Add]`. Implemented by treating a newtype as ~a 1-field
nominal struct and reusing the struct/enum machinery at every layer: `Ty::NewType(key)` (checker),
`Obj::NewType{type_key,inner}` (VM) / `Value::NewType{type_key,inner}` (interp), `program.newtype_methods`
+ `newtype_home`, with `hash`/`str` dispatched **at runtime in both engines** (like the enum-hash fix)
and the wire/snap/airlock paths covered so a newtype is sendable iff its inner is. **Both engines +
parity** (VM/`--serial`/interp byte-identical) via `examples/newtype.chz` + `newtype.expected` golden;
new grammar `<newtypeDecl>` + `tests/corpus/accept/newtype.chz` + `cargo test conformance` green; clippy
clean; ~2347 tests pass. **v1 limits (documented):** an aggregate underlying (`newtype Names =
List[str]`) gets identity+construct+unwrap+own-methods ONLY — no `.push`/index/iterate forwarding;
no `derive`. Docs: `syntax.md §7`, `spec.md` (M21 row + enum-methods note de-staled), `grammar.bnf`.

**✅ M21+ — Generic newtypes (`newtype Stack[T] = List[T]`).** Type parameters on a `newtype`, the Go
defined-type model extended to generics — reuses the struct/enum generic plumbing end-to-end:
`type_params` on `StmtKind::NewType` (`parse_type_params`, the v1 hard-reject removed), a
`newtype_type_params` map mirroring `enum_type_params`, and `Ty::NewType(key, Vec<Ty>)` carrying the
instantiated args like `Ty::Enum`. The underlying + method signatures resolve `T` (hoist/body passes
`enter_type_params`); method dispatch substitutes the value's type args into the sig (`Stack[int].top()`
⇒ `Option[int]`); ctor infers args by unifying the underlying against the arg (`Stack([1,2])` ⇒
`Stack[int]`) with **turbofish** for the inference gap (`Stack[int]([])` — the empty `[]` can't bind
`T`, the documented `ConcurrentMap(RwShared({}))` case). **Methods-only:** a type-parameterized newtype
gets **no native operator auto-flow** — even `newtype Box[T] = T` over a numeric `T` — gated at every
auto-flow site (`Div`/`Mod`, `op_overload_result`, `ordering_allowed`, the `satisfies` intrinsic arm)
by a new `newtype_is_generic`; scalar `UserId=int`/`Meters=float` auto-flow is unchanged. **Cast-unwrap
propagates the instantiation** (the one genuinely new bit): `List(s)` for `s: Stack[int]` ⇒ `List[int]`
(via `newtype_unwrap_target` + a runtime peel in `builtin_list`/`set`/`map`, both engines — a
map-over-map yields the inner map directly). Runtime is **type-erased** (`Obj::NewType`/`Value::NewType`
carry no args), so generic instantiation / dispatch / hash / str are byte-identical across interp,
cooperative VM, and `--parallel` — golden `examples/newtype_generic.chz` + `.expected` is a standard
two-engine + `--parallel` test, no escape hatch. Cross-module via `NewTypeSigInfo.type_params`. Out of
scope (follow-up): static / associated methods (`Type.method()` / `T.zero()`). Docs: `syntax.md §7b`
(out-of-scope claim lifted → methods-only + turbofish), `spec.md` M21 row, `grammar.bnf` `<newtypeDecl>`.

**✅ Turbofish at the declaration site — type-side (PART 1).** Explicit type args for a generic are
pinned **at the site the generic is DECLARED**: declared on the type (`enum/struct/newtype [T]`) →
pinned **on the type** (`Box[int]`); declared on a member (`fn m[U]`) → on the member. For a generic
TYPE the args go ON THE TYPE, uniformly for enum **variant constructors** and **static methods**:
`Box[int].Has(5)`, `Result[int, str].Ok(5)`, nullary value `Box[int].Empty`, generic static
`Box[int].empty()`. Multi-param types use the comma form (`Result[int, str].Ok`). The OLD **gliding**
form `Enum.Variant[T](args)` (type args on the variant) is **removed** — the checker emits a redirect
(`put the type arguments on the type: Box[int].Full(...)`); the bare/module-qualified variant branches
both guard it. **Parser:** the SINGLE-arg head (`Box[int].member`) stays on the index path (the parser
can't tell it from `arr[i].field`), reinterpreted by the checker; the MULTI-arg head commits a new
`ExprKind::TypeApply{name, args: Vec<Type>}` carrier (the disambiguating comma — a comma in a subscript
is otherwise always a parse error, so it steals nothing) parsed via `try_parse_type_apply`. **Checker:**
one `type_apply_head` helper resolves both carriers to `(type-name, [Type])`; in `infer_call` it is
**variant-first** (`infer_variant_call` with the resolved targs seeded — arity-checked by
`seed_targs`), else `infer_static_call`; `infer_field` gains the nullary-value branch returning the
**resolved** type args (not `Unknown`). The single-`Index` path also gained the variant-first check
(a gap the previous static-methods work left). **Compiler + interp** get matching `type_apply_head_name`
branches emitting the same `Op::NewEnum`/`Op::CallStatic` as the bare forms (runtime is type-erased).
**PART 2 (now landed, below).** **Both engines + `--parallel`** byte-identical via golden
`examples/turbofish_type_args.chz` + `.expected` (the test also asserts the program type-checks clean);
checker unit tests for each rule (single/multi-arg variant, seeded-not-Unknown, arity mismatch, nullary,
old-form redirect, static regression); a parser unit test; a `tests/corpus/accept` file for the
differential conformance check; clippy clean. Migrated the one surface use `examples/explicit_type_args.chz`
(`Box.Full[int](9)` → `Box[int].Full(9)`). Docs: `syntax.md` (§7a generic-static + enum/variant
sections — the declaration-site rule; multi-arg lifted), `spec.md` (new milestone note + static-method
single-arg limit de-staled), `grammar.bnf` (the `<typeApply>` head + `Type[T…].member` postfix
productions; old gliding production removed from prose).

**✅ Turbofish at the declaration site — member-side (PART 2).** Completes the declaration-site rule: a
**member** declares its OWN type args (`fn make[U]`, `fn first[A, B](self, …)`), pinned on the member
and composing with PART 1's type-side args. `Box[int].make[str](x)` supplies the enclosing `T` AND the
method `U`; `Box.make[str]("hi")` / `s.first[int, str](1, "x")` are bare carriers; inference is the
default (`Box[int].make(5)` ⇒ `U = int`). **Checker:** `infer_static_call` gained an `mtargs` arg and now
builds ONE by-name substitution map over BOTH the enclosing type params (seeded from the type turbofish)
and the method's own `[U]` (seeded from `mtargs`), inferring the rest from the args and degrading EVERY
un-inferred param — enclosing or method — to `Ty::Unknown` (no leaked `Ty::Param`; mirrors the static
fix at 7c75ab2). **UPDATE — parser steal BROADENED (uniform-receiver rule).** The member-turbofish steal
now fires on **ANY** `Field` receiver, not just a `Field` over a bare ident: `recv.name[X](args)` parses
as a method turbofish on a bare ident, a call result (`W(1).cast[str]("a")`), a field (`h.w.cast[U](x)`),
or an index (`xs[0].cast[U](x)`). `try_parse_type_arg_call` stays speculative (commits only on the
`[ <typeList> ] (` shape, else restores pos+depth), so `obj.items[0]`/`m.data[k]` (no call) and the
numeric `arr[0].handlers[0](20)` still backtrack to index-then-call. The combined `Box[int].make[str](x)`
now also rides the Field-callee path (the receiver `Box[int]` is itself a postfix) and is dispatched by
the `type_apply_head` branch — threading **both** the enclosing type args (`[int]`) and the method targ
(`[str]`, was dropped as `&[]`, now `&targs`); a method turbofish on a generic **variant** ctor
(`Box[int].Has[str](5)`) is now explicitly an error (the old Index-over-Field block that caught it is
bypassed). **AUTHORIZED REGRESSION (accepted, documented):** index-then-call of a fn-**valued** field on
a non-bare receiver — `arr[i].handlers[k](10)` — now parses as a turbofish and errors; workaround is
parens `(arr[i].handlers[k])(10)`. This makes non-bare receivers UNIFORM with the bare-ident case
`w.handlers[k](10)`, which already required parens. `infer_method_call` gained a `type_args` arg threaded into `infer_generic_method`
(instance multi-turbofish `s.m[A, B](x, y)` now seeds + arity-checks + catches an explicit-targ/arg
conflict, previously silently dropped) plus a top-of-fn guard — BEFORE the `.iter` fast-path — rejecting a
member-level turbofish on a builtin/non-generic member (fixes the `.iter[int]()` swallow; `len[int]()`
already errored). The `fn_sig` shadow guard already fires for static methods. **Compiler + interp** get
matching combined-Index-callee arms (peel the erased index → same `Op::NewEnum`/`Op::CallStatic` /
`build_variant`/`call` as the bare forms; runtime is type-erased). **OUT OF SCOPE (unchanged):** static
methods on `newtype`; associated protocol requirements (`T.zero()`) — **SHELVED** after two rejected
attempts, see `docs/future.md` §3.13; protocols stay instance-only.
**Both engines + `--parallel`** byte-identical via golden `examples/turbofish_member_args.chz` +
`.expected` (asserts type-checks clean too) incl. the regression-guard shape; new checker unit tests
(static own-`[U]` inferred, no-leak degrade, combined ok + mismatch, shadow-static rejected,
`iter[int]()` errors, instance multi-turbofish ok + mismatch, index-then-call regression);
`cargo test conformance` re-run after generalizing the `grammar.bnf` method-turbofish production to
`<typeList>`/`<argList>`; clippy clean. Docs: `syntax.md` §7a (member-level + combined + by-name unified
substitution; removed the "cannot declare its own `[U]`" / "method-level turbofish reserved" notes),
`spec.md` (PART 2 milestone note; lifted the static-own-`[U]` limit), `grammar.bnf` (generalized
production + combined-form checker-reinterpreted comment).
**KNOWN FOLLOW-UP (deferred, doc-only — revisit later):** the authorized-regression error for
`recv.name[k](args)` where `name` is a fn-valued field / not a generic method is currently the bare
`method '…' takes no type argument(s)`. Upgrade it to a *guiding* diagnostic that detects the
fn-field/non-generic-member case and suggests the parens workaround `(recv.name[k])(args)` so users
hit by the uniform-steal rule are pointed at the fix without reading the spec caveat. Checker-side,
low risk, no parser change; pairs with a regression test on the parenthesized form.

**✅ Static (associated) methods on struct + enum — the "no self ⇒ static" rule.** A struct/enum
method whose first parameter is **not** `self` (or which has no parameters) is a **static** method,
called `Type.method(args)` instead of `value.method(args)` (the Rust `fn new` ergonomic). **Additive**
— the positional `Name(...)` ctor is unchanged; static methods unlock named/alternative ctors
(`Rect.square(5)`) and validating ctors returning `Result`/`Option` (`Email.parse(s) ->
Result[Email, str]`, `Color.from_str(s) -> Option[Color]`). Instance vs static are **different call
shapes** — neither is invocable as the other (clear errors pointing at the right form). **Note — a
behavior change:** a method like `fn getx(p: Point)` (first param not `self`) is now STATIC, not an
instance method with a positionally-bound receiver (the old "receiver is positional, any name"
convention is gone). Classification is a pure decision over the existing AST (`first param != "self"`)
threaded through all three engines: a new `FnSig.is_static` (checker), a `Compiler.static_methods`
set populated in `hoist_types`, and `is_static_method()` in interp — so the engines agree by
construction. **Resolution** mirrors the existing `Enum.Variant(args)` qualified-ctor branch in
`infer_call`/`compile_call`/`eval_call`: a new static-method branch alongside the variant check (for
enums the **variant wins first**; variant/static names must be **disjoint**, a new decl-time check).
New `Op::CallStatic{type_key, method, argc}` (separate variant, mirrored in interp) executes like the
enum-method slow path **minus the receiver** (`do_static_call`, `arity == argc`, `push_frame_in_place`,
generator edge via `alloc_generator`). **Generic statics** via the **type-level turbofish**
`Box[int].empty()` (reinterprets `Field{obj: Index{Ident, idx}, name}` — indexing a bare type is
otherwise invalid, so unambiguous). (Multi type-arg + variant-side resolution were generalized by the
later "Turbofish at the declaration site — type-side" milestone above; a static method declaring its
own `[U]` + the member-level turbofish landed in the "member-side (PART 2)" milestone above.) v1 limits
(documented): static methods do **not** participate in
**protocol** conformance (instance-only); static methods on `newtype` are a follow-up (the newtype
receiver-error site stays). **Both engines + `--parallel`** byte-identical via golden
`examples/static_methods.chz` + `.expected` (mirrors `newtype.chz`); checker unit tests for each rule
+ the negative cases; clippy clean. Docs: `syntax.md §7a`, `spec.md` (M21 newtype-static note
de-staled + a new "Static methods" milestone note), `grammar.bnf` (`Type.method` / `Type[t].method`
postfix forms documented — no new production).

**✅ Raw string literals — `r"…"` / `r'…'` / triple `r"""…"""` (and uppercase `R`).** A verbatim `str`:
**NO interpolation** (braces `{`/`}` are literal — `r"{}"` prints `{}`, no `{{}}` doubling) and **NO
escape processing** (`r"\d+"` is literal backslashes — best for regex / Windows paths / brace-heavy
JSON). The escape hatch for the always-on `{…}` interpolation. Type is plain `str` (`Ty::Str`),
identical downstream. Lexer-only: a new `Token::RawStr` → distinct `ExprKind::RawStr` (mirrors
`Bytes` across all 9 touch-sites) so Rust's exhaustiveness checker FORCES both engines to handle it —
the VM emits `Op::ConstStr` directly and interp returns `Value::Str` directly, **both bypassing
interpolation**, so VM/interp/`--serial` are byte-identical by construction. The `r`/`R` prefix fires
only when immediately followed by a quote (adjacency rule — a variable named `r` is unaffected,
exactly like `b`). Short form can't contain its own quote; triple form embeds quotes (JSON).
**Two-engine parity** golden `examples/raw_string.chz` + `.expected`; `tests/corpus/accept/raw_string_literal.chz`
+ new `RAWSTR` terminal in `grammar.bnf <primary>`, `cargo test conformance` green; clippy clean.
**Out of scope (follow-ups):** combined raw-bytes `rb"…"`/`br"…"`, Rust-style `r#"…"#` hash delimiters
(the triple form already embeds quotes). Docs: `syntax.md §2/§10`, `spec.md`, `grammar.bnf`.

**🟦 M19 — Perf track (in progress).** M19 is a **pre-JIT perf push**, not a feature freeze — language
work still lands (e.g. module-scoped types, 2026-06). This milestone is otherwise pure
optimization, so the bar is **behavior-preserving + two-engine parity** on every change. Measure first
(`cargo run --release -- run benches/run.chz`), land behind a failing-then-green correctness test, keep
parity green, re-measure, record the delta in [`docs/benchmarks.md`](docs/benchmarks.md). Several levers
moved a *different* bench than predicted — trust the measurement, not the a-priori guess. The frozen
interp is untouched by VM-only work, so parity is automatic for those changes.

**Slice syntax → Python colon (owner-requested language change, mid-M19).** The subscript-slice form
moved from Rust-range `xs[a..b]` to Python `xs[a:b]` with the full surface: open bounds (`xs[1:]`,
`xs[:3]`, `xs[:]`), step (`xs[a:b:c]`), reverse (`xs[::-1]`), and **negative indexing** (`xs[-1]`,
`xs[-2:]`) on plain index AND slice bounds, for `list`/`str` and as an assignment target (`xs[-1] = v`).
Out-of-range rule = Python's asymmetry: a plain `xs[-100]` **faults** (`index -100 out of bounds (len N)`),
a slice bound `xs[-100:]` **clamps**. The `..` operator is unchanged — it stays the for-loop / match-pattern
range. The parser owns the colon (`parser::parse_subscript`, replacing the old post-hoc Range→Slice rewrite);
`ExprKind::Slice` now carries `start/end/step: Option<Box<Expr>>`. Runtime is a single shared resolver
(`src/slice.rs`: `slice_indices` + `norm_index`, derived from CPython `slice.indices`) called byte-identically
by both engines — it replaced the duplicated `clamp_range`. User `Slice` structs get the full surface via
default params: `slice(self, start: int?=None, end: int?=None, step: int?=None) -> R` (the runtime passes
real `Option[int]` components). Strict TDD, both-engine parity green, `examples/slicing.chz` +
`examples/edge_cases.chz` + `std/str.chz` migrated, `docs/grammar.bnf` colon-slice rule + `cargo test
conformance` green.

**Landed phases** (all TDD'd, two-engine-parity-clean; numbers + per-lever notes in
[`docs/benchmarks.md`](docs/benchmarks.md), ranked backlog in [`docs/future.md §4`](docs/future.md)):

- **Phase 1** — killed the per-call `Obj` clone in `invoke_value`; jump-relocating peephole + constant
  fold (`src/compiler/peephole.rs`, replicating the VM's checked overflow/div-by-zero semantics);
  superinstructions (`Op::BinLocalLocal`/`BinLocalConst`/`IncLocal`) fusing the hot local/const arith
  windows with an exact unfused fallback.
- **Phase 2** — in-place call args (`do_call` runs over the args already on the stack, killing the
  per-call `split_off` `Vec`); `stringify`-into-buffer (`BuildStr` reuses one buffer across interpolation
  parts).
- **Phase 2b** — global-slotting: every module global gets a stable `u32` slot; `GetGlobalSlot`/
  `SetGlobalSlot`/`DefineGlobalSlot` index `Obj::Module.slots` with no hash. Slot map lives in the shared
  `Arc<Program>` so parent and faulted-worker agree by construction (removes a latent snapshot
  ordering fragility).
- **Phase 3** — `ConstStr` interning (per-heap cache keyed by the literal's data pointer, GC-rooted,
  swapped with the heap across `swap_ctx`); per-char single-alloc `alloc_char` at every 1-char-string
  site.
- **Phase 4** — struct-field inline cache: `GetField`/`SetField` carry a per-call-site IC id into a
  per-`Vm` `field_ic` caching the field index. Runtime IC (the compiler is type-erased); holds an index
  not a `GcRef`, so it's invisible to GC/snapshots/`swap_ctx` and every access self-verifies.
- **Phase 5a** — FxHash (`src/vm/fxhash.rs`, no new dep) for `MapData`/`SetData` index + `str_intern`.
  `values_equal` confirms every hit ⇒ behavior-preserving. (Footgun caught by measuring: a naive
  multiply-only FxHash was 100× slower on int keys — fixed with a splitmix64 finalizer.)
- **Phase 5b** — struct type-id guard (`Obj::Struct.tid`, dense layout id): the field-IC hit guards on
  `cell.tid == obj.tid` instead of a string re-verify. Measured **neutral**, kept as the principled
  guard. The field-IC lever is now spent.
- **Call-loop flattening** — the bytecode `Op::Call` fast path now pushes the callee frame and lets the
  running `run_until` loop execute it (CPython-3.11 "zero-cost frames"), removing the per-call Rust
  `run_until` recursion **and** the per-call `Arc::clone(&self.program)`. HOFs / struct methods keep the
  re-entrant `run_proto` (they need the callee result synchronously mid-Rust-method). **Robustness bonus:**
  deep *plain* recursion no longer consumes host stack — bounded by `MAX_CALL_DEPTH`, not the thread
  stack. (Follow-up: flatten `do_method_call` for the `struct`/method benches.)
- **Small-string optimization (SSO)** — `Obj::Str` holds a `ChzStr` (`src/vm/chzstr.rs`): ≤22 UTF-8
  bytes live inline in the variant, longer spill to `Box<str>`. `Deref<str>` + `From` impls kept the
  ~100 match arms unchanged; `Clone`/`Eq`/`Hash` delegate to `as_str()` so map keys / interning / `==`
  stay byte-identical. `size_of::<Obj>()` unchanged at 88 B (guard-tested). Closes the SSO lever.
- **Phase 6 — method-call IC + flatten `do_method_call`** — `Op::CallMethod` carries a per-site `ic`;
  a struct receiver caches `(tid → proto, module_idx)` in a per-`Vm` `method_ic` vec (a hit skips the
  `program.structs` clone + the name-keyed `def.methods` probe), AND flattens the call (frame pushed in
  place; the running `run_until` executes it, no re-entrant `run_proto`). No `GcRef` in the cell ⇒
  swap/GC-invisible like the field IC; `NO_IC` re-entry callers (`spawn`/`defer` method) keep `run_proto`.
  **`struct` 2.90×→2.63× (−9%)**, the predicted bench; only it moved (it's the OO-dispatch bench).
- **Phase 7 — inline hot ops in `run_until`** — the dispatch loop handles the hottest opcodes inline
  (`GetLocal`/`SetLocal`, the superinstrs, `Jump`/`JumpIfFalse`, `Call`/`Return`) and delegates the tail
  to `step`, skipping a fn-call + the big match jump-table per op. Inlined arms reuse `step`'s helpers /
  copy its 1–3-line bodies (one source of truth). **Biggest lever of the session — moved every op-bound
  bench: `loop` 1.30×→~1.10× (−15%, was the dispatch floor), `list` 3.06×→~2.55× (−17%), `primes` −8%,
  `fib` −6%, `struct`/`str`/`map` −4–5%.**
- **Phase 8 — call-site spec for `Op::Call` — analyzed, DEFERRED (no-gain).** After Phase 7 inline,
  `do_call`'s happy path is already lean (the deref a call-IC skips is ~2–3 instrs); fib's residual is
  frame-setup in `finish_frame`, which a dispatch cache doesn't touch. A correct call-IC also can't avoid
  a heap-specific callee handle ⇒ `swap_ctx` hazard for ~0 gain. fib's real lever is Tier 2 (PEP 659) /
  Tier 3 (JIT). Full rationale in [`docs/benchmarks.md`](docs/benchmarks.md).
- **Memory layout #3 — positional closure captures.** `Obj::Closure.captured` moved from a per-closure
  `HashMap<String, Value>` to a positional `Vec<Value>` indexed by a compile-time slot; `Op::GetCaptured`
  carries a `u32` slot (hash-free `captured[slot]` hot read, no string hash) instead of a name; capture
  names live in `Proto.capture_names` (cold path only: the home-global fallback, error messages, and
  wire/snap name carrying). Nested captures (a closure capturing an enclosing closure's capture) map by
  `CapSrc::Captured(parent_slot)` stamped at compile time. Behavior-preserving + **three-engine parity**
  (`examples/closure_capture.chz` on VM/interp/--parallel). **−45% (1.83×)** on a closure
  construct+capture-read micro (`benches/chz/closure.chz`); standard suite neutral (no closure-heavy
  bench). `Obj::Closure` shrank 88→64 B (Module still caps `Obj` at 88 B, guard intact). JIT groundwork:
  constant capture offsets for the future Cranelift codegen. (Memory layout land order **#1 ✅ → #3 ✅ →
  #2 ✅**; see `docs/future.md` §4.)
- **Memory layout #2 — enum `variant_id` (completes the #1→#3→#2 sequence).** `Obj::Enum` dropped its two
  per-instance `Box<str>` (the type name + variant name, both program-global) for a single dense
  `variant_id: u32` — the enum analogue of struct `tid`. Match-arm dispatch, `==`, and `?` are now
  pure-int compares (was variant-name string compares / `ty==ty && variant==variant`); the type + variant
  names resolve from a new `Program::variants_by_id` table on the cold path only (Display/stringify/
  error/wire/snap). Native `Ok`/`Err`/`Some`/`None` hold the **reserved** fixed ids
  `VID_OK`(0)/`VID_ERR`(1)/`VID_SOME`(2)/`VID_NONE_VARIANT`(3); user variants follow at `4..`, so the
  reserved range is **disjoint** from every user id. `?`/top-level-error gate on the constants, and the
  native construction path (`alloc_enum`) stamps the constant **directly** (never a `variants[name]`
  lookup) — so a user enum may shadow a native name (`enum Foo: Some(int)`, allowed) without a genuine
  native Option/Result being stamped with the user's id. `Op::NewEnum`/`Op::MatchArm` carry the
  compile-time id; wire/snap carry the dense `variant_id` **directly** (shared `Arc<Program>` ⇒ meaningful
  both sides; preserves identity under shadowing). *(Parity bug fixed 2026-06-16: the first cut
  name-resolved native construction, so a user enum shadowing `Some`/`Ok`/… collapsed native-vs-user `==`
  and broke `?` — a VM-vs-interp divergence. Now guarded by two shadow regression tests + a shadowing
  section in the golden example.)*
  Behavior-preserving + **three-engine parity** (`examples/enum_layout.chz` on VM/interp/--parallel).
  **−20% (1.25×)** on an enum construct+match-dispatch micro (`benches/chz/enum.chz`); standard suite
  neutral. `Obj::Enum` shrank 56→32 B (Module still caps `Obj` at 88 B, guard intact). JIT groundwork:
  numeric variant id → constant/jump-table dispatch for the future Cranelift codegen + match-on-enum.

**Remaining / blocked levers:**

- **NaN-boxing `Value` is BLOCKED by full 64-bit ints, not "next."** `Value::Int` is a full `i64`; an
  i64 + a type tag don't fit in 8 bytes alongside `f64`, so it needs boxed big ints (branch + alloc per
  int, semantics-sensitive overflow) — not behavior-preserving, uncertain win on the very int benches it
  targets (Lua 5.4 stayed 16-byte for this exact reason). Blast radius is VM-only (the frozen interp has
  its own `Rc`-based `Value`), but it's a milestone spike. Parked.
- **String concat/split builder/rope** moves no current bench — `join` already buffers into one `String`;
  `+`/`split` aren't exercised by the `str` bench.
- **Arith specialization + frame pooling: effectively closed** — superinstructions inline the monomorphic
  int path; `CallFrame`'s `Vec`s are alloc-free (no per-call frame alloc to pool).
- **Big/separate milestones** (later-stage, once the language has matured): NaN-boxing as its own
  milestone, register VM, generational/incremental GC, and **Cranelift AOT/JIT as the stretch end-game**.

Gap to CPython after Phases 6–7 **~1.1×–3.2×** slower (worst still call-bound `fib` ~3.2×, then `map`/
`struct`/`list`/`primes` ~2.3–2.7×, `str` ~2.0×; **`loop` ~1.1×** — near parity, was the dispatch
floor), startup ~11× **faster**. **1607 tests** green, conformance 7/7, `clippy --all-targets` clean.

**Tier-2 index specialization landed (2026-06-12):** Int-key fast path in `get_index`/`set_index`
(skips `hash_key_rooted`'s rooting — alloc-free for an int key) + inline `GetIndex`/`SetIndex` in the
`run_until` hot arm. **`list` −4%** (its `for x in xs` lowers to per-element `GetIndex`); **`map`
neutral** (FxHashMap-probe-bound, not rooting/dispatch-bound — the predicted target didn't move, the
recurring "measure, don't guess" lesson). Behavior-preserving (7 `idxspec_*` VM==interp guards, incl.
the Int/Float key-collision trap). Moving `map` needs a denser int-keyed map, not this in-place tweak.
See `docs/benchmarks.md` "M19 Tier-2".

**Denser int-keyed map/set index landed (2026-06-13):** the map index was
`FxHashMap<u64, Vec<usize>>`, paying a tiny `Vec<usize>` heap alloc per distinct key (200k of them in
`benches/chz/map.chz`) + a pointer-chase per lookup — yet numeric keys hash injectively, so every
candidate list is length 1. Collapsed the per-key `Vec` to an inline single position via
`enum Pos { One(usize), Many(Box<Vec<usize>>) }`, extracting the (formerly duplicated) `MapData`/`SetData`
index logic into one shared `HashIndex(FxHashMap<u64, Pos>)` in `src/vm/heap.rs`. `One` is zero-alloc/inline;
`Many` (real hash collisions only) is `Box`ed to keep `Pos` 2 words so struct sizes are unchanged.
`candidates`/`push` signatures are identical → **VM hot paths in `mod.rs` unchanged, parity by construction**
(interp keeps its `Vec<usize>` oracle; both confirm hits with `values_equal`). **`map` 2.68× → 1.94×
CPython (−26%, remeasured on merged HEAD `2a934a8`; the dev-base figure was ~1.7×/−36% — variance +
heavier base, see `docs/benchmarks.md` merge-remeasure note)** — the predicted target landed. Others flat (touch no
map/set). 2 new collision-upgrade guards (RED on a `One`-only stub, GREEN with `Many`), 1712 green,
conformance green, clippy clean. **Next `map` suspect:** `values_equal` per-probe cost + `FxHashMap`
lookup/rehash (no longer the `Vec` alloc). See `docs/benchmarks.md` "M19 — denser int-keyed map/set".

**Positional struct layout landed (memory-layout lever #1, 2026-06-16):** `Obj::Struct` instance
fields went from `Vec<(Box<str>, Value)>` to a flat positional `Vec<Value>` (hidden-class / `__slots__`
layout, `src/vm/heap.rs`). Field names now live only in `StructDef`; the runtime resolves them on the
**cold path** (Display/stringify/probe-miss/wire/snap) via `name`→`StructDef`, while the hot field
read/write (IC-guarded on `tid`) is a pure `fields[idx]`. This kills the **N per-field `Box<str>`
allocations per struct instantiation** + the per-field name-clone on `==` (now a by-position value
compare). The synthetic native structs `Match`/`Response` are registered in `Program.structs`
(`src/compiler/mod.rs`) so the runtime can recover their declaration-order names. The interp (frozen
oracle) keeps `Vec<(String, Value)>` per instance — **untouched**; both engines iterate fields in
declaration order, so Display/`==`/interpolation stay byte-identical (two-engine parity by
construction). **Bench-neutral** (the suite is dispatch/alloc-bound and the `struct` bench reuses
instances — predicted in `gaps.md`), but a 4-field struct-construction micro went **827 ms → 510 ms
(−38%)**; primary value is the alloc reduction + **JIT groundwork** (positional storage → constant
field offsets Cranelift codegen needs). 1968 green (+2: positional-layout type guard +
`struct_layout.chz` two-engine golden), conformance 7/7, clippy clean. See `docs/benchmarks.md` "M19
memory-layout lever #1" + `docs/future.md §4`. **Land order #1 ✅ → #3 (closure captures) ✅ → #2 (enum
variant id) ✅ — sequence complete.**

**▶ Next perf batch (Tier 1 DONE — Phases 6+7 landed, 8 deferred; Tier 2 is next; full detail +
`file:line`s in [`docs/future.md §4` "Post-M19 next levers"](docs/future.md)).** Diagnosis: the
remaining gap is **call frame-setup + the alloc/hash paths**, not per-op dispatch (Phase 7 took `loop`
to ~1.1×). Target is CPython 3.14 (specializing interpreter + optional JIT).
- **Tier 1 (cheap→medium):** ✅ 1. method-call IC + flatten `do_method_call` (Phase 6, `struct` −9%).
  ✅ 2. trim per-op overhead in `run_until` — landed as **inline hot ops** (Phase 7; every op-bound bench
  faster, `loop`/`list` −15/−17%). The other two sub-levers (lazy `span`, serial/MN loop split) were left
  unshipped — predictably-false cheap branches, low expected payoff vs the inline win; revisit only if a
  profile shows them. ⏸️ 3. call-site specialization for `Op::Call` — **deferred (no-gain after inline);**
  see the Phase 8 bullet above + `docs/benchmarks.md`.
- **Tier 2 (structural):** ✅ 4. **adaptive opcode quickening (PEP 659) — v1 binops LANDED (2026-06-13):**
  the un-fused generic binop arms (`Add..GtEq` reached by stack operands; `Eq`/`NotEq`, never fused)
  specialize to an int/int fast path behind a per-`Vm`, per-site `(proto,ip)` deopt guard. Side table
  (`quicken: Vec<u8>` + `quicken_base` prefix-sum) mirrors `field_ic`/`method_ic` — heap-independent, not
  swapped, **no `Op`/compiler/interpreter change → parity by construction**. Measured: **`primes` −7–8%**
  (its never-fused `% … == 0` int `Eq` left `values_equal_guarded`), `fib` marginal, others flat (fused /
  alloc / hash-bound — as scoped). Gotcha pinned by test: the int `Eq` fast path **replicates the generic
  lossy `as_f64==as_f64`** (so `2^53 == 2^53+1` stays true), not exact `x==y`, to keep parity. 6 new guards,
  1613 green, clippy clean. See `docs/benchmarks.md` "M19 Tier-2 … quickening, v1". ✅ **CallMethod
  adaptive LANDED (2026-06-13): `poly_method` −33% (6.0× → 4.28× CPython)** — the method-call IC's
  single `MethodIcCell` is widened to an N-way (4-way) `MethodIcSite` with the binop quickening's
  one-way sticky-deopt: a bounded-megamorphic site (≤4 receiver types) HITS a way per type and flattens
  instead of refill-thrashing through a per-miss `StructDef` clone; a 5th distinct type latches `sticky`
  and goes slow (clone-free: borrows `Arc<Program>.structs` instead of cloning the `StructDef`). Side
  table still int-only (tids/proto/module-idx), no `GcRef` — heap-independent, parity by construction
  (interp has no IC). New `poly_method` bench + 5 guards + golden `examples/poly_method.chz`; 1838 green.
  This *unifies* the field/method caches under one adaptive form (`GetIndex`/`SetIndex` already got their
  Int-key fast path in #5 below, so they are covered). ✅ 5. **map/list index specialization** (`mod.rs`
  `GetIndex`/`SetIndex`) — **landed (Int-key fast path + inline dispatch): `list` −4%, `map` neutral**
  (hash-probe-bound). The remaining `map` win shipped as its own lever — ✅ **denser int-keyed map/set
  index LANDED (2026-06-13): `map` 2.68× → 1.94× CPython (−26% on merged HEAD)** — `Vec<usize>` candidate list → inline
  `Pos::One` / `Pos::Many` overflow in a shared `HashIndex` (`src/vm/heap.rs`). See the landed note above.
- **Tier 3 (big, separate):** 6. **Cranelift method-JIT** (end-game; the only path to match/beat fib;
  #4 is the stepping stone). 7. NaN-boxing (BLOCKED, above). 8. register VM / generational GC (low ROI).

### Robustness pass (landed, both engines)
- **Bounded infinite-recursion stack trace (gap #8, 2026-06-23).** At `MAX_CALL_DEPTH` (10_000) a
  recursion fault used to print one `  at <fn> (called at …)` line per frame → ~10_001 lines flooding
  the terminal. `format_trace` (rendered byte-identically in `vm/mod.rs` + `interp/mod.rs`) now (1)
  collapses runs of consecutive same-name frames to the innermost `at` line + `  … (× N more identical
  frames) …`, and (2) caps the collapsed list to head `TRACE_HEAD=10` / tail `TRACE_TAIL=10` with a
  `  … (M frames elided) …` marker. A recursion fault now prints ~4 lines; the captured `Vec<TraceFrame>`
  is untouched (debuggers/tests still see every frame). No-op for small distinct-name traces, so the
  exact-trace golden (`examples/stack_trace.chz`) is unchanged. Parity-tested both engines.
- **Cyclic-data depth guard + order-independent map `==`.** Two fuzzing-found bugs: a cyclic struct made
  `print`/`==` recurse unbounded on the host stack (uncatchable SIGABRT, even inside `recover:`); and map
  `==` was order-dependent while set `==` was order-independent. Fix: `MAX_STRUCTURAL_DEPTH = 10_000`
  threaded through display + a `values_equal_guarded` (the public `values_equal -> bool` stays a thin
  wrapper, so the ~66 hash-probe call sites are untouched); the recoverable depth-exceeded error surfaces
  only at the `==`/`!=` op sites. Map `==` is now order-independent value equality. (Interp's *call*-depth
  overflow in **debug** builds is left as-is — the tree-walk engine is slated for removal; release + VM
  are fine.)
  - **Airlock cyclic-sendable guard (2026-07-04).** The same class of bug survived on the concurrency
    airlock: copying a value across a task boundary (`spawn` arg / `Channel.send` / `Shared(...)` /
    worker return / M:N module-global snapshot) deep-walks it via `Vm::to_wire` / `to_snap` (src/vm/sched.rs)
    with **no depth guard**, so a check-accepted cyclic sendable overflowed the host stack → uncatchable
    SIGABRT on **both** engines. Fix: extend the same `MAX_STRUCTURAL_DEPTH = 10_000` recoverable-error
    guard into a shared depth-counted worker behind both serializers (`to_wire`→`to_wire_depth`,
    `to_snap`→`to_snap_depth`, fast path threads the shared budget) so serial `to_snap` and M:N `to_wire`
    trip at the identical depth; a cyclic value now degrades to the catchable `maximum structural depth
    (10000) exceeded (cyclic data structure?)` error, byte-identical serial vs M:N. Two `to_wire`
    call-sites already holding a span (serial `Executor.submit`, `#[cfg(test)]` worker return) route
    through `to_wire_at` so the error reports the real site, not line 0. Wide-but-shallow sendables (100k
    elements) still cross fine (the counter measures nesting depth). golden `examples/airlock_cycle.chz`
    + 5 unit tests (both-engine spawn/channel/shared, M:N-only module-global, wide-acyclic-crosses-fine).
- **`defer:` block form** — `defer` takes an indented block as well as a single call (multi-action cleanup
  without N `defer` lines), mirroring `spawn`'s dual form with no new VM op. Body runs top-to-bottom at
  scope exit, LIFO as a unit, free vars snapshot by value at the `defer` point, runs on all exit paths.
  A dedicated `defer_floors` write-gate rejects reassigning an enclosing local inside the block (no
  `SetCaptured` op); a `?` short-circuit inside the block is absorbed on both engines.
- **Integer `List.sum()` checked-add (2026-06-25).** The integer accumulation in `List.sum()` used a raw
  `acc += *n` on both engines — `[i64::MAX, 1].sum()` silently wrapped to `i64::MIN` (release) / host-
  panicked (debug) instead of faulting, while every other integer add (`+`, `+=`, `fold`, `*`, `/`) is
  checked. Now `acc.checked_add(*n)` raises the same recoverable `integer overflow in Add` at the
  `.sum()` call-site span, byte-identical to `+` (VM `vm/mod.rs` + interp `interp/builtins.rs`). The
  any-float path is untouched (accumulates to `float`, may reach `inf`). `examples/overflow.chz` now
  exercises the `sum` case alongside `math.abs`; two-engine parity tests `parity_list_sum_overflow` /
  `parity_list_sum_mixed_float`.

---

## Concurrency — feature-complete (confirmed 2026-06-12)

Core implemented through **M21** (still evolving; M19 perf in progress); **concurrency shipped through Tier-D (D0–D6c) + M-C**. The surface —
`spawn` / `parallel:` nursery / `Channel[T]` / `Shared[T]` / `Executor`, plus the VM's real OS-thread
engine and the netpoller + `std.net` — is complete and stable. **M-C implicit nurseries shipped
(2026-06-12)** — every function body and the module top level is an implicit nursery; a bare `spawn` is
legal anywhere and joins at `return`/end. ~1592 tests green; the cooperative engine (`--serial`) and the
OS-thread engine stay byte-identical on every `examples/parallel*.chz` + `examples/implicit_nursery.chz`
golden, and the frozen interp is the differential parity oracle for the sequential subset.

**CLI engine selection.** `chezzi run` now defaults to the OS-thread engine; `--serial` selects the
cooperative single-thread VM (the frozen parity oracle), `--parallel` is an accepted no-op alias, and
`--threads=N` (or env `CHEZZI_THREADS`, flag wins; `0`/omitted = all cores) sizes the OS-thread worker
pool via `vm::worker_count()`. `--threads` errors with `--serial`/`--interp` (neither is multi-threaded).

**`std.cancel` — cancellation tokens + `Channel.trip()` SHIPPED (2026-06-15).** A user-level
cooperative cancellation **`Token`** (Go-`context`-inspired, adapted): `cancel.manual()` /
`cancel.timeout(ms)`; methods `cancelled()`, `reason()` (`"cancelled"`/`"timeout"`), `done() ->
Channel[bool]` (a `wait:` arm), `cancel()` (anytime/any task), `deadline_at()`. **Tree propagation
landed** (see the next note). Pure Chezzi
(`std/cancel.chz`) over `Shared[bool]` +
`monotonic()` (deadline checked **at poll time** → timeout is deterministic across engines, no
background canceller) + ONE new native primitive **`Channel.trip()`** — a permanent level-trigger
latch (the manual-cancel fan-out a move-on-send `Channel` lacks; reuses `close()`'s wake fan-out
minus `closed`). Decoupled from the internal nursery cancel flag (so a user `cancelled()`-return runs
`defer`/`recover:` normally). Goldens: `examples/channel_trip.chz`, `cancel_manual.chz`,
`cancel_timeout_wait.chz` (byte-identical on cooperative-VM + interp); `examples/cancel_cpu.chz`
carries **no `.expected`** (manual cancel of a CPU sibling diverges by engine — default preempts,
`--serial`/`--interp` run to completion) and is covered by a Rust `#[test]`. A cross-task
cancel→`wait:` lost-wakeup regression (`MnSched::park`/`park_wait` gap re-check now includes
`done_latch`) is guarded by `cancel_trip_wakes_parked_wait_under_parallel`. Closes the `gaps.md`
cancellation gap (timeouts + manual cancel). See `docs/concurrency.md` §6e/§6c'.

**`std.cancel` TREE PROPAGATION — parent/child derivation SHIPPED (2026-06-17).** `Token.derive()`
(and the free-fn `cancel.derive(parent)`) builds a **child** token (Go `context.WithCancel`):
cancelling or timing-out a parent cancels every transitively-derived child, recursively root-to-leaves,
while cancelling a child **never** touches the parent (one-directional). The link is **live** — a
parent flip is observed by an already-derived child, *including one that crossed the
`spawn`/`parallel:`/`Channel` airlock* — because the link is the parent's `Shared` flag plus a `Shared`
registry of descendant `done()` channels, which cross as live cores exactly like the flat token's `flag`
(so the feature is automatically three-engine consistent — **zero Rust changes**, no checker change:
`sendable_rec` already permits the self-referential `parent: Token?` field + `Shared`/`Channel`/`Option`
arms). A child inherits the **tightest** deadline (soonest absolute of itself + ancestors; an
already-elapsed-timeout parent yields a child cancelled at once with reason `"timeout"`, its `done()`
ready via its own timer armed to 0 ms). `done()` cascades **transitively**: `derive()` registers a
child's `done()` channel into **every ancestor's** registry (walking the parent chain to the root, each
insert an atomic `Shared.update()` so concurrent siblings don't lose updates), so a manual `cancel()` at
ANY depth above trips the descendant's `done()` directly — a grandchild parked in `wait: leaf.done()`
wakes on a grandparent cancel, not just on its immediate parent. `reason()` is nearest-cause-wins
(self's own cause, else inherited). Goldens: `examples/cancel_tree.chz` + `.expected` (byte-identical on
`run`/`--serial`/`--interp`; `golden_cancel_tree_via_run_file` VM + `golden_cancel_tree_chz` interp
twin), plus eight VM unit tests (`cancel_child_*`, `cancel_transitive_grandchild`,
`cancel_grandchild_done_ready_after_grandparent_cancel` + `cancel_great_grandchild_done_ready_after_root_cancel`
— the transitive-`done()` guards, `cancel_token_sendable_with_parent` — the cross-airlock live-link
guard). **Known v1 limit:** the per-ancestor registry only **grows** (no token-drop hook); tokens are
request-scoped/short-lived, a future prune-on-cancel could clear it. Closes the `gaps.md`
tree-propagation gap. See `docs/concurrency.md` §6e.

> **`Channel.recv_timeout(ms)` — attempted then reverted (2026-06-12).** A bounded-wait `recv` was
> implemented with a **demote-always** shortcut (reuse `demote_recv_block` + a deadline) to avoid the
> heavier park+timer machinery. The review panel found it **unsound at `native_reentry == 0`**: (1) a
> top-level M:N `recv_timeout` demotes the worker, and a later reduction-budget yield strands the fiber →
> **silent hang**; (2) the cooperative park path reused `park_recv` (built for 0-arg `recv`) but
> `recv_timeout` has `argc=1` → **stack corruption** on resume; (3) cooperative-nursery no-producer faults
> `deadlock` not `None`, and demote-failure faults (not total). Reverted (commit `653dfd2`). **Lesson: the
> correct design is the heavier one** — at `native_reentry == 0`, snapshot-park on a timer (claim-flag +
> a `MnSched::timeout_wake` racing `send_wake`, like the socket-timeout `poll_timed_out` path), demote
> only at `native_reentry > 0`; cooperative needs a recv_timeout-aware quiesce (resolve-to-`None`, not
> fault) or accept the documented deadlock-fault divergence. Checker `Ty::Int → Option[elem]` sig + interp
> poll-once arm were correct; the VM scheduler integration is the hard part. A proper follow-up, not a
> drop-in. (`Atomic[T]` + `timer(ms)` have since **shipped** — see `concurrency.md` §6b/§6c,
> `examples/atomic.chz`. `wait` — Chezzi's `select` — is **designed + locked** (`concurrency.md` §6d),
> not deferred for lack of a design; it just awaits implementation as its own focused milestone.)

> **Concurrency follow-ups — `Atomic[T]` + `timer(ms)` LANDED, `recv_timeout` DROPPED, `wait` designed
> (2026-06-13).** Brainstormed the deferred trio and shipped two of three; `recv_timeout` is dropped as
> redundant.
> - **`Atomic[T]`** (commit `07ae080`) — generic atomic box mirroring `Shared[T]` (Mutex-backed, sendable
>   handle, value-first `Atomic(v)`): `load`/`store`/`exchange`/`cas` for any `T`, `add`/`sub` on numeric
>   `T` (checked-overflow like `+`/`-`). Two-engine parity; `--parallel` add/cas atomicity stress tests
>   (300-thread exact sum, 200-fiber CAS-retry). See `docs/concurrency.md §6b`.
> - **`timer(ms) -> Channel[bool]`** (commit `cd1673e`) — one-shot, **level-triggered** timeout channel.
>   Delivery is scheduled **at `recv` time in the receiver's own scheduler** (NOT at construction — a
>   top-level timer can be recv'd in a `--parallel` child): `--parallel` schedules a background `send` +
>   parks (accounted `inflight` so no false deadlock); cooperative VM / interp / callbacks inline-sleep to
>   the deadline (like their `sleep_ms`). 3-engine parity. Adversarial review (Reality Checker + Code
>   Reviewer) found **no Critical/Important** — sound park-gap (reuses `MnSched::park`'s queue re-check),
>   no inflight leak (job holds Arcs + always `fetch_sub`s), no double-schedule (queue-first on re-run).
>   Known v1 limitation: `timer.recv()` inside a native callback pins a worker (no demote). `docs §6c`.
> - **`recv_timeout` DROPPED** — `wait` + `timer` subsume it (`ch.recv_timeout(500)` ≡ `wait` over `ch`
>   and `timer(500)`), and it was the unsound/reverted one. No separate primitive.
> - **`wait` (select) — SHIPPED on ALL THREE engines (2026-06-13; M:N blocking park landed 2026-06-13).**
>   Full design + grammar + per-engine semantics in **`docs/concurrency.md §6d`** (cheat row in
>   `docs/syntax.md §11b`; `examples/wait_select.chz`). A `wait:` compound statement races channel
>   `recv`s — arms `v := ch.recv():` (`:=`/`=`/`_` targets), optional non-blocking `else:` (last), `timer`
>   arms, recv-only (unbounded channels → sends never block); source-order priority; closed+empty arm
>   **skipped**; all-closed+no-`else` faults. **Done:** lexer→parser (`parse_wait`)→checker (`check_wait`)
>   →interp (`exec_wait`, the parity oracle)→cooperative VM (`Op::WaitPoll` + `compile_wait`), incl. the
>   **cooperative multi-channel park** (one fiber filed under N keys via `wait_suspend`/`run_child`, swept
>   out of the other buckets on resume — `vm_wait_blocks_then_wakes_on_second_channel` +
>   `vm_wait_sweeps_other_buckets_after_waking`). **M:N `--parallel` blocking park — LANDED:** a blocking
>   `wait` now parks under `--parallel` instead of faulting. ONE `WaitPark { fiber, keys, claimed }` held
>   behind an `Arc`, with a `ParkedEntry::Wait(token)` filed in every arm's `MnSched.parked[key]` bucket
>   (`MnSched::park_wait`, the N-key generalization of `park`); the first waker CASes `claimed`, takes the
>   fiber, and sweeps the stale token out of all other buckets under one core-lock hold
>   (`send_wake`/`close_wake`/`cancel_drain`/`flag_deadlock` all token-aware). Routed via
>   `Disp::WaitPark(Vec<(key, core)>)` captured while the fiber heap is live (mirrors `Disp::Park`). The
>   1-key recv park stays the cheaper `ParkedEntry::Recv` case (alloc-free, byte-identical —
>   `vm_wait_single_arm_recv_park_unchanged_under_parallel`). Deadlock accounting: a wait-parked fiber is
>   `parked_n += 1` (ONE fiber, regardless of arm count) so the `is_deadlocked` predicate stays sound
>   (`vm_wait_lone_blocked_parallel_deadlocks`; a live sibling vetoes —
>   `vm_wait_sibling_send_vetoes_deadlock_parallel`). **`native_reentry > 0` (wait inside a native
>   callback):** can't snapshot-park → `demote_wait_block` blocks in place, polling all N arm queues
>   source-order on a bounded `DEMOTE_POLL_BACKOFF` (the N-arm analogue of `demote_recv_block`;
>   lower-throughput-but-sound **v1 limitation** — there are N channel condvars, no single one to block on).
>   All three engines byte-identical on `examples/wait_select.chz`; 150× + 4×80× stress loops clean (no
>   lost-wakeup). **Fixed in passing (a pre-existing two-engine parity bug exposed by the edge tests):**
>   the peephole optimizer did not relocate `Op::WaitPoll`'s `arm_targets`/`else_target` through its
>   fold/fuse index remap, so a multi-arm `wait` whose arm body fused a binop (`x + w`) jumped PAST the
>   bind prologue (VM 65 vs interp 66). Now `WaitPoll`'s targets are marked + relocated like `Jump`/
>   `MatchArm` (`relocates_waitpoll_arm_and_else_targets_past_a_fold`,
>   `vm_wait_arm_body_outer_local_in_binop_matches_interp`).

### Tier-D — complete (D0–D6c)

Designed in [`docs/concurrency.md §10`](docs/concurrency.md); the full per-phase TDD breakdown lives in
**[`docs/concurrency-tier-d.md`](docs/concurrency-tier-d.md)**. Landed, in one summary:

- **D0** — O(N²)→O(N·logN) cooperative ready-queue (per-nursery `ready` set + parked-index buckets).
- **D1** — lazy module snapshot: a shared read-only `Arc<ModuleSnapshot>` faulted into each worker heap
  on first access, killing the per-task module-graph rebuild.
- **D2a/D2b** — true **M:N work-stealing scheduler**: lightweight share-nothing fibers (own heap, carried
  in a swappable `FiberCtx`) multiplexed over the bounded pool, **parking on `recv` instead of pinning OS
  threads**; the joining thread runs an inline shell that alone guarantees completion (decision B).
- **D3** — BEAM-style **reduction-counting preemption** (`reds` budget, yield at exhaustion to the run
  queue's tail) so a CPU-bound fiber can't starve siblings.
- **D4** — Go-style per-worker local run queues + shared global overflow + random-victim work-stealing +
  periodic global check; runnable-gated park wake (a true `cv.wait` when `runnable==0`, bounded backoff +
  re-steal when `>0` — the mutex *is* the StoreLoad barrier, no Go-style fence needed).
- **D5** — **dirty/blocking pool**: a blocking off-heap-safe native (`read_file`/`write_file`, `fs.*`,
  `request`, `process`, `sleep_ms`) suspends the fiber and hands the call to a growable pool instead of
  pinning a core worker; an `inflight` fiber-state vetoes a false deadlock. A process-wide timer thread
  (later folded into the poll thread) parks sleepers on a deadline min-heap. *Path C* demotes the worker
  (one raw replacement OS thread, Go-`handoffp`-style) for a blocking `recv`/`sleep`/socket op reached
  *inside a native callback* (`native_reentry > 0`, host-stack loop frame, unsnapshotable).
- **D6a/b** — **netpoller** (`src/vm/poller.rs`, epoll/kqueue via `polling`): a would-block socket op
  becomes a cheap fiber-park. `std.net` (`Obj::Socket`/`Obj::Listener` over `Arc` cores) — non-blocking
  `connect`/`listen`/`accept`/`read`/`write`/`close`/`addr`; `connect` is true non-blocking via
  `socket2`. Drain-on-fault re-injects socket-parked fibers so a net server can share a nursery with a
  fallible sibling; one poll thread serves both socket readiness and sleeps.
- **D6c** — **per-socket read/accept/write timeout** (`--parallel`): `conn.read(n, timeout_ms)` /
  `sock.write(s, timeout_ms)` / `server.accept(timeout_ms)` return `Err("timeout")`; `0` polls once, a
  negative saturates. Reuses D6b's deadline-bounded poll, no new thread/heap/job (`poller::Parked` gains
  a `deadline`, a `fire_due_socket_timeouts` pass sets a per-fiber `poll_timed_out` marker). Checker
  gained optional trailing-arg arity. `examples/socket_timeout.chz`.

**Per-connection `spawn`** also landed — an **eager injectable nursery** (`--parallel` M:N, ≥2 cores): a
`spawn` in a *nested* `parallel:` runs concurrently with the rest of the body instead of queueing for the
join, so the canonical server shape (accept-loop `spawn`s a `handle(conn)` per connection) works. The
nested nursery is eager (`EnterNursery` builds the `MnSched` immediately + spawns one dedicated raw
drainer thread); a `spawn` injects a live fiber straight into it; a `body_open` flag holds termination
open and vetoes the deadlock predicate while the body may still inject. **v1 limits (documented):** needs
≥2 hw threads; bounded accept loops only (an unbounded `while true:` server never reaches the join —
graceful shutdown is future work); a handler talking back to the acceptor via a Channel is a cross-nursery
wakeup. `examples/echo_server_spawn.chz`.

**Cross-nursery flat scheduler — M:N (`--parallel`) DONE, cooperative DEFERRED.** The circular
outer-sibling cross-nursery deadlock (`examples/parallel_cross_nursery_circular.chz`: `inner()` spawns a
nested nursery while `main`'s outer `parallel:` still has an un-run sibling `O`; the inner owner used to
drain only its private queue and could never RUN `O` → `deadlock` fault) is **fixed under `--parallel`**:
- **One VM-global `MnSched`** with `SchedCore.scopes: Vec<JoinScope>` (replacing the scalar
  `{done,total,body_open}`) + a flat `slots` vec. Each nested nursery is a SCOPE enlisted into the SAME
  global run queue; `Fiber` carries a `scope_id`. The inline owner returns on a **scope-scoped stop**
  (`Take::Stop` when ITS scope's `done==total`, having drained the GLOBAL queue meanwhile — so it ran the
  cross-nursery sibling), while farmed helpers drain until global `terminate` (a `SENTINEL_SCOPE` owner id).
- A nested builder **early-enlists** the outer nursery's still-pending siblings (so the nested owner can
  run them — the cross-nursery wake) but **DEFERS** each enlisted scope's output flush to its OWN
  `JoinNursery` (`mn_scopes` records the scope; `mn_enlist_sched` holds the sched alive until the last
  enlisted scope joins). This preserves the **per-nursery-join flush order**, so three-engine parity for
  non-blocking nested spawns is byte-identical (`implicit_nursery_nested_functions` etc. unchanged).
  Outer scopes are enlisted **before** any helper worker is farmed, so a multi-task inner nursery can't
  trip the global deadlock predicate before the outer sibling is seeded (caught + regression-guarded by
  `examples/parallel_cross_nursery_fanout.chz` — a 2-task inner nursery, looped under a watchdog).
- The deadlock predicate + `finish`/`flag_deadlock`/`cancel_drain` went **global over scopes** (fault only
  when SOME scope is incomplete and nothing can progress anywhere); per-scope **cancel** Arcs (the shell's
  `self.cancel` re-pointed to the running fiber's scope cancel on each `run_one_fiber` swap-in;
  `cancel_drain(scope_id)` requeues only that scope's parked fibers) keep an inner fault from cancelling
  outer siblings (structured concurrency preserved). Genuine no-sender deadlocks still fault
  (`golden_parallel_deadlock_still_faults`, 30s watchdog).
- **Output order note:** because `O` (outer) and `I` (inner) live in DIFFERENT nurseries with different
  join points, the M:N flush order is `I` (inner join) then `O` (outer join) — i.e.
  `I got 1\nO got 1\ndone` — NOT the case-C single-nursery order (`O got 1\nI got 1`). Both complete; the
  ordering follows the parity-preserving per-nursery flush.
- **Eager nurseries unchanged (OPTION A):** the per-connection eager nursery keeps its OWN sched +
  dedicated drainer (single-scope fast path), untouched.
- **Cooperative (`run --serial`) + `--interp`:** still serialize nested nursery levels → the same program
  **still faults `deadlock`** there. The cooperative-engine flatten is a **separate, later commit**.
  Workaround on `run`: siblings in ONE nursery (doc case C). Golden is M:N-only (no coop/interp leg),
  watchdog-wrapped — mirrors `golden_channel_block`.
- **Post-review hardening (the first cut was REJECTED by the adversarial panel — 3 blocking; now fixed):**
  - **Inline outer-body `send`/`close` routing (charges #1/#2):** the inline `parallel:` builder runs with
    `self.mn == None` (sched only in `mn_enlist_sched`), so its own `send`/`close` used to bypass the
    global park set and never wake an enlisted, parked sibling → false `deadlock`. `channel_send_wire` +
    the `close` arm now route through `self.mn.or(self.mn_enlist_sched)`. Guards:
    `..._inline_send.chz`, `..._inline_close.chz`.
  - **`awaiting_builder` deadlock veto:** an early-enlisted scope is marked `awaiting_builder` (the live
    builder body is its feeder); `is_deadlocked` vetoes only while EVERY incomplete scope is awaiting the
    builder (`all_incomplete_awaiting_builder`). A genuine NESTED deadlock keeps a non-awaiting scope
    incomplete → still faults (`parallel_cross_nursery_genuine_nested_deadlock_still_faults`).
  - **Late spawn after enlist (charge #3):** a `spawn:` issued after `early_enlist_outer` drained the
    nursery vec used to be silently dropped at the join. `join_nursery` now runs the refilled tasks on
    the HELD flat sched (`mn_enlist_sched`) as a fresh trailing scope — `register_scope` is append-only
    (slots stay contiguous) and un-latches a stale global `terminate` so the inline owner runs the late
    task instead of stopping on the prior-scopes-all-done flag (no clobber of the held sched, no `index
    out of bounds` panic, no drop); `drain_escaped_nursery` reports them on an escape. Guards:
    `..._late_spawn.chz`, `parallel_cross_nursery_late_spawn_into_middle_runs`,
    `parallel_cross_nursery_late_spawn_escape_reports_pending`.
  - **Atomic enlist (charge #4):** `early_enlist_outer` now validates (prepares workers from clones)
    BEFORE consuming the nursery / registering a scope, so a `prepare_worker` `Err` (checker-gated
    backstop) can't leave an unseeded scope (hang) or a half-state — it unwinds cleanly.
  - **2+ enlisting levels — limit LIFTED (independent/normal nesting now RUNS):** the old blanket gate in
    `early_enlist_outer` ("2+ enlisting levels … aren't supported") was TOO BROAD — it regressed ordinary
    multi-level nesting (independent nested `parallel:` blocks with sibling/late `spawn:`s) that has no
    shared channel and never parks. The gate is GONE. Any depth of nested `parallel:` now matches the
    cooperative engine under `--parallel`. Only the genuinely-CONTENDED case (2+ live receivers racing ONE
    channel across nested scopes) remains divergent — and it is NOT gated: concurrent-divergent BY DESIGN
    (delivery order may differ, or it deadlock-faults; suspendable concurrency is VM-only/divergent), it
    only must never PANIC and never HANG. Guards: `parallel_cross_nursery_independent_3level_runs_all`,
    `parallel_cross_nursery_late_spawn_into_middle_runs`, `parallel_cross_nursery_contended_never_panics`,
    golden `examples/parallel_cross_nursery_multilevel.chz`.
    A late `spawn:` into a middle nursery runs on the HELD flat sched as a fresh trailing scope via
    `register_scope_seeded` — register + seed atomically under one core lock (mirrors `inject`), closing a
    `runnable==0` TOCTOU window where a SENTINEL helper could have falsely deadlock-faulted a parked outer
    receiver. Guard: `parallel_cross_nursery_late_spawn_parked_matches_coop`.
  - **Out of scope (documented separate limits):** the inline-body *blocking* recv (case B — wake-side
    fix only) and eager (per-connection) nurseries' private sched.

**`Channel.close()` + closed-channel semantics + `try_send` + `for v in ch:`** landed (both engines) —
the headline consumer-side feature giving clean producer→consumer termination (was: a consumer looping
`recv` after the producer was done could only deadlock-fault):
- `for v in ch:` — blocking iteration, drains buffered + future values, ends cleanly once
  closed-and-drained (Go's `for v := range ch`).
- `ch.close()` — idempotent, no args, wakes every parked/demoted receiver.
- `send` after close → faults; `recv` on closed-and-empty → faults (drains buffered first).
- `ch.try_send(v) -> bool` — the safe partner of `send` (`false` = closed; channels are unbounded, so
  closed is `send`'s only failure mode). `try_recv` unchanged (`None` on closed).
- Comprehension-over-channel (`[v for v in ch]`) is **rejected by the checker** (it would diverge — VM
  drains, interp oracle can't).

**Pending-`spawn`-drop on early `parallel:` escape → cancel-and-report** landed (both engines): a
`parallel:` body escaping via `?`/`return`/`break`/`continue` before the join now **cancels** unstarted
tasks (the same end-state a started sibling reaches under cancellation) and emits one byte-identical
stdout report line. VM routes a `drain_escaped_nursery` through four reclaim sites (`do_return`, the
recover-catch fault path, a net-new `Op::ReclaimNursery` for break/continue, and the `do_try` recover-
scoped-`?` short-circuit, which drains the escaped body's defers to its floor *before* the report so
interp order is restored).

### Group B (B3.0–B3.6) — the OS-thread multicore epic, complete

Decomposed and documented in **[`docs/concurrency-b3.md`](docs/concurrency-b3.md)** (validated
shared-nothing architecture, decisions A–G, risk register). Summary of the landing:

- **B3.0–B3.2** — a `WireValue` airlock (`src/vm/wire.rs`) replaced `deep_clone`; `Channel`/`Shared`/
  `Executor` cores moved out of the GC heap into `Arc<…Core>` (`src/vm/core.rs`); `program` went
  `Rc<Program>` → `Arc<Program>`; `Vm::spawn_worker`/`run_task_isolated` build an isolated worker `Vm`
  with its own heap and cross args/captures/result by wire (cross-heap safety enforced via
  `ensure_crossable`). All single-thread, behavior byte-identical.
- **B3.3** — `str` crosses by value (`WireValue::Str`); the **G1 module-globals checker gate** (mutating
  a module global reachable from a `spawn` task is a type error, *"use Shared[T]"* — scope-aware,
  transitive over the free-fn call graph); worker module-graph reconstruction (read-only `home` snapshot
  + method tasks); then **real OS threads behind `--parallel`** (bounded pool, parent participates inline,
  per-core condvar `recv`, `Shared.update` lock).
- **B3.4** — cooperative **cancellation** + cross-thread `os.exit` (per-nursery `cancel` flag, first
  fault/exit trips it; `os.exit` wins over any sibling fault; cancel bypasses `recover:` but still runs
  `defer`s). Single-level only — nested-nursery cancel propagation is documented/deferred.
- **B3.5** — nursery-local **deadlock detection** under threads (barrier-confirm detector; later retired
  in favour of D2b's exact single-coordinator predicate).
- **B3.6** — `Executor` on the pool + the **A3b `submit`-capture sendability gate** (checker). Under
  `--parallel` a submitted closure crosses by value (`WireValue::Closure`); the cooperative default
  engine keeps crossing it by handle so its same-heap drain shares captures by reference (matching the
  interp oracle — a by-value snapshot would break parity for the sequential subset).

### M-C — implicit nurseries (shipped 2026-06-12)

Every function body and the module top level is an implicit nursery that joins at its `return`/end
(module top joins at program exit); a bare `spawn` is legal anywhere, dropping the explicit `parallel:`
requirement. `parallel:` is demoted to an explicit *inner* sub-nursery for earlier joins. Design:
[`docs/concurrency.md §10`](docs/concurrency.md). Concurrency is now feature-complete (no Tier-E).

- **Join-on-exit.** `return <value>`, fall-through end, and `?` early-return are all join points —
  spawned tasks run FIFO, *then* control leaves; `defer`s run after the join (tasks, then cleanup). A
  `return`/`?` that escapes an *inner* `parallel:` still cancels-and-reports that inner nursery while
  joining the function's implicit one. An uncaught body fault cancels-and-reports the implicit nursery
  (abnormal exit) — identical to an explicit `parallel:` escape.
- **Single join site + zero-overhead gate.** Compiler pre-scans a body for a bare `spawn`
  (`compiler::block_has_bare_spawn`, stops at `parallel:`/nested-fn/`spawn:`-block); if present it emits
  one opening `Op::EnterNursery` and sets `Proto::has_implicit_nursery`. The VM's `do_return` joins it
  (cancel-inner-then-join-implicit, before defers) for `return`/`?`/end. Bodies with no bare spawn emit
  byte-identical bytecode to pre-M-C — perf benches (no spawns) unchanged.
- **Implicit nursery sites.** Function bodies, the module top level, **`spawn:` blocks, and `defer:`
  blocks** each get their own implicit nursery (each runs in its own frame; a bare `spawn` inside binds
  to *that* body's nursery). Joins at the body's own `return`/end.
- **Three-engine parity.** Interp (`call`/`run_block_task`/`eval_top_level` push an implicit nursery +
  `leave_implicit_nursery` join/cancel), cooperative VM, and `--parallel` are byte-identical. Tests:
  `vm::tests::implicit_nursery_*` (3-engine, incl. `_try_preserves_error_value` +
  `_spawn_in_defer_block` review-panel regressions), `interp::tests::implicit_nursery_*`, golden
  `examples/implicit_nursery.chz`. Checker `spawn_at_function_scope_ok` / `spawn_in_plain_fn_ok` /
  `spawn_at_module_toplevel_ok` (the old `spawn_outside_parallel_rejected` flipped); dead
  `nursery_depth` checker field removed.
- **RESOLVED (2026-06-12) — uncaught-fault cancel-report parity:** an *uncaught* fault with un-run
  nursery tasks now prints the cancel-report on the VM's stdout too, matching the interp and the
  `--parallel` engine. Three coordinated fixes in `src/vm/mod.rs`: (1) `unwind_deferred` gained a
  `report_escaped: bool` param — on a genuine fault (passed `true` from the fault-unwind arm; `false`
  from the two B3.4-cancel paths) it now cancels-and-reports each discarded frame's escaped nurseries
  **before** that frame's `defer`s run, matching the interp order (`exec_parallel` /
  `leave_implicit_nursery` report as the body unwinds, then `finish_frame` runs defers); the old
  `_ => return Err(rte)` uncaught arm reported nothing. (2) `drain_escaped_nursery` now reports
  **per-nursery** (innermost-first), not one combined line — two stacked nurseries → two lines, not
  `2 pending` (also fixed a latent recover-caught combine divergence). (3) the MODULE top-level
  nursery is preserved (`nursery_len + 1` floor): an uncaught *top-level* fault stays silent on both
  engines (it joins only on clean program exit). Review-panel (SRE) caught a defer/report interleave
  divergence the first cut missed; cold pass verified the shared `unwind_deferred` interactions.
  Tests: `vm::tests::uncaught_fault_reports_implicit_nursery` / `_explicit_parallel` /
  `_each_nursery_separately` / `_reports_before_frame_defers` / `_interleaves_report_and_defer_per_frame`
  / `_uncaught_toplevel_fault_does_not_report_module_nursery`, plus `recover_caught_fault_reports_*`.
  Full suite green (1600), three-engine parity.

### Standing decisions & contracts (do not re-litigate)

> **DECISION — do NOT build interp B1/B2 (suspendable tree-walker). Deliberate non-goal.** The interpreter
> stays frozen at the sequential concurrency subset and serves as the differential-testing parity oracle
> for the non-blocking surface (its real value: catching VM / GC / compiler bugs). Suspendable execution
> would need stackful coroutines or a full CPS `eval` rewrite — large, risky, covering a slice the oracle
> does not need. **The VM is the sole concurrent engine.**

- **Parity contract (narrowed, intentional):** the engines agree on the **sequential subset** — all
  *non-blocking* `parallel:` / `spawn` / `Channel` / `Shared` / `Executor` programs (byte-identical,
  parity-tested). **Suspendable concurrency (blocking `recv`) is VM-only by design**: under `--interp` a
  blocking `recv` faults `deadlock` (pinned by an interp test vs the VM golden). This divergence is the
  stated contract, not a bug.
- **Known VM v1 limits (acceptable; not parity issues):** a blocking `recv` reached inside a native
  callback (list HOFs, `sort`, `compare`/`hash`/`str` hooks, `Shared.update`, executor drain, a `defer`red
  call) faults `deadlock` *unless* Path C demotion applies (`recv`/`sleep`/socket under `--parallel`); a
  fiber blocked in an outer nursery *is* woken (D0 cross-level wake-marking, common case works); the narrow
  circular case (its unblocker is an outer sibling the inner scheduler must run) is **RESOLVED under
  `--parallel`** by the M:N flat scheduler (see the cross-nursery section above) but **still faults
  `deadlock` on the cooperative `run`/`--interp`** engines (the cooperative flatten is a separate, later
  commit). Independent/normal multi-level nesting (no shared channel) RUNS under `--parallel` and matches
  coop (the old "2+ enlisting levels" gate is gone). Residual M:N limits: a genuinely-CONTENDED shared
  channel across nested nurseries (2+ live receivers racing ONE channel) is concurrent-divergent BY DESIGN
  (delivery order may differ, or it deadlock-faults — never panics/hangs); the inline outer-body's
  *blocking* recv (case B — wake-side fix only; put blocking work in a `spawn:`); and eager
  (per-connection) nurseries' private sched.
  Fix design + resolution in [`docs/cross-nursery-flat-scheduler.md`](docs/cross-nursery-flat-scheduler.md);
  correct cooperative pattern in `examples/parallel_cross_nursery_ok.chz`.
  Documented residuals: a narrow parked-sibling false-positive under multi-demote; the `Shared.update`
  same-box recv hazard; a saturated-pool queued-task counted live (no-false-positive choice).
- **Use `iter.map`/`iter.filter`/`iter.fold`/`iter.reduce` (chezzi source, `std/iter.chz`)** if a
  callback may block under `--parallel` — they run through VM frames so a blocking `recv` parks. The
  native `xs.map(f)` is the faster non-blocking path (and demotes via Path C if a `recv` blocks in it).

**Permanent non-goals:** interp B1/B2 (above); variadic args, bignum (`i64`-only — every overflow is a
recoverable fault; binary work → the `bytes` (immutable) + `bytearray` (mutable) *sequence* types, both **shipped** — no `byte`/`u8` scalar). **Level-3 dynamic
C-ABI FFI is NO LONGER a non-goal — v1 shipped** (`extern "lib":` scalar calls via dlopen+libffi,
**plus opaque C `void*` handles** via the `ptr` type — `Obj::Ptr`/`Value::Ptr`, `std.ffi.null()`/
`is_null`, untyped + manual-free, `examples/ffi_ptr.chz`; **plus the return-only `str` opt-ins
`owned_str`** (copy + libc `free`, no leak) **and `str?`** (`NULL` → `None`, `examples/ffi_str.chz`);
**plus bidirectional fixed-width integers `int8`..`uint64`** (bind C `int32_t`/`uint32_t`/…;
truncate-on-param / sign-or-zero-extend-on-return, **imported per-name from `std.ffi`** — Chezzi's
first type imports, `examples/ffi_int.chz`);
**plus flat-scalar structs by value** (a Chezzi `struct` of scalar fields ↔ a C struct passed/returned
by value, `examples/ffi_struct.chz`);
**plus `bool` ↔ C `_Bool`** (1 byte — params/returns/struct fields; int-returning predicates like
`isdigit` bind `-> int` + test `!= 0`);
**plus sync scalar callbacks (#4)** (a `fn(scalars) -> scalar` extern param → a libffi closure
trampoline C calls back synchronously, same-thread, scalars only; faults caught + re-raised; both
engines + `--parallel` parity; `src/native/cffi.rs` `CType::Callback` + `Host::invoke_callback`);
nested structs / `str` struct fields / **the rest of callbacks (#4 — stored/cross-thread + pointer-deref
builtins)** / **varargs (#5)** (with design notes + the callback feasibility ladder +
a varargs fixed-arity workaround in `docs/ffi-and-packaging.md §1b`),
a custom user-named deallocator, C-spelling int aliases (`c_int`), and the rich Rust
`Box<dyn Any>` userdata handle still deferred — see "Done" below; forward design for the Rust
userdata Value + the package registry is in
[`docs/ffi-and-packaging.md`](docs/ffi-and-packaging.md)). **`yield`/generators are likewise
no longer a non-goal — complete VM-only support shipped** (see below).

> **`yield`/generators — complete, VM-only (landed on `feat/yield-generators`).** No longer a
> non-goal: a `fn` declaring `-> Iterator[T]` may `yield`; the call returns a suspendable generator
> (a one-shot cooperative coroutine — its own private frame/stack swapped into the VM, resumed by an
> intrinsic `.next()` that the `for`-loop step drives). VM-only: the frozen interpreter rejects
> `yield` (it cannot suspend a native Rust call), so **two-engine parity is waived** for generators.
> `defer`/`spawn`/`parallel:`/`wait:` are checker-forbidden inside a generator. See
> `examples/generators_basic.chz`, the `vm_generator_*` tests, and the `generator_*` checker tests.
> The adapter-struct model over `Iterator[T]` (`examples/iter_adapters.chz`) stays the parity-clean,
> recommended way to write lazy sequences.

---

## Done (newest → oldest)

One bullet per milestone/epic. Full landing detail (TDD notes, review-panel findings, test-count deltas,
branch names) is in the git log.

- ✅ **`print`'s `str(self)` display hook is gated on ACTUAL `Stringable` conformance, killing an uncatchable SIGABRT** (2026-07-04, Bug B) — the VM stringifier selected a user `str(self)` method as the implicit display hook by NAME + ARITY only, ignoring its return type. A `fn str(self) -> S` (returns the struct, not `str`) was chosen; the stringifier got an `S` back, re-stringified it, re-invoked the hook, and recursed forever → `fatal runtime error: stack overflow` (SIGABRT, uncatchable by `recover:`) on a check-accepted program. Fix (VM-only, `src/vm/stmt.rs` struct/enum/newtype display-hook arms): invoke the hook, then use its result ONLY when the RETURNED VALUE is a `str` (new `Vm::is_str_value`, mirroring `arith.rs` `struct_hash`/`enum_hash`'s invoke-then-check shape); a non-`str` result is NOT re-stringified — it falls back to the default repr, like a wrong-arity `str` already did. Checking the returned VALUE (not the declared syntax) covers an annotated `-> str`, an INFERRED (un-annotated) str, and a str type-ALIAS return alike — an earlier syntactic `-> str` gate was rejected in review because it silently regressed idiomatic un-annotated/aliased `str` hooks to the default repr (checker-vs-VM `Stringable` divergence). GC-safety: the non-`str` fallback re-reads the LIVE rooted struct/enum/newtype (the hook may have mutated a field and swept the pre-hook clone), so the default render never dereferences a dangling GcRef. `str` stays a normal user method — a direct `obj.str()` returning non-str is untouched (no checker rejection). +5 tests (`src/vm/tests.rs`, all `assert_mc_parity` → serial + M:N): the repro (struct/enum/newtype self-return → default repr), annotated/inferred/aliased str all still used by print + interpolation, direct-call-returns-non-str still works, and a GC-stress fallback (hook mutates a non-interned `List` field + 100k-alloc loop + returns self → correct re-read state, no panic). Docs: `docs/syntax.md` Stringable section now documents the display-hook resolution rule.
- ✅ **A runtime fault inside a `"{…}"` interpolation fragment now reports the fragment's real source LINE (was `line 1, col 1`)** (2026-07-04, `auto-task/interp-span-fix`) — any runtime fault (div-by-zero, index-out-of-bounds, integer overflow, …) whose faulting op sat INSIDE a string-interpolation fragment reported its span as `line 1, col 1` instead of the fragment's true line; the identical fault OUTSIDE interpolation reported correctly. This was the runtime/compiler counterpart of the 2026-06-30 `never-recover-span` checker fix (which corrected only the fragment ROOT nil-error span). Root cause: interpolation fragments are re-lexed from the escape-processed `raw` substring via `lexer::tokenize` (`src/interpolation.rs`), and `Lexer::new` hardcodes `line = 1`, so every fragment token span — and thus the arith/index opcode span the compiler emits and `Vm::err` renders — was fragment-relative (root at 1,1). Both serial and M:N VM share this codegen, so both printed the identical wrong span (why two-engine parity never caught it — it is a shared misleading-diagnostic bug, not a divergence). Fix (Strategy A, span-metadata only, runtime-inert): added a `base_line: usize` field to `Lexer` (default 0 → all normal lexing byte-identical) applied in the sole span funnel `span_at` as `line: self.line + self.base_line`, plus `Lexer::new_at` / free `lexer::tokenize_at`; `parse_interpolation` passes `base_line = span.line - 1` per fragment — the string literal's OPENING source line — so a fault inside any fragment reports that real line instead of `line 1`. We anchor to the opening line rather than the fragment's exact inner line ON PURPOSE: `raw` is the post-escape payload, where a `\n` ESCAPE and a genuine (triple-quoted) source newline are indistinguishable, so counting newlines in `raw` would inflate the reported line past an escape and point at UNRELATED code (a confidently-wrong diagnostic — flagged and fixed in review before merge). Opening-line is honest and never misattributes. COLUMN stays best-effort/fragment-relative (`col 1`) — also unrecoverable from the escape-processed substring. The shared parser hands the checker opening-line fragment spans too (symmetric, zero blast radius). +6 tests (`src/vm/tests.rs`): div-by-zero / index-OOB / overflow inside interpolation each report line 4 on BOTH engines, a multi-line triple-quoted fragment attributes to the string's OPENING line 4, a `\n`-escape-before-fragment fault stays on line 4 (regression guard against the escape miscount), and a non-interpolation fault + valid interpolation regression proves `base_line=0` leaves normal lexing byte-identical — all RED-first. Docs: refreshed the now-stale span doc comment in `src/checker/pattern.rs::check_interpolation`.
- ✅ **A nursery deadlock-abort now preserves a still-parked task's buffered stdout (two-engine parity)** (2026-07-05, `auto-task/deadlock-flush-parked-stdout`) — when a `parallel:` nursery was aborted by the M:N scheduler's deadlock detector, a still-PARKED task's ALREADY-buffered stdout was silently DISCARDED on the default M:N engine, while `--serial` printed it — a two-engine divergence on a DETERMINISTIC program (a consumer that prints three lines then blocks forever on a second `recv()` lost all three on `chezzi run`, kept them on `run --serial`). This was the exact gap the fault-output-flush entry below left open: `SchedCore::flag_deadlock` (`src/vm/mod.rs`) wrote each parked fiber's `TaskOutcome::Fault` slot with `out: String::new()`, discarding the fiber's own buffered output (`swap_ctx` had moved it into `f.ctx.out`/`f.ctx.stderr` when it parked), so the downstream `reduce_task_slots` propagated the deadlock error with an EMPTY buffer. Fix (~4 lines, the exact analogue of 888684d's real-fault fix): `flag_deadlock` now moves `f.ctx.out`/`f.ctx.stderr` into the `Fault` slot instead of allocating empties (`task_index`/`scope_id` are Copy, read before the partial move). `reduce_task_slots` is UNCHANGED — it still flushes only the lowest-index propagating fault's buffer at its task-order slot, so for a sole-printer parked task the transcript is byte-identical to serial; the pre-existing multi-printer residual race (documented in `reduce_task_slots`) is unchanged. Scoped to the ONE `flag_deadlock` method (reached only by the M:N nursery deadlock detector); serial, real-fault, completed-sibling, and non-nursery-deadlock paths never route through it, so none regress. +1 two-engine parity test (`parallel_nursery_deadlock_flushes_parked_stdout_2engine`, sole-printer, looped 50× for interleaving flakiness), RED-first on the M:N arm only (serial passed pre-fix). Docs: `docs/concurrency-tier-d.md` (Decision F), `docs/concurrency-b3.md` (B3.5).
- ✅ **A faulting `--parallel` task now preserves the stdout it buffered before the fault (two-engine parity)** (2026-07-04, `auto-task/fault-output-flush`) — a spawned task that FAULTS (panic / uncaught runtime error propagating to the nursery join) silently DROPPED all stdout it emitted *before* the fault, but ONLY on the default M:N OS-thread engine — the `--serial`/interp oracle preserved it (a two-engine divergence losing the user's debug output right before a crash, on the DEFAULT `chezzi run`). Root cause in `src/vm/mod.rs`: `TaskOutcome::Fault` was a tuple variant carrying NO buffered output, unlike `Exit { code, out, stderr }` and `Done(WorkerResult)`; every Fault construction site dropped the shell buffer and `reduce_task_slots` flushed only `Done`/`Exit` output in task order. The old rationale ("a faulting worker's partial output never had a deterministic position") was wrong for the LOWEST-index *propagating* fault: it has exactly the slot position the serial engine emits it at (after lower-index Done/Exit, before the propagated error is handled). Fix (localized to the fault-output-flush seam): made `Fault` a struct variant `{ err, out, stderr }` mirroring `Exit`; the real Fault sites that own a live buffer (`run_outcome`, `classify_mn_outcome`) `mem::take` the shell `out`/`stderr` (the Rust-panic-to-fault site carries an empty buffer; the deadlock-terminate site was later fixed to carry the parked fiber's own buffer — see the deadlock-flush entry above); `reduce_task_slots` flushes the terminal (`first_fault.is_none()`) fault's buffer INLINE at its task-order slot, then records the error. Higher-index racy faults + `Cancelled` still drop (no deterministic slot); `Exit`-over-`Fault` precedence byte-for-byte unchanged; the cooperative/`--serial` oracle untouched (the fix makes the default engine MATCH it). Fault-free goldens only ever hit `Done`, so byte-identical. **Residual (intentionally not chased):** byte-for-byte oracle parity holds only when the faulting task is the nursery's SOLE output-producer — with additional output-producing siblings the M:N transcript can still diverge from serial's stop-at-first-fault order (a sibling reaching `Done` before the faulter's cancel-trip keeps output serial never produced; `Fault`-vs-`Cancelled` classification is itself a scheduler race), a pre-existing nondeterminism the buffer-and-flush model cannot reconcile. +1 three-engine parity test (`parallel_faulting_task_flushes_partial_output_3engine`, single-faulter — the only deterministic fault-output shape), RED-first on the `--parallel` arm only. Docs: `docs/concurrency-tier-d.md` (Decision F), `docs/concurrency-b3.md`.
- ✅ **Float→string for large integral floats now shortest-round-trip-correct** (2026-07-01, `auto-task/float-shortest-roundtrip`) — the integral-valued branch of float formatting used `format!("{x:.1}")` (exact fixed-point expansion of the binary `f64`), so a large whole-valued float printed the artifact digits of its binary value instead of the documented shortest decimal that round-trips: `1.5e23` → `150000000000000004194304.0` (should be `150000000000000000000000.0`), `6.022e23` → `602200000000000027262976.0`. Contract (`docs/syntax.md:1787`, unchanged — the docs already promised "shortest-round-trip-correct … spelled out in full") was violated by the implementation. Fix: render the integral branch via Rust's default shortest `{}` Display (guaranteed fewest round-tripping digits AND never scientific notation for f64) then append a literal `.0` to preserve Chezzi's always-a-decimal-point invariant — `format!("{x:.1}")` → `format!("{x}.0")` in ALL THREE lockstep sites (`vm::format_float`, `interp::value::format_float`, `fmtspec::format_float_like`, single commit) so the stringify path and the bare-format-spec path stay identical and VM==interp holds. Behavior-preserving for every already-correct case (`3.0`, `-0.0`, `0.0`, `100.0`, `1e20`→`100000000000000000000.0`, negatives); the explicit `:e`/`:f`/precision spec arms are untouched. +1 golden `examples/float_large_integral.chz` (bare interpolation + bare-spec + small-integral controls, VM==interp==`.expected`), RED-first; `examples/literals.expected` avogadro artifact updated to the shortest form.
- ✅ **Never/bottom `recover:` payload now CONSISTENT + accurate interpolation-fragment nil-error span** (2026-06-30, `auto-task/never-recover-span`) — two coupled checker-only corner fixes. (A) `infer_recover` (`src/checker/mod.rs`) typed a `recover:` block by its tail: a direct `recover: panic(...)` went through the `StmtKind::Expr` arm and `infer`'d `panic` to bottom (`Ty::Unknown`, accepted as an `Ok` payload in value position), but a tail whose panic was reached through one more **statement-form** layer (`recover:\n  match 1:\n    _: panic("boom")`) went through `_ => check_stmt` and left `value_ty = Ty::Nil`, so the `Ok(v)` payload typed as `nil` and was rejected ("expression returns no value (nil) and cannot be used as a value") — the SAME bottom value usable in one path, banned in the other. Fix: after the tail check, `if value_ty == Ty::Nil && Self::stmt_terminates(last) { value_ty = Ty::Unknown; }`, reusing the existing sound, conservative divergence predicate (statement-form match all-arms-terminate, `while true:`, all-branch-returning `if/else`, trailing `exit`/`panic`). Both repro forms now accept; the `== Ty::Nil` guard keeps concrete-tail (`recover: 5` → `int`) and non-diverging-statement-tail (`recover: x := 5` → `Result[nil]`, still nil-banned) recovers untouched. (B) `check_interpolation` (`src/checker/mod.rs`) inferred each `{…}` fragment from a sub-parse with fragment-relative spans (root at `line 1, col 1`), so a nil-in-value-position error keyed on the fragment ROOT reported the bogus `(1,1)` fallback instead of the offending string literal; now stamps the whole-string-literal span onto the fragment root (`e.span = span`) before `infer_value`, matching the compiler's emit site, so the diagnostic carries the real line/col. Parity-safe by construction (check-time only; rejected programs never run, and the newly-accepted match-panic recover runs identically on VM and `--serial` → the panic takes the `Err` branch, the `Ok(v)`/`Unknown` arm is statically unreachable). +4 checker tests via `entry_ok`/`entry_rejects`/`check_entry` (diverging-match-tail accepts, direct-vs-match-panic consistency, concrete/non-diverging-tail regression fence, interpolation-void-fragment span ≠ (1,1)), all RED-first.
- ✅ **Expected-type inference: a type ANNOTATION now pins a generic ctor / generic fn-call's type params** (2026-06-30, `auto-task/expected-type-generic-inference`) — checker had checking-mode ("expected type flows IN") only for empty container literals and closures bound to `fn`-typed annotations; a generic constructor or generic function call was always inferred bottom-up, with the annotation used only as a POST-HOC `assignable_w` check. So `a: Heap[int] = Heap([], fn(x, y): x < y)` (and the return-type / call-arg forms) hit the un-inferable-closure-param deadlock — the empty `[]` couldn't pin `T`, the bare comparator params couldn't either, and `report_uninferable_closure_params` fired *before* the annotation could break the tie ("cannot infer type parameter `T` of `Heap`" + "cannot infer type of parameter 'x'/'y'"). Fix (checker-only, parity-safe by construction — generics are type-erased at runtime, no opcode/runtime change): a new `expected_hint: Option<Ty>` field threads the expected type from the three annotation sites — a `let`-binding's declared type (`StmtKind::Let` non-`ref` single-name non-closure branch), a function's declared **return** type (`check_return` non-closure branch), and a call **argument**'s declared parameter type (`infer_arg` non-closure branch) — into `infer_call`, which `take()`s it FIRST (so nested arg calls see `None`, no leak) and threads it through every generic ctor/call dispatcher (`infer_named_call` struct/newtype arms, `infer_qualified_struct_call`, `infer_newtype_call`, `infer_variant_call`, `infer_generic_call`). Each consumes it via a new `seed_from_hint(hint, &<declared-return-SHAPE>, &mut sub)` (`Struct(key,[Param…])` / `NewType` / `Enum` / a generic fn's `sig.ret`) placed AFTER arg-unification + `recover_iter_elems`/`recover_index_args` and BEFORE `report_uninferable_closure_params` — so precedence is **turbofish > arguments > annotation** (`unify` only binds a still-free param; an arg that pins `T` differently is the usual mismatch). Once `T` is seeded the existing `check_generic_arg` re-infers the comparator closure in checking-mode against `fn(int,int)->bool`, so the secondary "cannot infer parameter 'x'/'y'" errors also vanish. Bonus (same seam, for free): generic **newtype** ctor annotations (`e: Stack[str] = Stack([])` — previously needed a turbofish) and a return-only param of a generic fn (`xs: List[int] = empty()` for `fn empty[T]() -> List[T]`, previously "cannot assign List[T] to List[int]"). +6 graph_tests (3 primary repros let/return/call-arg + qualified-ctor + free-fn-return + a turbofish/annotated-closure/args-win-mismatch regression guard), all RED-first. **Remaining gap (documented, not forced):** a generic ctor nested inside a *container literal* (`a: List[Heap[int]] = [Heap([], …)]`) — the outer expr is a list literal, never reaches `infer_call`, so it would need a separate `infer_list` element-hint; annotate the closure params or turbofish there. Docs: `docs/syntax.md` (closure-param inference + generic-newtype §), `docs/spec.md` (new expected-type-inference note + newtype ctor), `docs/stdlib.md` (Heap §).
- ✅ **Container constructors `List[T]()` / `Map[K,V]()` / `Set[T]()` + bare `List()`/`Map()`; un-inferable-closure-param diagnostic; std standalone-check; std-module test wiring** (2026-06-29, `auto-task/container-ctor-turbofish`) — four bundled audit findings. **A (the ask):** the turbofish was rejected on every builtin but `Channel` — `name_is_generic` (`src/checker/mod.rs`) now also accepts `List`/`Map`/`Set`, and the three ctor arms read the type args (1 for List/Set, 2 for Map; arity-checked), so `List[int]()` pins an empty list's element type, `List[int]([1,2])` checks elements against it, and bare `List()`/`Map()` are now legal (mirroring the already-legal `Set()`), refined from the expected type / first use. **A is NOT checker-only** (audit's claim was wrong): 0-arg `List()`/`Map()` were rejected at RUNTIME in both engines too, so `builtin_list`/`builtin_map` in `src/vm/mod.rs` AND `src/interp/mod.rs` now return an empty container for 0 args (Set's existing shape) — two-engine parity held via a new `examples/container_ctor.chz` golden. **B:** `Heap([], fn(a,b): a<b)` leaked a misleading "cannot compare T and T" from inside the lambda; a new `report_uninferable_closure_params` guard (bare struct-ctor path + `infer_generic_call` + the module-qualified struct-ctor path `infer_qualified_struct_call`, so `c.Heap([], fn(a,b): a<b)` gets the same message) detects the genuine deadlock — an unbound type param appearing in an *unannotated* closure's PARAMETER slot — and emits "cannot infer type parameter `T` of `Heap`; annotate `Heap[T](…)` or the closure parameters", binding the param to `Unknown` to suppress the cascade. Scoped to parameter positions only (so `Mapped`-style `fn(T)->U` with `U` inferred from the body does NOT trip it) AND **probed against the closure body** (`trial_check_closure_args`, two trials — params left as the unbound `Ty::Param` vs. bound to `Unknown`): it fires ONLY when the body actually constrains the param (`a < b` errors unbound but is clean under `Unknown`), so a harmless body that doesn't need `T` (`each([], fn(x): print(x))`, `mapper([], fn(x): 42)`) keeps type-checking — it ran on `main` and must not be newly rejected — and an unrelated body error (errors under BOTH trials) is left for the normal per-arg check to report as itself. **C:** standalone `chezzi check std/…chz` reported phantom "unknown type 'RwShared'/'Shared'" — stdlib auto-privilege was granted only on the import path; `LoadedModule::is_std` (`src/resolver/mod.rs`) is now path-aware (file under `std_root()`), fixing the editor/LSP false positives; new lib test standalone-checks every `std/**/*.chz`. **D:** the committed per-module std test files (`collections`/`concurrent_collection`/`datetime`/`path`) existed but only 4 unrelated `_test.chz` were in the `cargo test` dogfood guard — all four are now registered (`src/test_runner.rs`). Docs: `docs/syntax.md` + `docs/stdlib.md` (turbofish + bare-empty constructor forms). `Shared`/`RwShared`/`Atomic` turbofish left OUT (value-first; tests intentionally reject it). **[SUPERSEDED 2026-06-30:** the value-first concurrency boxes now ALSO accept an optional, value-checked turbofish — see the "Turbofish construction on the value-first concurrency boxes" entry in *Current focus*.**]** Checker/resolver-only on the type side; both engines exercised; conformance green.
- ✅ **Negative int literal/range patterns + match-doc qualifier fix** (2026-06-28, `auto-task/neg-match-patterns`) — two bundled match-pattern changes, one commit. PART 1 (parser/grammar only — AST/checker/compiler/vm/interp already `i64`-signed, NO runtime change): a leading `-` in a pattern was a hard parse error ("expected identifier, found '-'"). Root cause: `parse_pattern_primary` (`src/parser/mod.rs`) only entered its int branch on `Token::Int`. Fix: new `expect_pattern_int` helper (eats an optional `Token::Minus` then `expect_int`), and widened the literal arm to `Token::Int(_) | Token::Minus`, using it for the literal AND both range bounds → `-3:`, `-10..-5:`, `-10..5:`, `0..-5:` all parse (and compose with guards/or-patterns). Stays **int-only**: a negative float `-3.0:` now routes through `expect_int` and is rejected "expected integer, found float" (no float pattern added; positive `3.0:` unchanged). A negative literal arm is still refutable — `_` is still required for exhaustiveness. `docs/grammar.bnf` `<patternPrimary>`: +4 `MINUS` alternatives + int-only comment; new accept/reject conformance corpus (`tests/corpus/accept/match_neg.chz`, `tests/corpus/reject/match_neg_float.chz`). +2 parser tests, +2 VM `run_parity` tests (neg literal/range + neg-with-guard/or), +2 checker exhaustiveness tests. PART 2 (doc-only): `docs/syntax.md` match/enum examples showed BARE arms (`Circle(r):`, `Leaf:`) but the impl requires QUALIFIED `Enum.Variant` — qualified all of them (Shape/Tree/Color groups) to match the implementation + prose; each edited snippet CLI-`check`ed clean. i64::MIN (`-9223372036854775808`) stays unparseable (lexer rejects the magnitude) — known limit, unchanged. Two-engine parity (VM==interp) green; `cargo test conformance` green.
- ✅ **`?` sum-type KIND soundness: a Result-`?` is rejected in an Option-returning fn (and vice versa)** (2026-06-27, `auto-task/try-kind-check`) — checker hole in `infer_try` (`src/checker/mod.rs`): the pre-computed `ret_err` collapsed both `Ty::Option(_)` and `Ty::Nil` enclosing returns to `None`, so (1) the `Ty::Result` operand arm SKIPPED its compatibility check whenever `ret_err==None` (a `Result`-`?` slipped through an `Option`-returning fn) and (2) the `Ty::Option` operand arm never inspected `current_ret` (an `Option`-`?` slipped through a `Result`-returning fn). The mistyped fn then returned the wrong sum-type and FAULTED a downstream exhaustive `match`/`??` at runtime ("no match arm for variant 'Err'/'None'") even though `check` passed. Fix (checker-only, no runtime/parity change — both engines inherit the stricter validation): dropped `ret_err`, folded a `current_ret` KIND match into each operand arm — `Result`-operand ⇒ enclosing must be `Result` (keeps the existing error-TYPE check) or `Nil`; `Option`-operand ⇒ enclosing must be `Option` or `Nil`; mismatched kinds get distinct errors ("'?' propagates a Result error, but the enclosing function returns Option, not Result" / "…returns Result, not Option"). `Nil` (top-level/`main`) still accepts either; `Unknown` enclosing (inferred-closure return) stays REJECTED. +4 checker tests (2 RED-first KIND-mismatch repros + 2 compatible-still-ok guards); existing error-TYPE + closure + recover guards stay green. Docs: `docs/syntax.md` §9 `?`-kind clarification.
- ✅ **Inferred struct/enum method return types now FLOW (soundness; closed an unchecked-struct-body hole too)** (2026-06-27, `auto-task/method-return-inference`) — checker bare-key vs module-key divergence: in the `build_graph`/`check_graph` path the struct layout is stored under `<module-key>::Name`, but `struct_self_ty` + `infer_returns_pass`'s struct branch + the pass-2 struct body-check guard all looked up `self.structs.get(name)` by the BARE name → misses. Three coupled defects, one root: (1) an un-annotated method's inferred return was written to a non-existent slot, so `s: str = P(3).val()` silently accepted `int` into `str` (and protocol satisfaction read `Unknown` — an inferred `compare→bool` wrongly satisfied `Comparable`); (2) `struct_self_ty` built `Ty::Struct(BARE, [])` corrupting `self`'s type; (3) the pass-2 guard missed → struct method **bodies were entirely UNCHECKED** in the entry path (`y: str = self.x` passed). Plus (4) `infer_returns_pass`/`count_uninferred` had no `Enum` arm → enum method returns never inferred. Fix (checker-only, parity-safe by construction — no opcode/runtime change): bare_key the three struct lookups + the `Ty::Struct` key (mirror the already-correct `enum_self_ty`), and add the `Enum` arm. Turning struct-body checking ON surfaced a pre-existing latent checker bug — the duplicate-binder pre-pass (`first_duplicate_binder`) counted a bare nullary-variant ident (`None`) as a binder, so `(None, None, None)` was falsely "bound more than once" (only ever hit inside struct method bodies, e.g. `examples/slicing.chz`); fixed by passing it the same variant-name predicate `bind_subpattern` uses. +11 checker tests (entry_rejects/entry_ok on build_graph path — the single-module `ok()`/`rejects()` helpers mask the bug). Newtype method-return inference left intentionally unfixed (out of scope; `newtype_self_ty` already key-correct) — known consistency follow-up. Docs: `docs/syntax.md` return-inference note (methods + protocol satisfaction).
- ✅ **Match-exhaustiveness soundness: guarded / refutable-payload variant arms no longer close a variant** (2026-06-27, `auto-task/match-exhaustiveness-guard`) — checker hole: `bind_match_arm` inserted a variant into the `covered` set UNCONDITIONALLY, ignoring (a) the arm's guard and (b) refutable payload sub-patterns (literals/ranges/nested-variants). So `E.A if false: …` / `Some(0): …` / `P.Pair(0, y): …` passed `chezzi check` then FAULTED at runtime ("no match arm for variant …"). Fix (checker-only, no runtime/parity change — both engines already fault identically): threaded `guarded: bool` into `bind_match_arm`, collected `payload_irref` from the existing `bind_subpattern` zip loop, and gated coverage on `!guarded && payload_irref`; duplicate-arm detection now keys on `covered.contains` (a PRIOR fully-closing arm) so the standard `E.A(n) if c` → `E.A(n)` guard-then-fallback idiom is ACCEPTED instead of wrongly rejected as "duplicate match arm". Tuple + int/str/bool literal scrutinees untouched (already conservative). +5 checker tests (4 RED-first repros + a duplicate-still-rejected regression guard). Docs: `docs/syntax.md` §8 refutable-payload clarification.
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
  the full `std.iter`/`std.cmp` receiver re-export half of #1. Docs: `docs/stdlib.md` str method table +
  `std.str` note, `docs/syntax.md` method cheat-sheet, `gaps.md` (#1 str half + #7 → resolved log).
- ✅ **Left-shift overflow now a recoverable fault** (2026-06-23, `auto-task/shift-overflow`) — `1 << 63`
  silently wrapped to `i64::MIN`, violating the "every i64 overflow is a recoverable fault" policy
  (the shift handler validated only the shift-*amount* range, never value overflow, unlike `+ - * / %`).
  Fix (both engines, `vm/mod.rs` `bitwise()` + `interp/mod.rs` `eval_binary` Shl arm): a left-shift-only
  round-trip check — `(a << b) >> b != a` ⇒ raise the shared `integer overflow in Shl`. Round-trip-safe
  shifts incl. `-1 << 63 == INT_MIN` still succeed; `>>` is unchanged (arithmetic, never overflows).
  Golden `examples/edge_cases.chz` `shift_ovf63` probe pins it on all three engines + a VM unit test
  guards the non-overflow regressions. Docs: `gaps.md` nit resolved, `docs/spec.md` overflow policy +
  `docs/syntax.md` shift note updated.
- ✅ **`list.map`/`.filter`/`.fold` OOB-on-shrink fixed** (2026-06-21, `auto-task/list-hof-shrink-oob`) —
  VM `list_hof` captured `n = v.len()` once then indexed the *live* heap list, so a callback that
  shrank the receiver (`xs.pop()`) ran a stale index past the now-shorter `Vec` → `index out of bounds`
  panic (vm/mod.rs:6840 map/filter, ~6890 fold) on both engines. Fix: allocate a **rooted snapshot**
  of the receiver's elements at call time and index that (mirrors `list_sort_by`; the interp already
  snapshots `elems` before dispatch, so this aligns the VM to interp). **Chosen semantics: snapshot** —
  map/filter/fold iterate the receiver's elements as of call time; a callback that shrinks **or** grows
  the receiver does not perturb iteration (consistent with comprehensions/`for`-loops/Python). Tests:
  `map`/`filter`/`fold`_shrinking_callback_no_panic + golden `examples/list_hof_shrink.chz` (VM==interp).
  Docs: `docs/stdlib.md` (snapshot note), `gaps.md` (entry → ✅ RESOLVED).

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
  `module.Variant` (dropping the enum) is NOT accepted. Docs: `docs/grammar.bnf` (+conformance green),
  `docs/syntax.md` match section.
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
  accept/reject fixtures. Docs: `docs/syntax.md` §3, `gaps.md` (RESOLVED), `docs/future.md` (item 12
  landed), `docs/concurrency.md`. `cargo test` green (2052+), `cargo test conformance` green, clippy clean.
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
  `docs/syntax.md` §3. `cargo test` green (2068), conformance green, clippy clean.
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
  under `--parallel` (`WireValue::Closure`), by handle on the cooperative oracle (parity).
- ✅ **B3.4/B3.5 — cancellation + cross-thread `os.exit` + thread deadlock detection** — per-nursery
  `cancel` flag (first fault/exit trips it; `os.exit` wins; cancel bypasses `recover:` but runs `defer`s).
  Single-level cancel only (nested propagation deferred).
- ✅ **B3.3 (a–d) — `str`-by-value + G1 module-globals checker gate + worker module-graph reconstruction +
  real OS threads behind `--parallel`** — mutating a `spawn`-reachable module global is a checker error
  ("use Shared[T]"); bounded pool, parent participates inline.
- ✅ **B3.0–B3.2 — `WireValue` airlock + cores into `Arc<…Core>` + `Arc<Program>` + isolated worker VMs**
  — `deep_clone` → wire round-trip; `Channel`/`Shared`/`Executor` cores out of the heap; cross-heap safety
  enforced (`ensure_crossable`). All single-thread, byte-identical. See `docs/concurrency-b3.md`.
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
- ✅ **M2.5 — canonical grammar + conformance** — `docs/grammar.bnf` executed via the `bnf` crate,
  differential-tested vs the parser. `cargo test conformance`.
- ✅ **M2 — parser → AST** — recursive descent + Pratt; spans; depth-capped.
- ✅ **M1 — lexer** — full `examples/hello.chz` incl. Indent/Dedent; string escapes, numeric underscores.
  Shipped follow-ups: scientific-notation floats (`1e3`/`1.5e-9`/`6.022e23` — any exponent ⇒ float;
  bare `e` not half-consumed), single-quote strings (`'…'` ≡ `"…"`, same escapes & interpolation),
  unicode `\u{HEX}` escapes (1-6 hex digits, rejects surrogates/>10FFFF/malformed). Golden:
  `examples/literals.chz` (VM + interp + `.expected`).

---

## Stdlib additions (post-M18, 2026-06-13)

Additive-only, two-engine-parity-clean library surface landed alongside the M19 perf freeze (the freeze
is on *language semantics/syntax*; these add functions without changing any existing behavior). Built in
3 parallel `auto-task` worktrees, merged A→B→C with a `post-merge-gate` pass (verdict **ship**; one
cross-task semantic merge conflict — a test-mock `Host` impl missing the new trait method — caught at
compile and fixed). All TDD'd; suite at **1630 green**.

- **`std.math`** — trig/exp/log intrinsics: `sin cos tan asin acos atan atan2 exp ln log2 log10 log`
  (native, `src/native/math.rs`; plain `Float` pass-through — domain errors yield NaN, no `Result`
  wrapping, matching the minimal additive design). Golden: `examples/math_more.chz`.
- **`std.str`** (pure-Chezzi, `std/str.chz`) — `ends_with index_of count replace strip_prefix
  strip_suffix`, built only on existing native str methods. Golden: `examples/str_more.chz`.
- **`std.iter`** (pure-Chezzi, `std/iter.chz`) — `take drop any all find flatten`, in the existing
  fiber-park-safe generic style. Golden: `examples/iter_more.chz`.
- **`std.request`** — non-GET/POST verbs `put`/`patch`/`delete`/`head` + a general
  `request(method, url, body, headers: Map[str,str])` for custom headers (`src/native/request.rs`).
  Required a cross-engine `Host::arg_str_map` and a new **`NativeArg::Map`** variant so the
  headers-carrying form stays in `is_blocking()` and offloads to the `--parallel` dirty pool without
  pinning a core worker. Two-engine parity locked by `request_verbs_and_headers_parity_against_local_server`.
- **Considered, not built:** `json.decode[T]` — already shipped (`src/json_decode.rs` + parser/compiler/
  checker); first-class compiled `Regex` — deferred, blocked on Level-3 Userdata (see `docs/spec.md`).

## Syntax ergonomics (post-M18, 2026-06-13)

Token/parser-level only — two-engine parity is by construction (both engines call `lexer::tokenize`
then `parser::parse`; interp untouched). TDD'd, conformance + clippy clean; suite at **1642 green**.

- **Multi-line collection literals** — the lexer gained a `bracket_depth` counter; while `>0` it
  suppresses layout (Indent/Dedent/Newline) so `[]`/`{}`/`()` literals, call args, and param lists
  can span lines (`src/lexer/mod.rs`). Stray closer clamps via `saturating_sub`; the suppressed-
  newline path always `advance()`s past `\n` and `continue 'scan`s (never recurses) so an unclosed
  bracket terminates at `Eof` — guarded by the `unclosed_bracket_terminates_at_eof` tripwire (a prior
  attempt OOM-killed the box by spinning the tokenize loop on malformed input; this is the invariant).
- **Optional trailing comma** — one trailing `,` before the closer on list/map/set/tuple literals +
  call arguments + fn/closure params (`[1,2,]` ≡ `[1,2]`; lone `[,]`/`(,)`/`f(,)` still error).
- **One-element tuples** — `(x,)` is now a 1-tuple (was rejected); `(x)` stays grouping. Flipped the
  `reject/one_element_tuple` corpus → `accept/`, added `accept/trailing_comma.chz`, and relaxed the
  `<primary>`/`<params>`/`<argList>` productions in `docs/grammar.bnf` (conformance green). Golden:
  `examples/multiline_literals.chz` (VM == interp == `--parallel`).

## QoL syntax batch (post-M18, 2026-06-14)

Four ergonomics features, each a vertical TDD slice through lexer→parser→checker→compiler/vm + interp,
VM == interp == `--parallel` on every registered example. Conformance + clippy clean; suite at **1902 green**.

- **`in` membership operator** — `x in xs` → `bool`: list/set element, map **KEY** (Python-style),
  str substring. `BinaryOp::In` at comparison precedence (level 7 == `==`); `for x in xs:` is
  unaffected (the parser consumes `in` explicitly there). New `Op::Contains` + `op_contains` helper
  (reuses `values_equal`/`hash_key_rooted`/`candidates` — the same machinery as `.has`/`.contains`);
  interp `eval_binary` scans linearly with `values_equal_guarded`. No user `Contains` overload.
  Example: `examples/membership.chz`.
- **Compound assignment** — `*= /= %= &= |= ^= <<= >>=` (joining `+= -=`), all desugaring to the
  existing binary ops via `AssignOp::to_binop()` (shared by compiler + interp). Arithmetic forms
  numeric (no int-slot widening — `int /= float` rejected); bitwise forms int-only. Works on var /
  index / field / map-value targets. (`//=`/`**=` excluded — no `//`/`**` base op yet.) Example:
  `examples/compound_assign.chz`.
- **Triple-quoted strings** — `"""…"""` / `'''…'''`, lexer-only. Same escapes + interpolation as a
  regular string; the only added power is unescaped quotes inside. Produces a normal `Token::Str`, so
  everything downstream is unchanged (parity by construction). Example: `examples/multiline_str.chz`.
- **Multi-target / tuple-swap assignment** — `a, b = b, a` (also `data[0], data[1] = …`, struct
  fields, and `a, b = f()` for a tuple-returning `f`). Parser collects a comma lvalue list before
  `=` (op `=` only — compound with multiple targets is a clean parse error); the full RHS is
  evaluated into a hidden temp FIRST (Python semantics — correct even when an index appears on both
  sides), mirroring the destructuring-binding lowering. Example: `examples/tuple_swap.chz`.

> One sharp edge found + fixed: adding the `Op::Contains` arm to the VM's `step` grew its frame just
> enough to trip `self_referential_stringable_hits_depth_limit` (infinite `str(self)` recursion must
> hit the 10_000 call-depth limit before exhausting the host stack). Dispatching with `return
> self.op_contains(span)` instead of `… ?` keeps `step`'s frame from materializing the extra
> `RuntimeError` temporary. Grammar (`<eqExpr>` + IN; `<assignStmt>` + 8 compound ops + tuple alt) and
> conformance corpus updated; `cargo test conformance` green.

## Roadmap (later)

- VM/GC optimizations beyond M19 — NaN-boxing (own milestone), register VM, generational/incremental GC,
  Cranelift AOT/JIT. Written up in [`docs/future.md`](docs/future.md).
- **Bug-discovery track (pre-JIT)** — automated bug finding. ✅ **CPython output-differential built**
  (`src/difftest/`, see Current focus). Remaining: cargo-fuzz parser (lever #1), Miri/sanitizers,
  proptest, metamorphic. Ranked plan + rationale in [`docs/bug-discovery.md`](docs/bug-discovery.md).
  Recommended to stand up Tier 1 before the JIT, so the reference semantics are fuzzed + differentially validated first.
- ~~**M-C — implicit nurseries**~~ — **shipped 2026-06-12** (see Concurrency above).

### Ideas — record-only (not scheduled)

- **Native FFI / Rust-library bindings** — let Chezzi call into Rust libs; design sketch in `docs/spec.md`
  → *Standard library* → "Future idea — native FFI". **Dynamic C-ABI FFI v1 has since shipped** (`extern
  "lib":` scalar calls via dlopen+libffi — see "Done" below; **plus opaque `ptr` handles, `char*`
  ownership (`owned_str`/`str?`), flat-scalar structs by value, and sync scalar callbacks — all
  shipped**); remaining surface (nested structs-by-value, `str` struct fields, stored/cross-thread
  callbacks + pointer-deref builtins, varargs, the rich Rust `Box<dyn Any>` userdata handle) is still
  deferred.

---

## Known friction / open (document-only)

Surfaced by coverage passes; no `src/` changes pending, recorded for when they bite:

- **Collection literals must be single-line** — a newline inside `[`/`{` ends the expression.
- **`match` limits** — no multiple `Some(...)` arms (one arm per outer variant; refine with `_`).
  Nested nullary-variant patterns (`Some(None)`, `Ok(Err(e))`) and **or-patterns** (`p1 | p2`) now
  work — see below.
- **Float arithmetic is total IEEE-754** (landed): float ops never fault — `1.0/0.0`→`inf`,
  `-1.0/0.0`→`-inf`, `0.0/0.0`/`5.0%0.0`→`NaN`, `math.sqrt(-1.0)`→`NaN`. `inf`/`NaN` are values;
  inspect with `math.is_nan`/`math.is_inf`/`math.is_finite`. **Integer** arithmetic still faults
  (overflow, `/0`, `%0`), and casting a non-finite float to `int` still faults. **Ordered
  comparisons involving `NaN` are total too** (landed): `< <= > >=` against a `NaN` always return
  `false` (never fault), matching IEEE-754 / Python / Rust; equality is unchanged (`nan == nan`→
  `false`, `nan != nan`→`true`). `sort()` and `sort_by_key` are **deterministic** with `NaN` keys —
  a total order (`f64::total_cmp`, `NaN` sorts to one end), never a fault.
- **`std.os.getcwd`** not yet injectable via `HostConfig` (parity holds); **`read_file`** capped at 64 MiB.

## Notes

- Recursive structs "just work" via the checker's two-pass name collection — trees and linked lists need
  only `Node?` child fields + a `match` per step, no special support.
