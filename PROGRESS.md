# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

> **Mode:** Claude implements directly — working, tested code each session (see `CLAUDE.md`).
> Full per-milestone detail lives in git history; this file is a forward-looking tracker, not a changelog.

---

## Current focus

**✅ Enum methods (mirrors the struct-method machinery end-to-end).** Enums now accept `fn name(self, …)`
method blocks after their variants, parsed via the same `parse_fn(true)` path structs use; the parser
enforces variants-before-methods. (`test fn` is **rejected** in enum bodies — enum test *suites* are not
wired in the compiler/test-runner, so a `test fn` would silently never run; rejected at parse time as a
follow-up. A `Hashable` enum's `hash(self)` is dispatched at runtime in both engines, so `set[E]`/`map[E,V]`
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
flatten path. **Out of scope (deferred):** `derive`, nominal `newtype`, and the multi-bound same-name-method
ambiguity diagnostic (a pre-existing struct-era wart, first-bound-wins).

**✅ Module-scoped user types (struct / enum / `type` alias).** Types are now **private to their
declaring module**, mirroring how top-level functions are namespaced — exported by default (no `pub`),
visible elsewhere ONLY via import. `import core.geo` → `geo.Point(1,2)` / `x: geo.Point` /
`list[geo.Point]` / `geo.Color.Red`; `import Point from core.geo` → bare `Point(1,2)` (rename allowed
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
  generator, `next`-struct): returns SELF (idempotent). `list(xs.iter())`/`set(...)` drain for free.
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
  `.iter()` (the fast path stays); cursor `.reset()`/`.peek()`/`.rev()`/`size_hint`.
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
  bare-fn entry). NOTE: string-interpolation operands are NOT nil-checked — the checker treats `Str` as
  opaque (interpolation contents unparsed), so that one listed site is out of scope here. All cargo
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

**✅ Built-in conversions — str ↔ bytes (UTF-8) methods + `list()`/`set()`/`map()` constructors
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
- **`list(it)` / `set(it)` / `map(it)` constructors over ANY for-iterable** (NOT the narrow
  `Iterator[T]` protocol). Element types resolve through the checker's **`iter_elem`** — the single
  source of truth for "what `for x in X` accepts" — so `list([1,2])`, `list(myset)`, `list(b"hi")`,
  `list("ab")`, `list(range(3))`, `list(bytearray(..))`, and `list(myUserIterator)` all typecheck with no
  new protocol bound. `list(it) -> list[T]`; `set(it) -> set[T]` (the EXISTING `set` broadened from
  list-only to any for-iterable, keeping the 0-arg empty-set form + the `Hashable` gate); `map(it) ->
  map[K, V]` where the element is **exactly a 2-tuple** `(K, V)` (a non-2-tuple is a **static** checker
  error), `K` `Hashable`, last-wins on dup keys (like the `{k: v}` literal). `list`/`map` are NEW reserved
  builtin names (added to `is_reserved_name` + both `is_builtin` sites + per-engine dispatch). The
  argument is **required** — an empty `list`/`map` is the `[]`/`{}` literal, so `list()`/`map()` are
  checker errors pointing there. `map(pairs)` (free call) and `xs.map(f)` (list HOF method) are separate
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
  bytearray|list[int])`, `==`/`!=` structural (incl. cross-type `bytes == bytearray` content-equal,
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
- **Cyclic-data depth guard + order-independent map `==`.** Two fuzzing-found bugs: a cyclic struct made
  `print`/`==` recurse unbounded on the host stack (uncatchable SIGABRT, even inside `recover:`); and map
  `==` was order-dependent while set `==` was order-independent. Fix: `MAX_STRUCTURAL_DEPTH = 10_000`
  threaded through display + a `values_equal_guarded` (the public `values_equal -> bool` stays a thin
  wrapper, so the ~66 hash-probe call sites are untouched); the recoverable depth-exceeded error surfaces
  only at the `==`/`!=` op sites. Map `==` is now order-independent value equality. (Interp's *call*-depth
  overflow in **debug** builds is left as-is — the tree-walk engine is slated for removal; release + VM
  are fine.)
- **`defer:` block form** — `defer` takes an indented block as well as a single call (multi-action cleanup
  without N `defer` lines), mirroring `spawn`'s dual form with no new VM op. Body runs top-to-bottom at
  scope exit, LIFO as a unit, free vars snapshot by value at the `defer` point, runs on all exit paths.
  A dedicated `defer_floors` write-gate rejects reassigning an enclosing local inside the block (no
  `SetCaptured` op); a `?` short-circuit inside the block is absorbed on both engines.

---

## Concurrency — feature-complete (confirmed 2026-06-12)

Core implemented through **M18** (still evolving); **concurrency shipped through Tier-D (D0–D6c) + M-C**. The surface —
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
nested structs / `str` struct fields / **callbacks (#4)** / **varargs (#5)** (both with design notes +
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
  + minimal: NOT a general user type-export mechanism; `ptr`/`owned_str` stay bare builtins. Five new
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
  elements, struct fields, and destructuring lets; a `ref`-over-generic-param is a type error. Concurrency:
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
- ✅ **`map[K, V]` dictionary (gap #5)** — literals, keyed read/insert/update, six methods, GC-traced.
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
  `request(method, url, body, headers: map[str,str])` for custom headers (`src/native/request.rs`).
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
  sides), mirroring the destructuring-`let` lowering. Example: `examples/tuple_swap.chz`.

> One sharp edge found + fixed: adding the `Op::Contains` arm to the VM's `step` grew its frame just
> enough to trip `self_referential_stringable_hits_depth_limit` (infinite `str(self)` recursion must
> hit the 10_000 call-depth limit before exhausting the host stack). Dispatching with `return
> self.op_contains(span)` instead of `… ?` keeps `step`'s frame from materializing the extra
> `RuntimeError` temporary. Grammar (`<eqExpr>` + IN; `<assignStmt>` + 8 compound ops + tuple alt) and
> conformance corpus updated; `cargo test conformance` green.

## Roadmap (later)

- VM/GC optimizations beyond M19 — NaN-boxing (own milestone), register VM, generational/incremental GC,
  Cranelift AOT/JIT. Written up in [`docs/future.md`](docs/future.md).
- ~~**M-C — implicit nurseries**~~ — **shipped 2026-06-12** (see Concurrency above).

### Ideas — record-only (not scheduled)

- **Native FFI / Rust-library bindings** — let Chezzi call into Rust libs; design sketch in `docs/spec.md`
  → *Standard library* → "Future idea — native FFI". **Dynamic C-ABI FFI v1 has since shipped** (`extern
  "lib":` scalar calls via dlopen+libffi — see "Done" below); remaining surface (structs-by-value,
  callbacks, varargs, opaque pointers / userdata, `char*` ownership) is still deferred.

---

## Known friction / open (document-only)

Surfaced by coverage passes; no `src/` changes pending, recorded for when they bite:

- **Collection literals must be single-line** — a newline inside `[`/`{` ends the expression.
- **`match` limits** — no multiple `Some(...)` arms (one arm per outer variant; refine with `_`).
  Nested nullary-variant patterns (`Some(None)`, `Ok(Err(e))`) and **or-patterns** (`p1 | p2`) now
  work — see below.
- **Float division by zero is a runtime fault**, not an IEEE `Inf`/`NaN`.
- **`std.os.getcwd`** not yet injectable via `HostConfig` (parity holds); **`read_file`** capped at 64 MiB.

## Notes

- Recursive structs "just work" via the checker's two-pass name collection — trees and linked lists need
  only `Node?` child fields + a `match` per step, no special support.
