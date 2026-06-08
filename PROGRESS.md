# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

---

> **Mode:** Claude implements directly — working, tested code each session (see `CLAUDE.md`).

## Current focus

> **Gaps pass II — type-system + runtime depth.** ✅ DONE (TDD, full suite + conformance green —
> 1164 tests, both engines parity-tested, clippy clean). Five tractable `gaps.md` items closed;
> integers (#6: `byte`/bignum) deferred to its own milestone.
> - **`Ref[T]` mutable box** ✅ — pure-Chezzi `std/ref.chz`: `struct Ref[T]: value: T` with
>   `get`/`set`/`update`. No engine change (generic struct + self-mutation + fn-param call already
>   work). Types are program-global, so `import std.ref` makes `Ref` usable bare. `examples/ref.chz`.
> - **`sort_by_key`** ✅ — native list method `xs.sort_by_key(f: fn(T) -> K)`, sugar over `sort_by`.
>   `K` Comparable; keys computed once per element, compared by natural order (scalar / struct
>   `compare`); stable. Mirrors `sort_by`'s GC-rooted re-entrant merge sort in both engines (VM roots
>   a parallel keys list). Desugar `BUILTIN_METHODS` + checker `infer_list_hof` arm + interp
>   `eval_list_sort_by_key` + VM `list_sort_by_key`/`order_key`. `examples/sort_by_key.chz`.
> - **call fn-typed field `self.f(x)`** ✅ — `recv.f(x)` where `f: fn(T)->U` is a field now resolves
>   to field-access-then-call (on `self` + external receiver). Desugar `normalize_call` made
>   field-aware (a program-wide `fn`-field-name set skips method-default injection — the gaps.md
>   GOTCHA); checker `infer_method_call` falls back to a `Ty::Func` field; both engines fall back to
>   calling the field value. `examples/fn_field.chz`; `iter_adapters.chz` now uses `self.f(x)` direct.
>   Narrow limitation: a name used as both fn-field and defaulted method loses that method's
>   default-fill (pre-type receiver unknown).
> - **relax non-const defaults** ✅ — a default may be any expression not referencing another
>   param/field (`compute()`, `1+2`, `GLOBAL*2`). Parser dropped the const-literal check; desugar
>   `validate_defaults` rejects param/field-referencing defaults (cloned into caller scope at the
>   omitting call site). `examples/default_expr.chz`. Param-ref defaults (`y = x+1`) still out.
> - **runtime stack traces** ✅ — an uncaught fault prints the error line + the call chain (innermost
>   first) with each call's line; **identical on both engines**. VM captures from `self.frames` at the
>   uncaught fault before unwind (`fault_trace`, reset by `recover:`); interp keeps a `call_stack`
>   popped only on success (`recover:` truncates). `RunError` wraps `RuntimeError` + trace at the run
>   boundary (engine `RuntimeError` + parity `Display` unchanged). `examples/stack_trace.chz`.

> **Scripting-ergonomics gap pass.** ✅ DONE (TDD, full suite + conformance green — 1143 tests,
> both engines parity-tested, clippy clean). Five `gaps.md` items closed in sequence, each with a
> golden + parity example:
> - **Hex/binary/octal literals** (`0xFF`/`0b1010`/`0o17`) — lexer-only: `number()` detects the
>   prefix and parses via `i64::from_str_radix`, `_` allowed between digits. Token stays `Int(i64)`.
>   `examples/hex.chz`.
> - **List `.concat`/`.extend` + map `.merge`/`.update`** — method-based (no operator overload).
>   concat/merge return a NEW collection; extend/update mutate in place → nil. Checker sigs +
>   interp (`builtins`/`eval_map_method` + `map_upsert`) + VM `core_method` (`expect_list_obj`/
>   `map_upsert_in_heap`); new collections built fully before the single GC alloc; self-ops snapshot
>   the other side first. `examples/concat_merge.chz`.
> - **Tuple-destructuring `for`** (`for a, b in list[(A,B)]`, N-var over `list[tupleN]`; one var binds
>   the whole tuple) + `std/iter.chz` `enumerate`/`zip` (pure Chezzi). Checker `for_bindings` tuple
>   arm; interp `iter_rows_from_value` row-expansion; VM was type-erased so `compile_for`'s multivar
>   branch now splits at runtime on a new **`Op::IsMap`** (map keys/values lockstep vs list-of-tuples
>   `GetField(j)` destructure). `examples/for_tuple.chz`.
> - **Optional chaining `?.` + null-coalescing `??`** — lexer-adjacent `?.`/`??` tokens, parser
>   carrier nodes (`OptChain`/`NullCoalesce`, `??` right-assoc bp 4), lowered to a `match` on the
>   `Option` by the **desugar pass** → zero checker/engine semantic code (match + Some/None already
>   work). None short-circuits; `Some(v)` re-wraps (no auto-flatten → `Option[Option[U]]`).
>   `examples/optchain.chz`.
> - **Tuple destructuring (general) + match-on-tuple + guards** — verified already working
>   (`a, b := fn()`, typed tuple values, `match (a,b): (1,x) if …`, `Some((a,b))`); added
>   `examples/tuple_match.chz` as coverage.

> **Fix — loop variable is immutable (cross-engine divergence).** ✅ DONE (TDD, full suite +
> conformance green — 1094 tests). `for i in 0..3: i = i + 100` used to diverge: the **VM** mutated
> the live counter slot (one iteration), the **interp** advanced an internal counter (all three).
> The **checker** now rejects assignment (`=`/`+=`/`-=`) to any `for`-loop variable — they're fresh
> per-iteration bindings (Python/Rust), so the divergent program is caught at the type-check gate and
> never reaches either engine. Impl in `src/checker/mod.rs`: a `loop_vars: Vec<HashSet<String>>`
> mirroring `scopes`, `mark_loop_var` (set in `StmtKind::For`), `is_loop_var` (resolves to the
> binding's defining scope so an inner `:=` shadow stays mutable), and a guard in `check_assign`'s
> `Ident` arm. `declare` clears the mark on re-declare. Tests `for_*_reassign_rejected`,
> `for_body_local_var_still_assignable`, `loop_var_shadowed_in_inner_block_assignable`.

> **M18 — `defer` → block/lexical scope.** ✅ DONE (TDD, full suite + conformance green — 1084
> tests, both engines parity-tested). **Supersedes M17's frame-scoping.** A `defer` now runs when its
> **enclosing lexical block** exits (every indented block is a scope: function body, loop body,
> `if`/branch, `recover:`, statement-form `match` arm, and the module top level), on every path:
> fall-through, `break`/`continue`, return, `?`, panic — LIFO within a block, inner-block-first across
> nesting. Fixes the Go loop footgun and makes `recover:` cleanup local. Checker: dropped the
> `in_fn` "defer outside function" ban (`in_fn` flag removed) — top-level defer is legal; call-only
> target check kept. **VM**: new `Op::EnterDeferScope`/`LeaveDeferScope` + `CallFrame.defer_markers`
> bracket each defer-holding block (emitted only when the block statically contains a `defer`, so
> defer-free code is byte-identical); `break`/`continue` emit drains down to the loop-body scope
> (`FnComp.defer_scopes` count + `LoopCtx.defer_floor`). Return/`?`/panic keep the whole-frame LIFO
> drain (inner-first falls out for free). `recover:` boundary drains via `Handler.defer_len` on all
> three paths — Ok (`Op::DrainHandlerDefers`), genuine-fault catch, and `?`-short-circuit (`drain_frame_to`);
> a defer that faults mid-unwind supersedes. **Interp**: `exec_block` split into a per-block
> defer-scope wrapper (`block_has_defer` gate, finally-drain on every exit incl. the `Err`/`?` path)
> over `exec_block_inner`; the function body uses `exec_block_inner` (its defers stay on the per-call
> list drained by `finish_frame`); `eval_recover_body` got the same finally-drain (clearing
> `propagating` so a defer fault supersedes the `?` value). `std.os.exit` still bypasses every drain.
> `examples/defer.chz` gained a `block scope` section (golden, both engines).

> **M17 — `defer` (Go-style, frame-scoped).** ✅ DONE (TDD, full suite + conformance green —
> 1050 tests, both engines parity-tested). `defer <call>` runs a call when the enclosing
> function/method/closure **frame** exits — on every path: normal return, `?` short-circuit, panic —
> in **LIFO** order. Receiver + args are evaluated **at the `defer` statement** (Go); only the call
> is delayed. New `Token::Defer` + `StmtKind::Defer(Expr)` + `parse_defer` + grammar `<deferStmt>`
> (drift-checked). Checker: `in_fn` flag rejects top-level `defer`; the target must be a method call
> or a first-class-value call (built-ins/ctors must be wrapped — `lookup`/`functions` classify).
> A per-frame deferred list drains via the existing **re-entrant invoke** (`call_value`/`invoke_value`
> + method dispatch) — the same path `map`/`filter`/`sort_by` use. **Interp**: `Interp.deferred:
> Vec<Vec<Deferred>>`; `exec_defer` records (args eval'd now); teardown extracted to a non-inlined
> `finish_frame` (keeps `call`/`call_closure` recursion frames small) that drains LIFO, a deferred
> fault superseding the result. **VM**: `Op::DeferCall`/`DeferMethod` + `CallFrame.deferred`
> (GC-rooted in `collect`); drained in `do_return` (covers return + `?` via `do_try`) and on panic by
> `unwind_deferred` over the frames the handler-stack discards (defers run before `recover:` catches).
> `std.os.exit` skips defers (matches Go's `os.Exit`). Interp thread stack 256→384 MB so the
> `MAX_CALL_DEPTH` (10 000) guard still fires ahead of host-stack overflow with the slightly larger
> frames. `examples/defer.chz` (golden, both engines).

> **M16 — Comprehensions + `std.os.exit(code)`.** ✅ DONE (TDD, full suite + conformance green —
> 1041 tests, both engines parity-tested).
> **Comprehensions** `[elem for x in it if g]` (+ set `{e for …}` / map `{k: v for …}`): one `for`
> clause (binds one name, or two for `for k, v in m`) + optional `if` guard. A first-class
> `ExprKind::Comprehension { kind, key, elem, vars, iter, guard }` (`src/ast`) — *not* a parse-time
> desugar (closures are single-expr; `.map`/`.filter` are list-only, so neither reaches ranges,
> sets, maps, or struct iterators). Parser detects `for` after the first element (`src/parser`,
> `parse_comp_clause`); grammar `<compClause>` + corpus (drift-checked). Checker `infer_comprehension`
> reuses `for_bindings` (so every iterable binds like a `for` loop) + the set/map Hashable checks.
> Interp `eval_comprehension` shares iteration with `exec_for` via the extracted `iter_rows_from_value`.
> VM `compile_comprehension` reuses `compile_for` by synthesizing the accumulate body (`$comp.push/add`
> / `$comp[k]=v`) — zero duplicated iteration logic. `examples/comprehensions.chz` (golden, both engines).
> **`std.os.exit(code)`**: hard, uncatchable cooperative exit — `Host::request_exit` sets a pending
> code on each engine; the native returns an `Err` sentinel that unwinds past every `recover:` to the
> top, where the driver reports the code. Threaded through `RunOutput` (new 4th field) + the CLI
> (`ExitCode::from(code)`, clamped `0..=255`). `examples/exit.chz` (golden, both engines).

> **M15 — Slicing + the `Index`/`IndexSet`/`Slice` protocols.** ✅ DONE (TDD, full suite +
> conformance green — 1013 tests, both engines parity-tested). `xs[1..3]` / `s[0..2]` slice
> half-open + bounds-clamped, reusing the existing `..` range (no new lexer token). The earlier
> "hardcode list/str, defer the protocol" plan (`gaps.md`) was **reversed**: this landed the
> deliberate **pair** as prebuilt structural protocols — `Index[K, V]` (read `obj[k]` via
> `index(self, key) -> V`), `IndexSet[K, V]` (mutable `obj[k] = v` via `set_index`; requires `index`
> too), and `Slice[R]` (`obj[a..b]` via `slice(self, int, int) -> R`). Built-in `list`/`map`/`str`
> conform **intrinsically** (mirroring `Iterator[T]`; `str` is read-only — no `IndexSet`), user structs
> conform **structurally**. AST: new `ExprKind::Slice { obj, start, end }` (parser emits it when a `[…]`
> subscript is a `..` range; `src/ast`, `src/parser`). Checker (`src/checker/mod.rs`): the three
> protocols registered in `prebuilt_protocols` + `is_reserved_protocol`; intrinsic conformance in
> `satisfies_args`; helpers `index_kv`/`index_set_kv`/`slice_result`; `infer_index` + new `infer_slice`
> handle struct and bounded-`Ty::Param` receivers; `check_assign` dispatches struct `set_index`; and
> `recover_index_args` recovers `K`/`V`/`R` at call sites (the `Iterator[T]` recovery generalized — so
> `fn first[C: Index[int, V], V](c)` works over a list AND a struct). Engines dispatch by runtime
> operand kind (no type info threaded to the compiler), mirroring operator overloading: interp
> `eval_slice` + `call_struct_method`; VM `Op::GetSlice` + `dispatch_index_method`, with the sliced
> list's element handles GC-rooted across the alloc. `examples/slicing.chz` (golden, both engines).
> Deferred: omitted bounds (`xs[..n]`), inclusive `..=`, negative indices, and arg *recovery* for
> general user-defined parameterized protocols.

> **M14 — Method-level type parameters.** ✅ DONE (TDD, full suite + conformance green — 961 tests).
> A struct method may now introduce its **own** fresh type params `[U]` beyond the struct's `[T]`
> (`fn map_to[U](self, f: fn(T) -> U) -> U`). `U` is inferred from the call arguments, declared
> bounds are enforced, and `Iterator[T]` element recovery applies — the free generic-fn inference
> path (`infer_generic_call`) generalized into a sibling `infer_generic_method` invoked from the
> `Ty::Struct` arm of `infer_method_call` (`src/checker/mod.rs`). A method type param that reuses a
> struct param's name is rejected (`fn_sig`, "shadows"). Generics are type-erased, so **no engine
> change** — `examples/method_type_params.chz` runs byte-identical on VM + interp (parity test).

> **M14 — User-defined parameterized protocols (concrete-arg bounds).** ✅ DONE (TDD, full suite +
> conformance green — 969 tests). `protocol Container[T]:` now takes type parameters; a bound supplies
> concrete args (`fn first[X: Container[int]](c: X)`), and a struct satisfies it **structurally** with
> `T` substituted — generalizing the special-cased built-in `Iterator[T]`. AST: `Protocol.type_params`;
> `ProtocolInfo.type_params` (`src/checker/mod.rs`). Parser/grammar: `parse_protocol` consumes
> `parse_type_params()`; `<protocolDecl>` gained the `<typeParams>` alternative. Checker: `check_bounds`
> arity-checks against the protocol's param count (Iterator = 1, others by declaration); `satisfies_args`
> substitutes the protocol's params into each required method sig before structural matching;
> `enforce_bounds` resolves + forwards the bound's args; the bounded-param method-dispatch arm
> substitutes them so `c.get(0)` is `int`. Iterator keeps its intrinsic conformance + element recovery
> (user protocols take their args explicitly). A parameterized protocol is a **bound only** — using it
> as an existential value type (`c: Container[int]`) is rejected. Type-erased ⇒ **no engine change**;
> `examples/param_protocol.chz` parity-tested. Deferred (per scope): Iterator-style arg *recovery* for
> user protocols (`[S: Container[T], T]`), and parameterized protocols as value types.

> **M14 — Default + named args on methods.** ✅ DONE (TDD, full suite + conformance green — 978
> tests). Methods now accept constant-literal defaults and named call args, like free fns + struct
> ctors. Done entirely in the pre-type **desugar pass** (`src/desugar/mod.rs`) — the single point that
> reaches the checker and **both** engines (they each re-build the graph, so a checker-side rewrite
> wouldn't reach them). Since the desugar has no receiver type, it resolves a method call by name via a
> program-wide method registry (`collect_methods`): fills omitted defaults + reorders named args into a
> positional list, leaving the checker and engines untouched. Ambiguity guard: same-named methods on
> different structs with **different** params → a named call is rejected (pass positionally); built-in
> method names (`map`/`push`/`len`/… — `BUILTIN_METHODS`) are skipped so a user struct reusing one
> doesn't hijack a list/str/map/set call. Parser: methods parse with `allow_defaults=true`.
> `examples/method_default_args.chz` parity-tested. Deferred: non-constant default expressions;
> closures/enum-variant defaults.

> **Default + named arguments.** ✅ DONE (TDD, both engines in lockstep, full suite + conformance
> green — 935 tests). Free functions and struct constructors take constant-literal **defaults**
> (`fn f(x: int, y: int = 10)`, `port: int = 8080`) and **named call args** (`f(1, y=2)`,
> `Server("db", port=9000)`). Implemented as a scope-aware **desugar pass** in `resolver::build_graph`
> (`src/desugar/mod.rs`): it resolves each call's callee (own-module / `from`-import / `mod.f(...)`
> alias, never a shadowing local) and normalizes named/omitted args into a positional list, so the
> checker and **both** engines consume an identical, already-desugared AST — no new opcodes, no
> per-engine call-binding logic. AST: `Param.default`, `Field.default`, `Call.named` (always empty
> post-desugar). Parser: `name = expr` args (positional-before-named enforced), `= const` defaults
> (const-literal + trailing-default rules; rejected on closures/methods/protocols). Golden +
> parity: `examples/default_args.chz`, `examples/named_struct.chz`. Deferred: non-constant defaults.
> (Defaults/named on methods later shipped in M14; variadic args are a permanent non-goal — see
> `spec.md`.)

> **Tech-debt sweep — `gaps.md` "Known fragilities" cleared.** ✅ DONE. The three remaining open
> items landed TDD, both engines in lockstep, full suite + conformance green (874 tests).
> - ✅ **Dup generic type param `[T, T]`** rejected at parse — one check in `parse_type_params`
>   (covers `fn`/`struct`/`enum`); `duplicate type parameter '<name>'`. Test
>   `duplicate_type_param_rejected`.
> - ✅ **Nested `set` equality parity** — interp `SetData::eq` made order-independent (via
>   `values_equal`), mirroring the VM; a set nested in a struct/list now compares equal regardless of
>   insertion order on both engines. Golden `examples/set_eq.chz`, parity test
>   `nested_set_equality_parity`.
> - ✅ **Explicit call-site type args `name[T,…](…)`** — `ExprKind::Call.type_args`; parser
>   speculative steal on a bare-name callee (`try_parse_type_arg_call`, mirrors `.decode[T]()`;
>   `fns[0](x)` stays index+call); checker seeds the subst map (`seed_targs`/`name_is_generic`),
>   inference fills the rest. Type-erased runtime. Works for generic fns/structs/enum-variants.
>   Grammar + conformance updated. Golden `examples/explicit_type_args.chz`, 7 checker units + 2
>   parser units.

> **M11 — Tier 3: panic recovery + Go-style errors** (`gaps.md` Tier 3). 🟦 IN PROGRESS. Plan:
> `~/.claude/plans/see-past-commits-docs-fluttering-turing.md`. Staged A→B (errors first, then the
> recovery boundary), each TDD + commit.
>
> - ✅ **Phase A — Go-style `Result[T, E]`.** `Result` is now 2-param: `T!` = `Result[T, Error]`,
>   `T!E` = `Result[T, E]`. New built-in **`Error` protocol** (`message(self) -> str`); `str`
>   conforms intrinsically so `Err("…")` still works everywhere (no wrapper/builder needed). Added a
>   `Ty::Protocol(name)` existential (a protocol used as a value type, e.g. the default `Error`) +
>   a checker `assignable()` that does protocol conformance (the context-free `compatible` can't).
>   `?` now checks the propagated error type fits the enclosing function's `E`. Runtime is
>   type-erased (only new runtime: `str.message()`); both engines parity-checked. Migrated example
>   error-consumption sites (`"…" + e` / `e.trim()` → `e.message()`). Docs + grammar (`T!E`) +
>   conformance corpus updated. 845 tests green, clippy clean.
> - ✅ **Phase B — `recover:` boundary.** Block expression → `Result[T, Error]` that catches any
>   runtime fault (OOB, div0, overflow, missing key, …) occurring transitively beneath it — no
>   per-call wrapping. **try-block semantics:** a `?` inside short-circuits to the boundary (its Err
>   lands in `r`), so one `recover:` handles both panics and propagated errors, and `?` is allowed
>   even when the enclosing fn doesn't return a Result. Interp catches the unwind (snapshot/restore
>   locals + globals + call-depth, gate on the `?`-propagation channel); VM uses a handler stack +
>   `PushHandler`/`PopHandler` ops with the catch converging at a `done` label. `return`/`break`/
>   `continue` escaping a recover and `?`-on-Option inside one are rejected by the checker. New
>   `recover` keyword + AST/parser/grammar (`recoverExpr`) + conformance corpus. Golden
>   `examples/recover.chz`, parity across all cases. 857 tests green, clippy clean.

> **M10 — Tier 2: type-system depth** (`gaps.md` Tier 2). 🟦 IN PROGRESS. Plan:
> `~/.claude/plans/see-gaps-and-help-mellow-meteor.md`. Staged G1→G4 (ascending risk), each TDD +
> commit.
>
> **THIS SESSION (gaps follow-up, in order):** ✅ tech-debt (parser `MAX_DEPTH` 128→64, dropped the
> test stack-size crutch); ✅ **G4 — generic enums**; ✅ **map-model rework** — `map`/`set` are now
> real insertion-ordered **hash tables** (entries `Vec<(u64_hash, k[, v])>` + side index
> `HashMap<u64, Vec<usize>>`) and the key restriction is **lifted**: any `Hashable` type (int/str/bool
> or a struct with `hash(self) -> int`) is a key/element. Struct-key `hash()` re-enters (GC-rooted on
> the VM operand stack like `sort_by`; Rc-safe on interp); numeric keys hash by canonical f64 bits
> (±0.0 normalised); float keys stay rejected. Both engines byte-identical — parity + 2 gc-stress
> tests + golden `examples/hashmap_keys.chz`. Checker `is_hashable_key` → `satisfies(Hashable)`.
> Reviewed (Solidity + Godot S++ lenses): rooting/index-invariant/parity clean; applied the ±0.0
> hash-normalisation finding.
> - ✅ **G1 — `Stringable` protocol.** Prebuilt protocol `str(self) -> str`; a struct that defines it
>   overrides its default repr in `print`, the `str()` builtin, and `{…}` interpolation (nested in
>   list/tuple/map/set/enum too). Both engines via a new protocol-aware `stringify` (`&self`
>   `display` kept for error/debug text). Enums keep the built-in repr (no enum methods). Named
>   `Stringable` (not `Display`/`Show`) to match the `-able` convention + the `str()` builtin.
>   Self-referential `str` trips the call-depth guard on both engines (interp counts the dispatch so
>   it errors before host-stack overflow). Golden `examples/stringable.chz` + checker units + parity.
> - ✅ **G2 — `Hashable` protocol.** Prebuilt `Hashable` (`hash(self) -> int`) usable as a
>   `[T: Hashable]` bound; int/str/bool satisfy intrinsically, structs via a `hash` method. **Map/set
>   key restriction now LIFTED** by the map-model rework (see "THIS SESSION" above): `map`/`set` became
>   real hash tables and any `Hashable` type is a key/element.
> - ✅ **G3 — numeric operator protocols + multi-bound + type aliases.** Per-operator `Add`/`Sub`/
>   `Mul` (method `add`/`sub`/`mul`) overload `+`/`-`/`*` on same-typed structs (int/float intrinsic;
>   `/`/`%` never); both engines dispatch via `run_proto`/`call`. Multi-bound `T: Add + Mul`
>   (`TypeParam.bound`→`bounds: Vec`, refactored ~8 sites). Transparent type aliases `type UserId =
>   int` (new `type` keyword, `resolve_type` cycle guard, reserved/dup checks). Goldens
>   `examples/operators.chz` + `examples/type_alias.chz`, checker units, grammar + conformance.
>   (Also hardened the parser `deep_nesting` test onto a generous-stack thread — MAX_DEPTH=128 sat at
>   the 2 MiB test-thread edge.)
> - ✅ **G4 — generic enums.** `Ty::Enum(String)` → `Ty::Enum(String, Vec<Ty>)`; AST `Enum` gained
>   `type_params`; parser reuses `parse_type_params`; checker enters params over variant payloads,
>   infers args at variant construction via `unify` (mirrors generic structs), substitutes payloads
>   in `match` (`enum_param_map` + `subst`), enforces bounds. **Type-erased** — compiler/VM unchanged
>   (`StmtKind::Enum { .. }`). `Result`/`Option` stay special. Golden `examples/generic_enum.chz`
>   (Tree[T] at int+str, Either[A,B]) on both engines + parity; checker + parser units; grammar +
>   conformance updated; `docs/syntax.md` §8.
>
> **M9 — Tier-2 stdlib: `std.regex` + `std.request`** ✅ DONE. Plan:
> `~/.claude/plans/see-the-docs-and-dreamy-unicorn.md`.
> - ✅ **Seam** — `NativeRet::Struct`/`Map` added so native fns return structured values; lowered on
>   both engines (interp unit-tested; VM via parity).
> - ✅ **`std.regex`** (the `regex` crate) — stateless (the `Host` seam can't pass a compiled handle
>   back in), thread-local compile cache; `is_match`/`find`/`find_all`/`replace_all`/`split` →
>   `Result`, `Match {text,start,end,groups}` (byte offsets). Golden `examples/regex_demo.chz`.
> - ✅ **`std.request`** (`ureq` + rustls, blocking) — `get`/`post` → `Result[Response]`
>   (`{status, body, headers: map[str,str]}`); ≥400 is a normal Response, transport errors → Err.
>   Loopback-server integration + cross-engine parity tests; manual `examples/request_demo.chz`.
> - ✅ **Checker** — synthetic `Match`/`Response` structs seeded (program-global reserved names) +
>   `native_module_sig` for both modules.
> - First runtime deps (`regex`, `ureq`); language stays single-threaded/sync (concurrency deferred).
>
> **M8 — Tier-1 stdlib build-out** (`gaps.md` Tier 1). ✅ DONE. Plan:
> `~/.claude/plans/in-gaps-see-tier-nested-neumann.md`. Milestones:
> **M1 ✅** char Python-style — `s.chars() -> list[str]` + iterable strings (`for c in s:`);
> no `char` type (a char is a 1-char `str`, like Python). Golden `examples/string_iter.chz`,
> byte-identical both engines. · **M2 ✅** std.json layer A — pure-Chezzi `Json` enum +
> recursive-descent `parse` (unicode `\u`/surrogates, errors as `Err`) + `stringify` +
> accessors (`as_*`/`get`/`at`/`len`/`is_null`); golden `examples/json_dynamic.chz`
> byte-identical both engines. (Papercut: JSON literals in Chezzi *source* must double braces
> `{{ }}` — bare `{}` is string interpolation.) · **M3 ✅** native trio —
> std.process `cmd(s)->Result[str]` (Ok=stdout/Err=stderr via `sh -c`); std.fs
> `list_dir/exists/is_file/is_dir/size/glob` (glob = dependency-free `*`/`?` matcher, last
> component); std.time `now/monotonic/sleep_ms/format` (UTC date via Hinnant civil-from-days, no
> chrono). Zero engine/`Host`/`NativeRet` changes — fns call `std::` directly + existing
> lowering. Golden `examples/sys.chz`. ·
> **M4 ✅** set type — `{a, b, c}` literals (deduped, insertion-ordered; disambiguated from
> map by no `:`), `set()`/`set(list)`, methods `add/remove/has/len/union/intersection/
> difference`, `for x in s`, order-independent equality, `set[T]` annotation; elements are
> hashable scalars. `Ty::Set`/`Value::Set`/`Obj::Set` + `Op::NewSet`, GC-traced, both engines.
> Golden `examples/set.chz`. · **M6 ✅** docs — spec/syntax/grammar.bnf (set-literal +
> `.decode[T]` productions, drift-checked; corpus `set_literal.chz`/`decode_call.chz`), gaps.md
> Tier-1 ticked. · **M5 ✅** std.json `decode[T]` — type-directed decode into
> struct/typed-map/list/scalar via a scoped `ExprKind::DecodeCall` (parser special-cases
> `.decode[T](…)`, no general call-site type-args) + a self-contained `TypeDescriptor`
> (`src/json_decode.rs`) built at compile time (VM `Op::JsonDecode`) / eval time (interp); reuses
> the module's `parse` then coerces. Option fields ↔ null/absent, extra keys ignored, recursive/
> generic struct targets rejected. Plus dynamic `as_object`/`as_array`. Golden
> `examples/json_decode.chz`, byte-identical both engines. · **M6 ⬜** docs.

> **Language gaps round 2 (#10–#15): ✅ ALL DONE.** See the section below.

> **M7 — generics + structural protocols.** ✅ **G1 + G2 + G3 DONE.** Type-erased generics (all work
> in the checker; both runtimes barely change). **G1:** generic functions (`fn max[T: Comparable]`),
> Go-style structural `protocol`s, prebuilt `Comparable` wiring `< <= > >=` to a user `compare`
> method. **G2:** generic structs (`Pair[A, B]`, `Stack[T]`) — `Ty::Struct` carries type args,
> field/method types substitute the struct's params, type args inferred at construction or written
> explicitly. Goldens `examples/generics.chz` + `examples/generic_structs.chz` are byte-identical
> on interp + VM; grammar + conformance corpus updated.
> **G3:** stdlib unified onto the new system — `min`/`max`/`clamp` are now generic
> `[T: Comparable]` functions in a new pure-Chezzi **`std.cmp`** module (the old numeric-only native
> `std.math.min`/`max` + their `numeric_poly` hack removed; `abs` stays native), and `list.sort()`
> widened to any Comparable element (incl. structs, via each engine's `struct_compare` + a stable
> merge sort). Also fixed module-qualified generic calls (`cmp.max`). Golden
> `examples/stdlib_cmp.chz` byte-identical on both engines.

> **M6 — stdlib + pipe `|>` + core-type methods.** ✅ **M6a + M6b + M6c DONE.** The Level-2 native
> FFI seam (`NativeFn` + `Host` trait) was scheduled and built: each binding is written once and
> runs on both engines. Ships `std.math`/`std.io`/`std.os` (native) and `std.str` (pure Chezzi).

## Round 2 — language gaps #10–#15  ✅ DONE

A second probing pass (real DSA + apps) surfaced six gaps; all fixed **TDD**, both engines in
lockstep, each with a golden `examples/*.chz` run under interp + VM. **646 tests green** (incl.
parity + `cargo test conformance`), clean `cargo clippy`. Details + fix notes in `gaps.md`.

- ✅ **#11 `sort_by`** — `xs.sort_by(fn(T,T)->int)`, stable, in place. A merge sort drives the
  fallible/re-entrant comparator (not `slice::sort_by`); the VM permutes `usize` indices with the
  source list GC-rooted (gc-stress tested). `examples/sort_by.chz`.
- ✅ **#10 `ord`/`chr`** — two builtins in the `len`/`range` lockstep tables (interp + compiler + vm
  + checker). `examples/cipher.chz` (ROT13 + digit parsing).
- ✅ **#12 int+float math** — `abs`/`min`/`max` numeric-polymorphic (int→int, float→float, mixed
  rejected) via `Host::arg_is_int` + checker `ModuleSig::numeric_poly`. `examples/knapsack.chz`.
- ✅ **#14 map `for`** — `For.vars: Vec<String>`; `for k in m` (key) and `for k, v in m` (entry).
  VM normalises the iterand (list→clone, map→keys) via `ListClone`. `examples/word_freq.chz`.
- ✅ **#15 nested/tuple match** — recursive `Pattern` (`Tuple`/`Ident` + `Vec<Pattern>` bindings) +
  `MatchKind::Tuple`; recursive lowering reuses `MatchArm`/`GetField`/`Eq` (no new opcodes).
  `examples/match_nested.chz`.
- ✅ **#13 bitwise** — `& | ^ << >>` (int-only) across lexer→parser→checker→both engines +
  `grammar.bnf`; Python precedence; shift-out-of-range is a runtime error (no panic).
  `examples/bits.chz`.
- ✅ **iterator protocol** (Tier 3) — a user struct with `next(self) -> Option[T]` is iterable in
  `for x in s`, binding `x: T`, looping lazily (`next()` called per step, so infinite iterators with
  an early `break` terminate). Structural detection in the checker (no formal generic `Iterator[T]`);
  the type-erased VM branches at runtime via the new `Op::IsStruct` opcode, so both engines agree.
  No grammar change (only the iterand's allowed type widened). `examples/iterator.chz`.
- ✅ **`Iterator[T]` protocol** (M13) — the language's first **parameterized** protocol bound. A
  generic fn `[S: Iterator[T], T]` accepts any iterable — built-in `list`/`set`/`str`/`map`
  intrinsically (like `int` satisfies `Add`), or a struct via its `next` — and **recovers the element
  type** `T` (unified from the iterand's element) into loop vars and return types. Bounds now carry
  type args (`ast::Bound { name, args }`, parser + `grammar.bnf <bound>` rule). Element recovery is
  shared across free-fn / struct-ctor / enum-variant call sites (`recover_iter_elems`/`enforce_bounds`),
  and `inner.next()` on a bounded param yields `Option[T]` so **lazy adapter structs** (Take/Mapped
  over an infinite source) compose with **no `yield`** — the Rust `std::iter` model. Checker + parser +
  grammar only; both engines unchanged and parity-tested. **`yield`/generators dropped as a non-goal.**
  `examples/iterator_bound.chz`, `examples/iter_adapters.chz`.
- ✅ **match guards + range patterns** (extends #15) — `pattern if cond:` (optional bool guard on
  both `MatchArm`/`MatchExprArm`; a guarded arm is never irrefutable, so it can't make a match
  exhaustive) and half-open int `start..end` patterns (`start <= v < end`, int-only, refutable).
  No new token/opcode: parser reuses `If`/`DotDot`, engines reuse `JumpIfFalse` + `GtEq`/`Lt`. A
  bare top-level identifier on a literal scrutinee is now a value-binding catch-all (disambiguated
  from nullary variants via the program-global variant registry / runtime enum check).
  `examples/match_guard.chz`, `examples/match_range.chz`.

**Deferred** (recorded in `gaps.md`): generics / operator-overloading trait (extends #12), a real
`char` type (extends #10), `sort_by_key` (sugar on #11).

> **`std.os.exit(code)` — shipped (post-M14).** Hard, uncatchable cooperative exit: an exit-code
> channel is threaded through both run drivers (`RunOutput` 4th field) + the CLI. Unwinds past
> `recover:`; the process exits with `code` (clamped `0..=255`). `std.io.read_file` is capped at 64 MiB (returns `Err`, no OOM).
> `std.os.getcwd` reads the real cwd (not injectable via `HostConfig` yet — parity holds, documented).
> Level-3 dynamic `cdylib`/C-ABI FFI remains out of scope per the spec.

> **Entry-model change (post-M6, semantics fix).** Removed the auto-call of `main()` — `main` is now
> an ordinary function; programs run top-to-bottom and call it themselves (scripting-language model;
> future `chezzi.toml` `entrypoint` is a note only). An unhandled `Err`/`None` at the top level (a
> bare expression statement, or a top-level `?`) now exits with `unhandled error: …` on **both**
> engines (was: silently dropped for `main`'s `?`). VM gained `Op::PopExprStmt`. All examples/tests
> migrated to explicit `main()` calls.

## Post-M6 — Tuples + multiple return + destructuring (gap #8)  ✅ DONE

Tuples on **both** engines: literal `(e1, e2, …)` (≥2 elements), tuple type `(T1, T2, …)` in type
position, tuple-return functions, destructuring let `a, b := expr`, and `.0`/`.1`/… element access.
Immutable + fixed-arity; shared by `Rc<Vec<Value>>` (interp) / `Obj::Tuple` (VM). Built **TDD**
(failing test first per layer). **No new tokens** — reuses `(` `)` `,` `.`.

- ✅ **Disambiguation** — expression `(e)` stays grouping, `(e1, e2, …)` is a tuple, `(e,)` is a
  parse error (`"1-element tuples are not supported"`), `()` unchanged. Type `(T)` unwraps to `T`,
  `(T1, …)` is a tuple type. `t.0` lexes as `Ident · Dot · Int(0)`; the postfix `Dot` handler now
  accepts an `Int` (decimal-string field name) → reuses `ExprKind::Field`.
- ✅ **AST** — `Type::Tuple` + `ExprKind::Tuple`; `StmtKind::Let.name: String` → `names: Vec<String>`
  (single binding = `vec![name]`; len>1 only on the destructuring path). Ripple updated at every
  match/construct site (parser ×2 construct + ×2 test, checker hoist + check, interp exec, compiler).
- ✅ **Parser** — `parse_primary` LParen arm (grouping vs tuple vs 1-tuple error); `parse_type`
  leading-LParen branch (unwrap-1 / tuple-≥2, `?`/`!` postfix still applies); destructuring let in
  `parse_simple_stmt` (peek `Ident Comma`); grammar `<primary>`/`<type>`/`<letStmt>`+`<identList>` +
  `<postfix> DOT INT`; corpus `accept/{tuple_literal,tuple_return,destructure_let}.chz`,
  `reject/one_element_tuple.chz`.
- ✅ **Checker** — `Ty::Tuple` (+ `compatible` element-wise, `Display` `(a, b)`); `resolve_type`,
  `infer` (Tuple), `infer_field` tuple-index arm (out-of-range/non-numeric → error); `check_destructure`
  (arity match, `Unknown` permissive, non-tuple/arity-mismatch errors).
- ✅ **Interp/VM** — `Value::Tuple`/`Obj::Tuple` (`type_name` `"tuple"`, `Display` `(a, b)` identical);
  eval `Tuple` + `Field` index + destructuring `Let`; `Op::NewTuple`, tuple-aware `GetField`
  (compiler emits `GetLocal(hidden)`+`GetField("i")` per binding — no new index op), `values_equal`
  element-wise. **`Heap::children` traces tuple elements** (gc-stress parity proves it).
- ✅ **Tests** — parser units (8), checker `ok`/`rejects` (8), interp `run` (4), cross-engine parity
  (7: literal/print, element access, OOB, destructure, equality, multi-return-destructure, heap-elements
  gc-stress), golden `examples/pair.chz` byte-identical on both engines. **569 total** (from 542),
  clean `cargo clippy` + `cargo test conformance`.

## M6a — Core-type methods (str / list)  ✅ DONE

Built-in methods on `str` and `list` dispatch on the value in **both** backends + the checker, with
a golden + parity suite. Built **TDD** (red→green per bug class; checker/interp tests bite-verified
to fail before the handlers existed). No new opcode — the existing `CallMethod(name, argc)` carries
everything; methods dispatch on the receiver at runtime.

- ✅ **Method set** — str: `len/upper/lower/trim`, `split(sep)→list[str]`, `join(list[str])→str`
  (separator-bound: `",".join(xs)`), `starts_with(s)/contains(s)→bool`. list: `push(x)→nil`
  (mutates in place), `len()→int`. `len()` stays a free builtin too (additive).
- ✅ **Interp** (`src/interp/builtins.rs` `call_method` + hook in `eval_method_call`) — str methods
  build new `Rc<str>`; `split` builds `Rc<RefCell<Vec>>`; `push` does `borrow_mut().push`.
- ✅ **VM** (`src/vm/mod.rs` `core_method`, dispatched **before** the clone-match so `push` mutates
  the heap list in place via `get_mut`). Multi-alloc `split` is safe — the GC only collects at
  instruction boundaries (M5b), never mid-opcode. Error strings mirror the interp **exactly**.
- ✅ **Checker** (`infer_method_call` + `str_method_sig`/`list_method_sig`) — `split→list[str]`,
  `join` param `list[str]`, element types checked (`xs.push("x")` on `list[int]` rejected). Unknown
  method / wrong arity / method-on-int all rejected with clear messages.
- ✅ **Tests** — checker `ok`/`rejects` (16 cases), interp `run` asserts + error cases, VM parity
  programs (str/list/chained/errors) in `parity_full_suite`, golden `examples/methods.chz` +
  `.expected` run byte-identical on both engines.

## M6b — Pipe operator `|>`  ✅ DONE

`a |> f(x)` desugars **at parse time** to `f(a, x)` — threading the left value as the first arg. So
the checker / interp / VM need **zero** pipe-specific code (they see a plain call). Built **TDD**.

- ✅ **Parser** — `InfixOp::Pipe`, lowest binding power (level 0, left-assoc) in `infix_op`; the
  `parse_bp` arm requires the RHS to be a `Call` and prepends the LHS to its args. Non-call RHS
  (`5 |> 7`, `5 |> f`) → `ParseError "right side of '|>' must be a function call"`.
- ✅ **Grammar + conformance** — `docs/grammar.bnf` gains a `<pipeExpr>`/`<pipeCall>` layer at the
  loosest precedence (RHS must end in a call); PIPE is no longer a reserved/unused token.
  `cargo test conformance` differential-tests grammar↔parser accept/reject; corpus files
  `accept/pipe_chain.chz` + `reject/pipe_noncall.chz` added.
- ✅ **Tests** — parser unit tests (desugar shape, arg-prepend, left-assoc chain, looser-than-`+`,
  non-call rejects); pipe programs in the VM `parity_full_suite`.

## M6c — Standard library + native FFI seam  ✅ DONE

The Level-2 native FFI seam — the mechanism that exposes a Rust function as a callable Chezzi value,
written once and run on **both** engines. Built **TDD** (red→green per cycle). CPython-built-in-C
model; Level-3 dynamic `cdylib` loading stays out of scope.

- ✅ **Seam** (`src/native/`) — `Host` trait (engine-agnostic arg access + stdout/stderr/stdin +
  args/env/cwd) + `NativeFn` (`fn(&mut dyn Host) -> Result<NativeRet, HostError>`) + `NativeRet`
  (engine-neutral return, lowered to each engine's `Value` **after** the call, so native code never
  touches `Rc`/`GcRef` — GC-safe by construction). `HostConfig`/`Stdin` carry args/env/stdin.
- ✅ **Both engines** — `Value::Native`/`Obj::Native`, `call_native` + `InterpHost`/`VmHost`
  adapters + `lower_native`, native-module injection in `eval_module`/`run_module`. VM
  `Obj::Native` has no GC children; the stress test `native_returned_heap_values_survive_gc_stress`
  guards the invariant.
- ✅ **Resolver** — native `std.*` (`is`/`native_name`) resolves to a **virtual** module (synthetic
  `<native:std.math>` id, no `.chz` file); `std.str` stays file-backed. `LoadedModule.native`.
- ✅ **Checker** — `native_module_sig` (the third lockstep table) injects static signatures; `from`
  imports + `m.fn()` type-check with zero new logic. Math params are `float` (no implicit int→float).
- ✅ **Modules** — `std.math` (abs/min/max/floor/ceil/round/pow/sqrt + `pi`/`e`), `std.io`
  (print/eprint/read_line/read_file/write_file), `std.os` (args/env/getcwd) — all native; `std.str`
  (repeat/reverse/pad_left/is_empty/split_lines) — **pure Chezzi** (`std/str.chz`).
- ✅ **CLI** — `chezzi run f.chz a b` passes program args; env + real stdin wired via
  `HostConfig::from_process`; `io.eprint` flushes to real stderr.
- ✅ **Tests** — per-module parity (interp == VM on stdout *and* stderr *and* error), checker sig
  tests, resolver virtual-module test, GC stress, `examples/std_demo.chz` golden, conformance. 401
  total, clean `cargo clippy`.
- ✅ **`std.os.exit(code)`** (shipped post-M14): hard, uncatchable exit threaded through both run
  drivers + CLI. ⏸️ Still deferred: `read_file` capped at 64 MiB; `getcwd` not yet injectable via `HostConfig`.

## Post-M6 — `map[K, V]` dictionary (gap #5)  ✅ DONE

Insertion-ordered maps on **both** engines: literal `{"a": 1}` / empty `{}`, keyed read/insert/update
`m[k]` / `m[k] = v` (missing-key read & compound-on-missing → runtime error), and methods
`len`/`has`/`get`/`keys`/`values`/`remove` (`get`/`remove` → `Option[V]`). Keys restricted to hashable
scalars (int/str/bool); float & composite keys rejected in the checker. Representation:
insertion-ordered `Vec<(Value, Value)>` (interp `Rc<RefCell<…>>`, VM `Obj::Map`), linear scan by
value-equality — deterministic iteration. Built **TDD** (red→green per layer).

- ✅ **Lexer/grammar** — new `Token::LBrace`/`RBrace` (no brace tokens existed, so `{}` is
  unambiguous); conformance `symbol()` + `docs/grammar.bnf` `<mapEntries>` production +
  `accept/map_literal.chz`.
- ✅ **AST/parser** — `ExprKind::Map(Vec<(Expr, Expr)>)`; `parse_primary` `{` arm (no trailing comma).
- ✅ **Checker** — `Ty::Map(K, V)`; `resolve_type` lowers `map[K,V]` + validates hashable key;
  `infer_map` (homogeneous keys/values, hashable key, empty → `map[?,?]`); `infer_index` /
  `check_assign` Index arms treat map keys as the key type (not int); `map_method_sig`.
- ✅ **Interp/VM** — `Value::Map` / `Obj::Map`; `ExprKind::Map`/`Op::NewMap` (last-key-wins upsert);
  Index read & `exec_assign`/`set_index` restructured to branch on map vs list/str; `core_method`
  /`map_method` for the six methods; `Display` = `{k: v, …}` (bare elements, mirrors list).
- ✅ **AsInt relocation** — removed `Op::AsInt` before `GetIndex`/`SetIndex` (it only validated int,
  wrongly rejecting str/bool map keys); int-validation moved into the VM `get_index`/`set_index`
  list/str arms with the **exact** `"expected int, found …"` message. Both engines now report the
  index int-error at the index-expression span (parity-tested); **zero regression** in the
  pre-existing list-index suite (OOB, compound-OOB side-effect skip, str-index-reject all green).
- ✅ **GC** — `Heap::children` traces **both** map keys and values; gc-stress parity test with heap
  keys+values proves no use-after-free.
- ✅ **Tests** — checker `ok`/`rejects` (18), parser + lexer units, 11 cross-engine parity tests
  (literal/print, read, missing-key error, insert/update, compound, all six methods, keys-iteration,
  int/bool keys, gc-stress) + a list non-int-index regression guard, golden `examples/map.chz` +
  `.expected` byte-identical on both engines. **542 total**, clean `cargo clippy` + `cargo test conformance`.

## Post-M6 — Index & field assignment (mutability)  ✅ DONE

`xs[i] = v` and `p.x = v` (plus `+=`/`-=`) now mutate in place on **both** engines — the two
highest-leverage gaps from `gaps.md` (#1, #2), which unblock in-place array algorithms (counting
sort, DP, sieve) and stateful objects. **Reverses the M4 decision** that rejected field/index
assignment "to match the interpreter" — now that both engines support it, the checker allows it.
Built **TDD** (red→green per layer; checker/interp/VM tests bite-verified to fail first).

- ✅ **Front-end was the only blocker** — the parser already accepted `Field`/`Index` lvalues and
  `docs/grammar.bnf`'s `<lvalue>` rule already permitted them; no grammar change needed.
- ✅ **Checker** (`check_assign`) — `Index` target requires a `list` (str index-assign rejected —
  strings are immutable); `Field` target requires a struct **data** field (methods/module members
  rejected); element/field type checked via the existing `check_assign_value`.
- ✅ **Interp** (`exec_assign`) — mutates in place through the existing `Rc<RefCell<…>>` (list
  elements, struct fields); each subexpression evaluated once; bounds/missing-field errors mirror
  the read path.
- ✅ **VM** — four new ops in `src/vm/op.rs` (`SetIndex`, `SetField`, `Dup`, `Dup2`); `compile_assign`
  emits them (`Dup`/`Dup2` give compound `+=`/`-=` a read-modify-write with no double-eval);
  `set_index`/`set_field` mutate via `heap.get_mut`. Error strings byte-match the interp.
- ✅ **Tests** — checker `ok`/`rejects` (9 cases), interp `run` asserts + OOB error, VM unit +
  cross-engine parity (incl. OOB), conformance corpus (`accept/index_assign.chz`,
  `accept/field_assign.chz`), golden `examples/mutate.chz` + `.expected` byte-identical on both
  engines. 433 total, clean `cargo clippy`.

## M5a — Bytecode compiler + stack VM (handle values, no collector yet)  ✅ DONE

`cargo run -- run --vm <file>` runs on the bytecode VM (tree-walk interp stays the default).
Golden parity holds: VM stdout == `hello.expected` and == the interpreter's output, and the
multi-file `tests/fixtures/proj/` runs identically. Built `src/compiler/` (AST → `Program`) +
`src/vm/` (`value` handle + `heap`-addressed `Obj` + `op` + exec loop); 48 VM tests, 261 total;
clean `cargo clippy --all-targets`. Built **TDD** (red→green per bug class).

- ✅ **Value model** — `Value` is `Copy` (unboxed `Int/Float/Bool/Nil` + `Obj(GcRef)`); the 6
  reference kinds (Str/List/Struct/Enum/Func/Closure/Module) live in a VM-owned `Heap` of slots +
  free-list (handle copies alias one object → by-reference sharing). No `RefCell`; `alloc` only
  inserts (mark-sweep lands in M5b).
- ✅ **Compiler** (`src/compiler/mod.rs`) — locals → operand-stack slots resolved at compile time;
  globals/struct/variant/builtins resolved by name in the interpreter's order. Two passes (hoist
  types → compile toplevel + fn/method/closure protos). Closures **snapshot all visible locals by
  value** (matches the interp's frame snapshot — reassign-after-capture invisible) via `CapEntry`/
  `GetCaptured`. String interpolation pre-parsed at compile time into literal/expr chunks.
- ✅ **Bytecode** (`src/vm/op.rs`) — flat `Vec<Op>` of typed operands (jumps = absolute indices),
  each op paired with a `Span` in `Proto::lines` so runtime errors recover source locations.
  Covers every AST node: literals, all 14 binary ops (runtime type dispatch matching the interp:
  checked int / float-promote / div-mod-by-zero error / str+str), `and`/`or` short-circuit,
  unary, list/struct/enum construction + arity errors, field/index (+ out-of-bounds, str char
  index), method calls (`self`-bound), closures, `?` (unwrap / propagate-to-caller / top-level
  error), `for`-range (**lazy** counting loop) + `for`-list (cloned), `while`, `if`/`elif`/`else`,
  `match` (variant dispatch + payload binding + no-arm error), `MAX_CALL_DEPTH` guard, builtins.
- ✅ **CLI** — `--vm` flag wired in `cmd_run` after the unchanged type-check gate; `vm::run_file`
  mirrors `interp::run_file` (256MB thread, resolver graph, run-once dep order, home-globals,
  entry-only `main()`, partial-output-before-error).

## M5b — Mark-sweep garbage collector  ✅ DONE

Hand-built tracing GC in `src/vm/heap.rs` + `Vm::collect`; 8 GC tests, 269 total; clean
`cargo clippy --all-targets`. Built **TDD** — each test forces a collection and pins one root
source; the headline operand-stack-root test was **bite-verified** to fail (7 dangling-handle
panics) when the root is removed.

- ✅ **Collector** — worklist mark (no native recursion) + sweep + free-list slot reuse. The heap
  owns slot/mark/sweep primitives + the allocation-driven growth threshold (`next_gc = 2×live`,
  min 256); the VM owns root tracing.
- ✅ **Collect at instruction boundaries** — `run_until` collects at the top of each loop step
  (or before *every* step in stress mode), where the entire live set is reachable from the roots,
  so there are **no mid-opcode off-stack temporaries** to miss (the build-then-alloc sequences in
  `NewList`/`NewStruct`/`NewEnum`/`ListClone`/`MakeClosure` complete within one un-interrupted step).
- ✅ **Root set** — the whole operand stack (covers every frame's local slots **and** in-flight
  expression temporaries), each frame's `home` module + backing `closure`, and the module
  namespace cache (`module_objs`). Children traced: list items, struct fields, enum payloads,
  closure captures + home, func home, module globals.
- ✅ **Guarded bug classes** — value live only on the operand stack / in a frame slot / via module
  globals / via a closure capture / propagated by `?` all survive collection; an allocation-heavy
  loop stays bounded (`<2000` live after 10k allocating iterations) instead of growing
  monotonically; `hello.chz` + a struct/enum/closure/match program are byte-identical under GC
  stress vs. normal.

## M5c — Module parity + perf + CLI default flip  ✅ DONE

`chezzi run` defaults to the VM; `--interp` runs the reference tree-walker. 6 parity tests, 275
total; clean `cargo clippy --all-targets`.

- ✅ **Cross-engine parity** — `parity_full_suite_vm_vs_interp` runs 16 programs (every feature
  class + 5 error cases) through **both** engines and asserts identical `(stdout, error)`. Golden
  `hello.chz` + the multi-file `proj/` run identically via `vm::run_file`; the project is also
  byte-identical under GC stress.
- ✅ **Home-globals on the VM (M4.5 headline bug)** — `imported_fn_uses_home_globals`: a new
  `tests/fixtures/homeglobals/` where `main` defines `MSG := "from-main"` and imports `who` from
  `lib` (which has `MSG := "from-lib"`). `who()` resolves `MSG` against **its own** module
  (`from-lib`) — both engines agree. Multi-file run-once / dep-order / `import as` / `from` all
  carry over.
- ✅ **Perf** — refactored the dispatch loop to **borrow** each instruction (one `Rc` bump per
  `run_until`, no per-op `clone`) — the single biggest win. Measured release speedup over the
  interpreter: **~6.5×** on an arithmetic loop, **~4.3×** on recursive `fib`. (Short of the ~10×
  aspiration: at ~1.7 ns/op the safe match-dispatch VM is near its floor and the tree-walker is
  itself fast; closing the gap needs inline caching / unsafe dispatch — deferred.) `bench_vm_…`
  records the ratio and asserts a debug-safe floor.
- ✅ **CLI flip** — `cmd_run` defaults to `vm::run_file`; `--interp` selects the tree-walker;
  `--vm` still accepted. `USAGE` updated.

**Interpolation parse-error timing (documented divergence):** the VM pre-parses `{expr}` chunks at
compile time, so a *malformed* interpolation in dead code is a load error rather than a
reached-only runtime error. Any program that runs successfully on either engine produces identical
stdout — this only differs on already-broken input.

## M1 — Lexer  ✅ DONE

All 5 guiding tests green; lexes full `examples/hello.chz` (nested Indent/Dedent, `0..10`→DotDot, `?`).

- ✅ **1a. Char cursor**
- ✅ **1b. Operator tokens**
- ✅ **1c. Numbers** — int + float; `_` digit separators (`10_000_000`, only between digits).
- ✅ **1d. Strings** — `"..."` with escapes `\n \t \r \\ \" \0` (unknown escape → error).
- ✅ **1e. Identifiers & keywords**
- ✅ **1f. Comments & whitespace**
- ✅ **1g. Newlines**
- ✅ **1h. Indentation** — indent stack + pending-Dedent queue in `scan_indentation`.
- ✅ **1i. EOF** — Newline → trailing Dedents → Eof
- ✅ **1j/1k. Tests green.**

**Open follow-ups (small, do anytime):** scientific notation in numbers (`1e3`); single-quote strings; unicode `\u{…}` escapes.
**Done post-M1:** string escapes (`\n \t \r \\ \" \0`), numeric underscores (`10_000_000`) — both lexer-only, TDD, conformance still green.

## M2 — Parser → AST  ✅ DONE

`cargo run -- ast examples/hello.chz` prints the full `{:#?}` tree; 25 tests green (incl. golden hello.chz); clean `cargo clippy`.

- ✅ **Spans retrofitted into the lexer** — `tokenize` now emits `Tok { kind, span }` with 1-based `Span { line, col }`. `tokens` CLI output unchanged (prints `kind`). AST nodes + `ParseError` carry spans.
- ✅ **AST** (`src/ast/`) — `Module`/`Stmt`/`Expr` (kind+span), decls (`FnDecl`/`Field`/`Variant`/`MatchArm`/`Pattern`/`Import`), `Type`, op enums. All `Debug`.
- ✅ **Parser** (`src/parser/`) — recursive descent (statements) + Pratt (expressions, binding powers per syntax.md §4). Shared block rule handles indented + inline (one-line `match` arms). Covers fn/struct/enum/match, if/else-if/else, for/while, return, all 4 import forms, closures, ranges, calls/field/index/`?`, list literals.
- ✅ **CLI** — `chezzi ast <file>` wired (lex → parse → pretty-print).

**Hardened after agent-review-panel** (2 passes + cold pass; 42 tests):
- Non-lvalue assignment (`1 = 2`, `f() = 3`) → `ParseError`, not a wrong AST.
- Statement terminator enforced — `x := 5 y := 6` on one line is an error.
- Recursion depth cap (`MAX_DEPTH = 128`) on all 4 recursive entry points (`parse_bp`, `parse_unary`, `parse_type`, `parse_stmt`) → deep nesting returns a `ParseError` instead of SIGABRT.
- Inline-block bodies allow `else` chaining but reject a nested compound statement (`if a: if b: …`) to avoid dangling-`else` ambiguity — nest via indentation.
- Error messages render tokens in source form (`':='`, not `Walrus`).

**Deferred (unchanged):** map literals `{...}` (no brace tokens), pipe `|>` (M6), string-interpolation parsing.
**Deferred nit (rationale):** the comma-separated-list loop recurs in ~6 spots; left inline — a generic `parse_separated` helper adds `FnMut` borrow friction and the call sites differ (some consume the closing delimiter, some don't) for marginal gain.

## M2.5 — Canonical grammar + conformance  ✅ DONE

Canonical grammar file with an executable drift check; 48 tests total.

- ✅ **`docs/grammar.bnf`** — the canonical grammar, BNF over the lexer **token stream** (terminals are token classes, so `INDENT`/`DEDENT`/`NEWLINE` are expressible). Mirrors the `parse_*` rules incl. the M2 hardening (lvalue restriction, statement terminator, inline-block-no-compound, precedence cascade).
- ✅ **Executable differential test** — `docs/grammar.bnf` is run with the [`bnf`](https://docs.rs/bnf) crate (Earley parser, **dev-dependency only** — not in the shipped binary; release build stays zero-dep). For every corpus file, grammar accept/reject must equal the hand parser's. Fed one private-use char per token (since `bnf` matches char-by-char).
- ✅ **Conformance corpus** — `tests/corpus/{accept,reject}/*.chz` (18 + 7), annotated `# rule:` / `# expect:`, doubling as executable docs.
- ✅ **Cross-checks** — grammar terminals == `Token` enum (only `PIPE` reserved/unused); grammar rules ↔ `parse_*` fns; `symbol()` is an exhaustive match (compiler-enforced completeness); every headline rule has a corpus example; reject messages are specific.
- ✅ **Bite-tested** — verified the harness actually fails on grammar drift, a bad corpus file, and a bogus token.
- Run: `cargo test conformance`. Excluded by design: deep-nesting (a parser depth cap, not a grammar rule).

## M3 — Tree-walk interpreter  ✅ DONE

`cargo run -- run examples/hello.chz` executes the program end-to-end. Built `src/interp/`
(`mod` + `value` + `env` + `builtins`); 70 interp tests, 118 total; clean `cargo clippy`.
Built with **TDD** (red→green per feature; every test targets a real bug class).

- ✅ **Values** (`value.rs`) — `Int/Float/Bool/Str/List/Func/Closure/Struct/Enum/Nil`. Reference
  types share via `Rc<RefCell<…>>`. Deterministic `Display` (struct fields in declaration order).
  Result/Option are plain `Enum`s (`Ok/Err/Some/None` pre-registered).
- ✅ **Env** (`env.rs`) — lexical scoping: `globals: Rc<HashMap>` + a stack of local frames; a call
  `swap_locals` to a fresh frame so a callee never sees the caller's locals. Closures snapshot
  captured frames.
- ✅ **Eval/exec** (`mod.rs`) — full expr + stmt set: arithmetic (int/int→int trunc, float promotion,
  checked overflow, div/mod-by-zero error for **both** int and float), `and`/`or` short-circuit,
  comparisons/equality, list literals + indexing, ranges (lazy in `for`), unary, calls, field
  access, method calls (`self`-bound), closures, `if`/`for`/`while`/`match`, `return` (via `Flow`),
  string interpolation (`{expr}`, `{{`/`}}`).
- ✅ **`?` operator** — value-level early return via a `propagating` channel caught at the call
  boundary; unwraps `Ok`/`Some`, propagates `Err`/`None` from the enclosing fn.
- ✅ **Builtins** (`builtins.rs`) — `print`, `len`, `range` (length-capped), `int`/`float`/`str`
  casts (range-checked), `sqrt`. `sqrt`/casts are temporary builtins until `std.math` (M4.5).
- ✅ **Entry point** — hoist top-level `fn`/`struct`/`enum`, run top-level stmts, auto-call nullary
  `main()`. CLI `chezzi run <file>` wired; prints partial output before a runtime error.
- ✅ **Robustness** (review-panel hardened, warm + cold pass) — interpreter runs on a dedicated
  256 MB-stack thread with a `MAX_CALL_DEPTH` guard (infinite recursion → clean error, not SIGABRT);
  no reachable panics on adversarial input; lazy ranges; accurate error spans.

**Deferred (unchanged):** maps `{...}`, pipe `|>` (M6), break/continue (no AST nodes), core-type
methods (`s.upper()`, `xs.push()` — only user struct methods so far). Exhaustiveness of `match` is
a runtime error now; static check arrives with M4. `?` inside a closure is absorbed at the closure
boundary (a checker rule for M4).

## M4 — Type checker (local inference)  ✅ DONE

`cargo run -- check examples/hello.chz` type-checks; `run` now **gates** on the checker (type
errors block execution — no partial output). Built `src/checker/` (`mod` + `ty`); 73 checker
tests, 191 total; clean `cargo clippy`. Built **TDD** (red→green per error class; every test pins
a real bug class).

- ✅ **Type lattice** (`ty.rs`) — `Ty`: `Int/Float/Bool/Str/Nil`, `List[T]`, `Result[T]`,
  `Option[T]`, `Struct/Enum(name)`, `Func{params,ret}`, and `Unknown` (top/bottom element,
  compatible with everything — suppresses error cascades). `compatible()` is structural; **no**
  implicit int→float (numeric promotion lives only in arithmetic).
- ✅ **Pragmatic local inference** — bidirectional, no unification. `:=` infers from RHS; typed
  `let`/params/fields/returns checked against annotations. `Ok/Some` carry their payload type;
  `Err`/`None` are generic (`Result[?]`/`Option[?]`) so they unify with any declared `Result[T]`.
- ✅ **Return-type inference (post-M4)** — a function/method that omits `-> T` infers its return
  type from the body's `return`s (pass-1.5 `infer_returns`/`infer_fn_ret`, run after `hoist`):
  first concrete return wins, conflicts are a real error, no value-return → `nil`. Single pass in
  source order, no fixpoint — a call to a *later* un-annotated fn (or a self-recursive call with no
  concrete base) infers `Unknown` and stays permissive (define callees first / annotate for a
  precise type). Param types still required. Runtime-free (engines never read the declared return).
- ✅ **`T?` / `T!` type shorthand (post-M4)** — in any type position, `T?` desugars to `Option[T]`
  and `T!` to `Result[T]` (parse-time, in `parse_type`; new `Token::Bang`). Stacks left-to-right;
  pure sugar — checker/engines/`Some·None·Ok·Err`/`match`/`?` unchanged.
- ✅ **Expression-valued `match` / `if` (post-M4)** — `x := match s: …` (multiline, exhaustive,
  value-expression arms) and `x := if c: a else: b` (inline, `else` required) yield a value.
  New `ExprKind::Match`/`IfElse`; checker unifies arm/branch types (shared `match_variants` /
  `bind_match_arm` / `check_exhaustive` helpers); both engines emit value-producing forms (parity
  tested). Statement `match`/`if` unchanged; loops + fn bodies stay statements with `return`.
- ✅ **Two-pass** — pass 1 hoists every top-level decl (forward refs work, like the interp);
  pass 2 walks bodies, **collecting all errors** (Go-style) into a `Vec`.
- ✅ **Error classes** (each with a test) — unknown name/type, call arity, non-callable, arithmetic
  (`+`/`-`/`*`/`/`/`%`, matching interp incl. `str+str`), comparison, bool context, assignment
  mismatch (typed-let, `=`/`+=`/`-=`), return-vs-signature, field access, indexing, match
  exhaustiveness (+ unknown variant, dup arm, binding arity), and the `?` operator (operand must
  be Result/Option; enclosing fn ret must be Result/Option/**Nil** — the last allows `?` in
  `main()`, matching interp's top-level unwind).
- ✅ **CLI** — `chezzi check <file>` + `run` gating; `--errors=json` emits a structured JSON array
  (hand-rolled, zero-dep escaper) and preserves the contract even on fatal lex/parse errors.
- ✅ **Robustness** (review-panel: 4 S++ warm + 1 cold pass) — redeclaration guard
  (dup fn/struct/enum/variant → clear error, no pass-2 panic); field/index assignment rejected to
  match the interpreter (which only assigns bare vars); closure body checked against its explicit
  return annotation; unknown CLI flags fail; no reachable panic on valid parsed input.

**Deferred (note):** `map[K,V]` typing (no map literals yet), all-paths-return analysis, deeper
generic unification, user-defined generics, `?`-inside-closure frame semantics, field/index
assignment (blocked until the interpreter supports it), pipe `|>` (M6).

## M4.5 — Modules / imports + resolver  ✅ DONE

Multi-file programs run; `chezzi.toml` root detection works. Built `src/resolver/`; 22 new tests
(7 resolver + 11 interp + 4 checker), 213 total; clean `cargo clippy --all-targets`. Built **TDD**
(red→green per bug class; the headline cross-module-globals test was bite-verified to fail without
the fix). Imports already lexed/parsed since M2 — M4.5 made them *mean* something.

- ✅ **Resolver** (`src/resolver/mod.rs`) — `find_root` (walk up for `chezzi.toml`, else entry's
  dir), `std_root` (`$CHEZZI_STD` else compile-time `<crate>/std`), `build_graph` (DFS, postorder
  load order = deps before dependents, entry last). Module identity = canonicalized abs path
  (`ModuleId`) → diamonds de-dupe, run-once parse. Cycles → clean `ResolveError` (`a -> b -> a`),
  not a stack overflow. `a.b.c` → `<root>/a/b/c.chz`; `std.*` → `<std_root>/…`. Lex/parse errors in
  an imported file are re-labelled (`in module 'core.db': …`) since `Span` carries no filename.
- ✅ **Interp** (`Value::Module` + `ModEnv`) — `module.fn()` is a plain call on a looked-up member
  (no `self`); `import a as m` / `import f, g from a` bind into the importer's scope. **Run-once**:
  each module's body (incl. top-level statements) evaluates exactly once in dependency order, its
  globals snapshotted as a cached namespace; `main()` auto-runs **only** for the entry file.
- ✅ **Cross-module globals (the subtle bug)** — a fn imported from B that reads B's top-level `K`
  must resolve `K` against **B**, not the caller. Fixed by bundling each callable's home globals
  into the value (`Value::Func(decl, ModEnv)`, `Closure.home`) and `Env::swap_globals` on every
  call — mirrors the existing `swap_locals` idiom. `ModEnv` is a `Rc<RefCell<HashMap>>` newtype
  with pointer-eq / opaque `Debug` (the table is self-referential — a deep compare/print would
  recurse forever).
- ✅ **Checker** (`check_graph`, `Ty::Module`, `ModuleSig`) — type-checks the whole graph,
  accumulating errors across modules (Go-style). `io.read()` resolves member sigs; `from` imports
  validate the member exists; imported-module errors carry the `in module '…'` label. Type names
  are program-global in M4.5 → reuse across modules is an "already defined" collision (also a
  runtime backstop in the interp hoist).
- ✅ **CLI** — `check`/`run` are path-aware (`build_graph` → `check_graph`, gate, then `run_file`);
  resolve/cycle/missing-module failures are `Fatal`, preserving the `--errors=json` contract.
- ✅ **Golden** — `tests/fixtures/proj/` (`chezzi.toml` + whole-module + `from` imports + a
  cross-module constant) runs to `main.expected` via the real entry point.

**Deferred (note):** actual `std/` content (M6 — `std.*` *resolves* but the dir is empty by design,
the std-path test asserts the path not a load); per-module type-name namespacing (program-global +
collision-detected for now); next-to-binary std discovery / install story; re-export / transitive
`from`; VM parity (M5 must mirror run-once + home-globals — golden tests will enforce).

## Roadmap (later)

- ✅ **M5** — Bytecode VM + mark-sweep GC (M5a compiler+VM; M5b GC; M5c parity+perf+CLI flip)
- 🟦 **M6** — Stdlib + pipe `|>` + **core-type methods**.
  - ✅ **M6a** — core-type str/list methods on both backends + checker + golden/parity.
  - ✅ **M6b** — pipe `|>` (parse-time desugar to a call; grammar + conformance updated).
  - ✅ **M6c** — stdlib via the Level-2 native FFI seam (`NativeFn` + `Host`): `std.math`/`std.io`/
    `std.os` native, `std.str` pure Chezzi. Written once, runs on both engines. (`std.os.exit`,
    `getcwd` cwd-injection, and Level-3 dynamic FFI deferred — see the M6c section above.)

### Ideas — NOT scheduled (record-only)

- **Future directions brainstorm** — `defer`, a shared-nothing (BEAM-style) concurrency **+
  parallelism** model (`spawn`/`parallel:`/`chan[T]`, per-task heap+GC, move/copy messaging),
  missing scripting features (
  …; default + named args, `Iterator[T]`, slicing, and comprehensions now shipped — `yield`/generators and variadics are
  permanent non-goals),
  and VM/GC optimizations (superinstructions, inline caching, NaN-boxing, …) are written up in
  **[`docs/future.md`](docs/future.md)**. Opinionated + speculative; promote into `gaps.md` when
  scheduled.
- **Native FFI / Rust-library bindings** — let Chezzi call into Rust libs (bootstrap the ecosystem
  instead of rewriting everything in Chezzi). Design sketch lives in `docs/spec.md` → *Standard
  library* → "Future idea — native FFI": `NativeFn` value + a `Host` trait (write a binding once,
  works on interp + VM) + opaque userdata; default build stays **zero third-party crates**, crate
  bindings behind Cargo features; dynamic `cdylib` plugins deferred. **Deliberately excluded from
  M6 and the current roadmap** — do not start without an explicit decision to schedule it.

---

## Gaps — round 2 (open, document-only)

Second probing pass: wrote real DSA + apps to stress the post-round-1 language. Five new programs
run green and byte-identical on **both** engines — `examples/bst.chz`, `linked_list.chz`,
`knapsack.chz`, `calc.chz`, `word_freq.chz` (+ `.expected` goldens) — and surfaced six new gaps,
written up in `gaps.md` (#10–#15) with quoted, observed errors. Headlines: **#10 no char access**
(no `ord`/`chr`; `s[i]` is a 1-char str, not a codepoint) and **#11 no `sort_by`** are real-app
blockers; #12 (int `min`/`max`/`abs`), #13 (bitwise ops), #14 (map iteration), #15 (nested match
patterns) are friction. Confirmed working 🟢: recursive/self-referential structs (tree + linked
list), mutable `self` across method calls, nested-list DP, empty-map `K,V` inference. No `src/`
changes this pass — surfacing only; full 569-test suite still green.

## Gaps — round 3 (open, document-only)

Coverage pass: gave every deterministic orphaned example a **golden** test (exact output + parity,
not just engine-parity) and added three comprehensive multi-feature programs, all green + byte-
identical on both engines:

- `examples/edge_cases.chz` — arithmetic faults under `recover:`, int/float boundaries, empty/nested
  collection printing, slice clamping, index faults, truthiness, block-scoped shadowing, closure
  capture-by-value, defer LIFO, comprehensions.
- `examples/evaluator.chz` — tokenizer + recursive-descent parser + AST evaluator with `Result`/`?`
  error paths (bad char, unbalanced parens, trailing input, divide-by-zero).
- `examples/ledger.chz` — map of mutable structs, overdraft `Result`s, `defer`, `sort_by`, guarded
  comprehensions.

Promoted to golden (were parity-only): `method_default_args`, `method_type_params`, `param_protocol`;
filled `.expected` for `hof`, `list_hof`, `list_methods`, `loops`, `match_value`, `pair`.
`request_demo.chz` stays manual-only (real network → non-deterministic).

Confirmed working 🟢: toward-zero integer division (`-7/2 == -3`, `-7%3 == -1`), block-scoped
shadowing, closure value-snapshot semantics, mutating a struct reached via `map.get(...)`, `sort_by`
on a list of structs, `defer` running on the `?` short-circuit path. Friction surfaced (no `src/`
changes): **collection literals must be single-line** (a newline inside `[`/`{` ends the expression);
a `match` cannot have multiple `Some(...)` arms nor nested nullary-variant patterns (must nest a
second `match`); **float division by zero is a runtime fault**, not an IEEE `Inf`/`NaN`. Full suite
(1071 tests) + conformance green, clippy clean.

## Notes

- Recursive structs "just work" via the checker's two-pass name collection — trees and linked lists
  need no special support, only `Node?` child fields + a `match` per step.
- The two loudest missing features for everyday code: `sort_by` (ranking / priority queues /
  Dijkstra) and `ord`/`chr` (any char-level parsing or cipher). Both are small additions — the HOF
  `invoke_value` plumbing and the native-seam dispatch already exist.
