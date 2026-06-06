# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

---

> **Mode:** Claude implements directly — working, tested code each session (see `CLAUDE.md`).

## Current focus

> **M10 — Tier 2: type-system depth** (`gaps.md` Tier 2). 🟦 IN PROGRESS. Plan:
> `~/.claude/plans/see-gaps-and-help-mellow-meteor.md`. Staged G1→G4 (ascending risk), each TDD +
> commit.
>
> **THIS SESSION (gaps follow-up, in order):** ✅ tech-debt (parser `MAX_DEPTH` 128→64, dropped the
> test stack-size crutch); ✅ **G4 — generic enums**; 🟦 **map-model rework** (real hash tables +
> Hashable struct keys) — in progress.
> - ✅ **G1 — `Stringable` protocol.** Prebuilt protocol `str(self) -> str`; a struct that defines it
>   overrides its default repr in `print`, the `str()` builtin, and `{…}` interpolation (nested in
>   list/tuple/map/set/enum too). Both engines via a new protocol-aware `stringify` (`&self`
>   `display` kept for error/debug text). Enums keep the built-in repr (no enum methods). Named
>   `Stringable` (not `Display`/`Show`) to match the `-able` convention + the `str()` builtin.
>   Self-referential `str` trips the call-depth guard on both engines (interp counts the dispatch so
>   it errors before host-stack overflow). Golden `examples/stringable.chz` + checker units + parity.
> - 🟦 **G2 — `Hashable` protocol (bound only).** Prebuilt `Hashable` (`hash(self) -> int`) usable as
>   a `[T: Hashable]` bound; int/str/bool satisfy intrinsically, structs via a `hash` method. **Map/set
>   key restriction NOT lifted** — found `map`/`set` are association lists (`Vec<(K,V)>`, linear scan +
>   structural `==`, no hashing), so enabling struct keys is entangled with a real-hashmap decision;
>   deferred to a dedicated map-model session (user's call). Checker units only (no runtime change).
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

**Deferred** (recorded in `gaps.md`): generics / operator-overloading trait (extends #12), match
guards + range patterns (extend #15), a real `char` type (extends #10), `sort_by_key` (sugar on #11).

> **Deferred within M6c (small, intentional):** `std.os.exit(code)` — a correct cooperative exit
> needs an exit-code channel threaded through both run drivers + the CLI; deferred to avoid a
> misleading half-implementation. `std.io.read_file` is capped at 64 MiB (returns `Err`, no OOM).
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
- ⏸️ **Deferred (intentional):** `std.os.exit(code)` (needs an exit-code channel through both run
  drivers + CLI); `read_file` capped at 64 MiB; `getcwd` not yet injectable via `HostConfig`.

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

## Notes

- Recursive structs "just work" via the checker's two-pass name collection — trees and linked lists
  need no special support, only `Node?` child fields + a `match` per step.
- The two loudest missing features for everyday code: `sort_by` (ranking / priority queues /
  Dijkstra) and `ord`/`chr` (any char-level parsing or cipher). Both are small additions — the HOF
  `invoke_value` plumbing and the native-seam dispatch already exist.
