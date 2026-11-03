# Chezzi — Engineering Lessons

Rules for people who work on the compiler and runtime, distilled from the project's working memory
(2026-06 to 2026-10). **Read the section for the area you touch before you touch it**: most of these
mistakes shipped fully green the first time. Each rule has one line on the incident behind it. The
full incident text is in [`history/bug-hunts/lessons-incidents.md`](../history/bug-hunts/lessons-incidents.md).
Live bug rows are in [`gaps.md`](gaps.md); the hunt strategy is in [`bug-discovery.md`](bug-discovery.md).

## 1. Design north star

**Chezzi is a magpie language. Drift from the owning ancestor is a bug, not a design choice.**

| surface | ancestor | examples |
|---|---|---|
| syntax, scripting feel, `Executor` ergonomics | Python | indentation, interpolation, slicing, truthiness |
| interfaces + concurrency | Go | structural `protocol`s, `spawn`/`parallel:` nursery, `Channel`, cancellation points, `defer` |
| types, errors, control flow | Rust | `enum` + exhaustive `match`, `T!E`/`T?` + `?`, `panic`/`recover` |

- **Correctness is judged against the ancestor's measured output.** Write the Go, Python or Rust
  program and run it. A semantics that surprises a Python/Go/Rust user needs a reason a user would
  accept, never a reason the test suite would.
- **When a heuristic verdict is unsure, it must decline** (hang, stay silent, ask). Ranking: correct >
  no answer > wrong answer. Worked example: `gaps.md` **W7-12**.
- **Fake determinism is drift.** Per-task buffered output once cost users interactive prompts and the
  output of a hung program. Keep determinism in the test sink (order-insensitive assertions), not in
  the language.
- **Fully resolve the issue; do not file a residual as a substitute for a fix.** Three fixes in a row
  each closed one row and opened one or two. A residual's premise decays (see §7). File only what is
  out of scope.
- **Derive, don't mirror. Each fact has exactly one owner; everything else reads it.** Wave 16 traced
  5 P0 and 7 P1 to facts decided in two or more places; 34 of 51 earlier fixes were point patches whose
  families all came back, and the fixes that held all deleted code
  ([`w16.md`](../history/bug-hunts/w16.md)).
  - Before you fix a bug in a family that already has a sibling fix, ask **"which duplicate does this
    delete, or which single source does it add?"** A clause at the repro site is at best a documented
    interim, and only with the owner's agreement.
  - A second resolver, predicate or list that must agree with a first is the bug, even before it
    drifts. Make one consume the other and delete the copy (precedent: `ExternTable` replaced the
    compiler's own extern resolver).
  - Names: the checker records a `Resolution` per `NodeId` from ONE head decider over `owning_scope`
    (`head_binding`). The compiler reads that table and nothing else; a missing entry is an
    `internal:` error, never a fallback. A shadow rule moves only in the commit that makes every
    compiler arm for that position read the table.
  - A "mirrors X" comment, a "keep in sync with Y" comment, or a drift test with an exemption is a red
    flag in review.
  - Each family fix lands with **one test that enumerates the whole grid** (every operation × every
    context, every binder × every name kind). Precedent: `intrinsic_grants_all_have_vm_arms`.
- **How to find the root cause.** A bug report names a symptom in one cell. Before you design a fix:
  1. **Name the fact the bug is about**, not the program ("what does this name refer to", "may this
     operation block here", "is this call a write", "are these two handles the same object"). Group
     all findings by that fact.
  2. **Find every place that decides that fact** by grep, with `file:line`: checker, desugar,
     compiler, runtime, native tables, docs. Two or more deciders is the bug. Tell-tales: "Mirrors the
     checker's …", "keep in sync with …", a drift test with an exemption, a per-keyword or per-op
     denylist, a veto list that grows per shape.
  3. **Draw the grid the fact spans** (operations × contexts, binders × name kinds, kinds × positions,
     handle kinds × identity operations). Mark the broken cells. Probe a few unreported cells.
  4. **Count the history:** how many past tickets or rows patched this class (`git log -S`,
     [`history/bug-hunts/`](../history/README.md#bug-hunts)). Three or more means the fix is
     structural.
  5. **Name the wrong assumption**, usually "the same AST gives the same answer" or a leftover from an
     earlier design (one heap, one engine, top-level-only declarations).
  6. **Design the single source and the deletion:** the owner, the readers, the copies to delete, the
     whole-grid test. Write it down and get the owner's decision on any language change before you file.
- **Derive the grid's axes from every CONSUMER of the fact, never from the repros.** Three wave-16
  structural fixes were wrong just outside their grids ([`w17.md`](../history/bug-hunts/w17.md)). Grep
  every crossing route, every syntactic position a name can occupy, every receiver kind, and make
  each one an axis.
- **A single source must reach every boundary its fact crosses.** Add the boundary axis: same module
  vs another module, value vs call, first stage vs retry ([`w18.md`](../history/bug-hunts/w18.md);
  TICKET-196 exported the record as `ModuleSig::members`).
- **A fact pushed for one cause and polled for another is two deciders** (TICKET-188). Derive both
  from one wake set, including the deadlock verdict's registry.
- **A structural protocol's parameter names are part of its contract** (TICKET-187). Conformance
  checks names at every arity, and a protocol call never borrows an implementor's default. A table
  keyed by METHOD NAME across all structs never replaces the receiver's own declaration.
- **A remaining, bounded exception in a Decisions section is a cell to measure.** A "bounded" 5 ms
  guard wait hit about 25% of updates under seeded mode (TICKET-194). Put each named exception in the
  grid and measure it under the seeded scheduler before you accept it.
- **Defer a stdlib gap on verified cost, never on "nobody asked".** A reference-language idiom is the
  need. `Reader.lines()` was deferred on a false cost claim. Spike before you trust a "too expensive"
  note.

## 2. Verification: what a green gate cannot see

A silent wrong answer has no assertion to fail. No gate measures performance, reads a message or
executes prose.

- **A change that widens what is rejected (or narrows when an error is returned) is untested by the
  suite that passed before it.** Four batches shipped green past 4000+ tests. Before you land a reject,
  refuse or error predicate: write the premise, list the neighbours it implies (same shape, other type,
  context or worker count), and run them on the pre-change binary in a separate `CARGO_TARGET_DIR`.
- **"The rule fires" is not "the rule is right."** A `rejects()` test passes whether the reason is true
  or invented (a ban on `fn neg` deleted the only spelling of negation). For each rejected case, prove
  the harm on the pre-fix binary; derive `ok()` neighbours from the premise, not from the name category.
- **Pick the test shape that can expose the mistake.** A carve-out passed on `map` (`fn(E) -> U`) and
  failed on `fold` (`fn(A, E) -> A`, where the concrete `A` shows it).
- **A symmetric test for an asymmetric relation is a bug, whatever the results.** One W7-42 guard was
  cut wrong three times, each green; the third gave a silent wrong value. If swapping the operands
  changes the question, the predicate must be directional. Derive neighbours by order (swap two
  statements, insert an unused one) and prefer the existing directional helper (`merge_unknown`).
- **A checker warning's gate is derived from the runtime, one program per shape.** List every position
  the rule could fire in and run each. When unsure, under-warn.
- **To install a bound, grep the structural shape for every site; do not list sites from memory.** A
  third fold loop (`parse_type_postfix`) was missed and SIGABRTed (W7-50). A sizing oracle written from
  the same list misses the same site.
- **A lossy decode blinds a comparison oracle.** `from_utf8_lossy` maps each invalid byte to `U+FFFD`.
  When a change widens what a program can emit, audit the detectors in the same commit. Test: feed
  `ff fe` vs `fe ff` and assert the comparator reports it.
- **Check list contents with `.len()` and index, never with `print()`.** `[""]` renders as `[]`. An
  unfamiliar behaviour is not a bug until an oracle says so.
- **Adversarial review is a required step.** Each incident in this section passed the full gate. Budget
  it for checker soundness, runtime verdicts, the shared-heap safety rules, and any error-vs-proceed
  decision (the `adversarial-review` skill).

## 3. Checker soundness

**The compiler is type-blind by construction** (`compile_graph` receives only the AST). The checker's
accepted set must be a **subset** of what the compiler can lower. Each violation is a
check-OK-then-broken program, and a JIT would trust the static type.

- **Fix by narrowing the checker, never by plumbing types into the compiler.** Where both must agree on
  a syntactic predicate, put one shared predicate in `src/ast/mod.rs` and call it from both
  (`const_num`). For name resolution the fix is a checker-recorded resolution that the compiler only
  reads (wave 16 K1-K3: a local `ord` lost to the builtin). Do not add another "Mirrors the checker"
  copy. Six of thirteen wave-5 bugs had this shape.
- **An int never widens into a `float` slot (rule D3, the Rust model).** `x: float = 1` is an error,
  literal or not, at every sink; write `1.0` or `x.to_float()`. There is no `Op::CoerceFloat`. Do not join
  `int`/`float` in an inferred inline-body type (`x / 2` would divide as ints under a `float` type).
  Close int-under-float bugs in the checker; never re-add a runtime coercion. Mixed arithmetic on values
  (`2 * 1.5`, `i < f`) promotes by runtime tag.
- **An un-inferable `Unknown` in an inferred inline-body type bypasses the type check**
  (`compatible(Unknown, _)` is true). `fill_ret` must match every `Ty` variant that carries an inner
  type, with no catch-all.
- **Do not default a slot to a protocol without checking satisfaction.** The inferred `T!E` E-slot
  defaults to `Error` only if `Unknown` or the payload satisfies `Error`.
- **A guard over a hoisted declaration must not use source position.** A top-level `fn` is in its slot
  before any statement runs. Judge the slot, never the statement order. (A module binds each name
  once, so a second module-level binding is an error, not a re-binding to check.)
- **A guard keyed on a module-wide bare-name table fires on a parameter or local that shadows the
  name.** Require scope-0 resolution (`scopes.iter().skip(1).any(...)` means shadowed).
- **A checker→compiler side table keyed on a `Span` aliases wherever one coordinate is reachable under
  two identities** (re-lexed interpolation fragment, spliced default, chained `a?.b?.c`, a synthesized
  variadic pack). Make the coordinate real (`Span` grew `file`; the pack grew an origin field); never
  re-anchor a span. `Span` is 12 bytes and sizes `MAX_DEPTH`, so growing it costs nesting depth.
- **A synthesized declaration is a real declaration.** It must be (1) injective over the full owner
  key, kind included; (2) legal where it lands (a free fn cannot spell `Self`); (3) reachable by a
  resolution path that carries the module coordinate (W7-51).
- **Generic operator overloads need the type's own substitution and `compatible(l, r)`.** Bind the
  receiver's params on the actual side, then require matching type args. Add the heterogeneous
  boundary test (`Box[int] + Box[str]`) on both struct and enum.
- **Test an operator protocol through a generic bound** (`fn has[C: Contains[int]](c): n in c`).
  Concrete-type tests cannot see a missing `Ty::Param` arm.
- **`where T: <scalar>` is an equality constraint.** A harvested `where` is enforced only where the
  receiver's method-call arm calls `enforce_bounds`.
- **Two test entry points, two checker paths.** `ok()`/`rejects()` check one module with bare type
  keys; `entry_ok()` runs `build_graph` + `check_graph` with `<module>::Name` keys, as the CLI does. For
  module-scoped type resolution, use the graph path or both. The reserved-type method surface is
  harvested on two paths (`mod.rs` graph harvest, `setup.rs` single-module harvest): a `native struct`
  mirror must seed both today. That is a duplicate to remove; never add a third path.
- **A native `Ty::Struct` type missing from the reserved/collision guard is accept-then-trap.**
  `import X from M` + `struct X` must be a clean "already defined" error. A struct-returning native
  passes through four fixed lists (`std/M.chz` decl, compiler layout array, `seed_stdlib_structs`,
  `types_by_name`); field order matters.
- **Widen a protocol grant before you install a guard that asks for it.** W7-41's `eq`-bound guard
  shipped while `Eq` was scalars-only and turned `Box([1,2]) == Box([1,2])` into an error; reverted.
  Three fixture traps are in the gaps row.
- **A filtered `cargo test --lib <filter>` is not a trustworthy signal** for checker graph tests: they
  share process-global state. Gate on the full `--lib` run or use `--test-threads=1`.
- **Inference never reads a later statement** (TICKET-238). When a statement is checked, nothing it
  stored has a hole. Do not add a "pin", "refine" or side table that lets a later use type an earlier
  binding; give the statement the type (`xs: List[int] = []`) or reject it there. One predicate
  (`Ty::has_hole`), one check (`Checker::closed_binding_ty`).

## 4. Runtime and concurrency

- **A runtime verdict that declares user code broken is built from what is impossible, never from
  what looks idle.** Three deadlock predicates in a row faulted healthy programs (a cap-1 pipeline is
  all-parked by design). Restrict the verdict to the shape you can prove and decline the rest. Derive
  "can this waiter still be satisfied?" from the waiter itself (its arms, deadline, context) in one
  registry of blocked waiters, not from a veto list. The same holds for "may I block here?": one
  context, one `block_on` ([`w16.md`](../history/bug-hunts/w16.md) Family 2). Fence it with a looping
  test, mutation-check that the test fails on the buggy predicate, and audit the party set for
  self-reference. **A verdict that is right 85% of the time on one program is not a verdict.**
- **A readiness question counts only the OTHER side's entries** (TICKET-185). Each readiness helper
  takes `me` (`ChanState::recv_ready_for(me)`, `send_ready_for(cap, me)`), in the park gap checks and in
  `PartyWait::satisfiable`. "Delivered" has one commit point: a CAS on the party's `Pending` under the
  channel lock.
- **The shared heap is memory-safe only through three rules; a race is the user's bug** (M2, M3.0;
  [`01-model.md`](../reference/concurrency/01-model.md) "Data races"). A buffer that grows, or that a
  sort or a remove replaces, retires, and only the stop-the-world sweep frees it. A `Value` slot
  stores with Release and loads with Acquire. An op on a live object writes atomic slots, never a
  `&mut` over the object. A new container op or object kind obeys all three, plus a grid in the
  race gates; a missed rule is UB. (Until M3.5 the rule here was the airlock's runtime guard.)
- **Freshness is a property of the value graph, not of the operand's root AST kind** (TICKET-240). One
  recursive decider (`Checker::fresh_shape`, `checker/fresh.rs`), read by the write-to-a-temporary
  check (TICKET-254). Before you add a root shape, ask what the rule says one level down. Trap: a map
  key can be a struct with a mutable field.
- **An operator arm with a wildcard on the operator absorbs each operator added later** (TICKET-247:
  `yield ?buf` lost a write). Match the operator enum with no wildcard in one function every walker
  reads (`aliases_operand`). Default an unnamed kind to "holds": a false fault beats a lost write.
- **No wildcard arm on an enum in non-test code** (TICKET-251). `Cargo.toml` `[lints.clippy]` denies
  `clippy::wildcard_enum_match_arm` for the library and binaries; `tests/wildcard_enum_lint_gate.rs`
  runs `cargo clippy --lib --bins --features lsp` inside `cargo test`. Resolve a flagged arm in this
  order: (1) one exhaustive classifier on the enum that each site calls (`Obj::as_list`, `Obj::as_enum`,
  `ExprKind::as_ident`, `ExprKind::is_named_head`); (2) the named variants; (3) an `#[allow]` with a
  one-line reason and a raised `PINNED_ALLOWS` in the gate test. Do not rewrite it as `if let`,
  `let ... else` or `matches!` on the raw enum. A foreign `#[non_exhaustive]` enum keeps its `_`.
- **"Is a write" has one owner.** `native::is_mutating_native_kind` is the one table of mutating
  natives; the checker's `mutates_receiver` reads it (TICKET-179). Never add a second list or a
  per-method exemption.
- **Capture is free-variable, by reference.** A superset capture is safe; a subset breaks silently. The
  analyzer matches every `ExprKind` with no `_` arm and also drives cell-boxing.
- **Container key and membership equality fault like `==`.** Each production site uses the guarded
  form through `seq_slot`/`set_slot`/`map_slot`; the swallowing wrapper is `#[cfg(test)]`-only. Grep
  both `self.values_equal(` and `vm.values_equal(`.
- **Struct/enum map and set keys are snapshotted on insert** (Go value-key model); values stay by
  reference. A key from `keys()` is the stored key by reference (WON'T-FIX, as in Python and Java).
- **A `+1/-1` counter that gates scheduling must survive a Rust panic.** `Vm::guarded` brackets
  `native_reentry` around each native→Chezzi re-entry: `catch_unwind`, restore, `resume_unwind`.
- **A libffi callback `Cif` must be `Box`ed.** FFI UB depends on layout: all goldens passed while the
  CLI segfaulted. Check FFI on the built binary. Segfault without gdb: a temporary SIGSEGV handler with
  `Backtrace::force_capture()` + `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only`.
- **A compound mutate-and-return over `Shared`/`RwShared` does check, mutate and capture inside one
  `write` closure.** Insert-then-`read` lost the key to a concurrent `remove`.
- **`emit_out`/`emit_err` are no-ops on a dead pipe; re-raise the halt at the call site.** A new native
  that writes stdout checks `stream_halt(span)` after it returns, plus a `| head -1` CLI test.
- **A `NativeRet::Map` from a `HashMap` has random order; sort keys before lowering.** Any
  order-observable native value from an unordered Rust container must sort.
- **Native `Kind` is not paperwork.** `Blocking` offloads to the dirty pool; `Inline` pins a core
  worker. A wrong `Kind` is a starvation bug.
- **Size recursion backstops for the smallest production stack, then measure.** The LSP worker is
  ~2 MB. Put test callers on the production stack (`on_frontend_stack`), bisect bytes per node per
  phase, derive the constant. When real depth exceeds the smallest stack, move onto a dedicated big
  stack. A per-`Parser` bound is not global; enforce on the finished tree. Left-associative chains
  parse iteratively and overflow the walkers. Do not raise `VM_STACK_BYTES`: each M:N worker reserves
  it.
- **Opt-in caps measured per heap or by wall clock are not comparable across execution contexts.**
- **Swap what belongs to a context, and undo the swap on unwind** (TICKET-246). M3.3 deleted the box
  context and `Vm::with_box`: a `Shared`/`RwShared` closure runs in the caller's task. A new swap of
  per-context state is undone under `catch_unwind`, never by a swap pair after the call.
- **Render a shared box with no lock and no wait** (TICKET-246, M3.5). Rendering under the payload
  lock hung a box that holds itself (`rc=124`). `Vm::display_box` reads the box's one slot with no
  lock or guard (Go's `fmt`), and keeps a stack of open boxes so a box already on it prints
  `Shared(...)`.
- **A parallel reader of a live heap is proven by the type system** (TICKET-246). Every task reads
  the one heap through `&Heap` (M3.3). `Heap: Sync` is a `const _` assertion. A new `Cell`/`RefCell`
  in `Heap` or `Obj` must fail that build; never add `unsafe impl Sync`.
- **A pure-Chezzi scanner over non-ASCII text must pre-collect its codepoints.** `text[i:i+1]` is O(1)
  only for all-ASCII text; non-ASCII re-collects the `Vec<char>` per call (O(n²)). `field = field + c`
  is O(k²). Check with a large-input timing test.

## 5. Native surface and stdlib

- **A new builtin type, ctor or fn goes in its owning `std.*` module, import-gated, never the global
  namespace.** The gate is checker-only name resolution; a pure type/ctor also needs the `bind_import`
  skip, so cover it with a test that runs.
- **Qualified paths are a Go/Python hybrid** (TICKET-175). An un-aliased `import std.net` binds
  `net.Socket` and `std.net.Socket`: a synthetic dotted-alias import plus one scope-aware fold in
  desugar. Struct patterns stay two-level.
- **A native type under `module.Type` needs three additive touch points:** the checker
  `Type::Qualified` arm, the compiler field-callee arm emitting the same opcode, the `bind_import` skip.
- **Builtins are bodyless `native` decls in `std/*.chz`, front-end only.** A native decl cannot express
  an arg-dependent result (`print`, container ctors) or a bare scalar alias (FFI widths). Check the
  actual got-vs-want `Ty` before you claim a wall.
- **The native seam is scalar/str/map-shaped.** Generic `list[T]` cannot cross; generic helpers are
  pure Chezzi in `std/*.chz`. Concrete `list[str]` crosses through the `map[str,str]` triad in both
  hosts (`VmHost`, `OffloadHost`). A native module can be file-backed (`is_file_backed_native`).
- **Type args go where the generic is declared** (`Box[int].Has(5)`, `obj.m[U](x)`). The parser cannot
  tell `Box[int].make[U](x)` from `value[i].field[k](x)`; only the checker can, by reinterpretation.
- **Static protocol requirements** (`T.default()` through `[T: Default]`) use witness passing (M24).
  Remaining ceiling: a type-declared `T` in a method body (`docs/reference/syntax/`, "static
  requirements").

## 6. Test infrastructure

- **Testing is hybrid: Chezzi first** (`tests/chz/`, run at two worker counts). Use Rust
  `#[cfg(test)]` only for what `assert` cannot express. Fault paths are testable in Chezzi via
  `recover:`.
- **Order assertions: exact when causally forced, multiset when concurrent, never blanket-sorted.** An
  in-process capture splices task output in spawn-slot order; the CLI streams line-atomically. CPU
  contention looks like flakiness: check `uptime` and `pgrep cargo` first, and do not relax an
  assertion until the flake stops.
- **A test that needs ≥2 free pool threads or a wall-clock assertion is its own binary in `tests/`.**
  The lib suite shares one process-wide `OnceLock` pool.
- **Each test that touches process-global native state takes the module's `#[cfg(test)]` mutex.** One
  unlocked participant defeats the lock for all.
- **A wall-clock perf repro must be red on the release binary the gate builds.** Size inputs at 100k+,
  one absolute bound, never a ratio of two clock samples (`tests/no_wall_clock_ratio_gates.rs` bans it).
- **`live_bytes` under-measures a `Value`-size change.** Gate value-model changes on bench deltas and
  peak RSS.
- **The CPython differential's int-bound tracking must follow a value across each seam** (loop back
  edge, call boundary). Regression pattern: hand-build the worst-case IR and assert the oracle reports
  it.
- **Mechanical oracles go quiet before the language is correct.** Wave 9 found nothing in 23 000
  panic-fuzz and 13 000 differential seeds; each finding came from a hand-built program judged against
  a run reference. Sample a flake comparison: a ~5% rate can read as `0/30`.
- **Validate the LSP in headless nvim against a reinstalled `chezzi-lsp`**, not cargo alone.
  Decl-site hover is an additive `Span` + `hover_record_at`, gated on `!generic_arg_prepass`.

## 7. Process and git hygiene

- **Build the repro first, then price a filed gap.** A "too expensive / unreachable" note decays under
  later commits (both W7-4 residuals were mispriced). Read a closed row's prescription before you
  re-implement it.
- **Merge ritual:** confirm `HEAD` is `main`; `git merge-base main <branch>` must equal `main`; build
  via `cargo run --bin chezzi`, never a hard-coded target path; delete the branch and prune the
  worktree (each `target/` is ~1.6 G).
- **Two concurrent builds must not share a release target dir.** Confirm with
  `strings … | grep <new message>` when in doubt.
- **Automated review signals are not a merge gate,** but read the charges: panels found true positives
  nothing else caught. The arbiter is a manual repro on the merged binary at both worker counts,
  against the ancestor.
- **Subagent briefs need a mapped seam and explicit stop conditions** (file:line anchors, invariants,
  "STOP if this needs a checker change"). Put "no subagents, no monitors, no sleep, never two cargo
  commands in one worktree" verbatim in each dispatch.
- **Wrap concurrent `cargo` in a memory-capped scope** (`systemd-run --user --scope -p MemoryMax=6G`).
  Exit 137 means "reduce parallelism". Run salvage tests under a shell `timeout`: a hang reads as exit 0.
- **Docs are part of the change.** Grep for `deferred` / `experimental` / `not yet` / `follow-up` about
  what you changed and fix it in the same commit.
