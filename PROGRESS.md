# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

**Legend:** ⬜ not started · 🟦 in progress · ✅ done

> **Mode:** Claude implements directly — working, tested code each session (see `CLAUDE.md`).
> Full per-milestone detail lives in git history; this file is a forward-looking tracker, not a changelog.

---

## Current focus

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
- **Non-goals (v1):** mutable `bytearray`, `byte`/`u8` scalar, bignum, encode/decode codecs
  (base64/hex are a separate `std.*` gap), `bytes` methods beyond the 5 table rows + Display, a
  `{b:spec}` format-spec, and `ConstBytes` interning (allocs per push, like a list literal).

**✅ Qualified enum-variant access `Enum.Variant` (owner-requested, explicit exception to the M19/M18
feature freeze).** Variants can now be written **qualified** (`Color.Red`, `Shape.Circle(2)`,
`case Shape.Circle(r):`) as an optional, equivalent spelling of the bare form — `Shape.Circle(7) ==
Circle(7)`. It is a pure readability aid: variant names stay program-global (the cross-enum collision
rule is **unchanged** — two enums still can't share a variant name), so the qualifier validates and
then resolves to the same variant. **Both engines + parity** (VM/`--serial`/interp byte-identical via
`examples/enum_qualified.chz` + golden). Implemented by intercepting `Field{Ident(enum), variant}`
(value/pattern) and `Call{callee: Field{…}}` (payload constructor) in the checker, compiler, and
interp, gated so a real binding named like the enum wins; the parser gained an optional `enum_name`
qualifier on `Pattern::Variant` (validated by the checker, ignored at runtime). Out of scope: letting
two enums share a variant name (would need enum-scoped variant lookup — a separate, larger change);
qualified **built-in** variants (`Option.Some`) are accepted in patterns but bare stays canonical.

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

**🟦 M19 — Perf track (in progress).** The language is frozen feature-wise; this milestone is pure
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
- **Big/separate milestones** (only once the language has truly stopped moving): NaN-boxing as its own
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

Core feature-complete through **M18**; **concurrency shipped through Tier-D (D0–D6c) + M-C**. The surface —
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
Channel[bool]` (a `wait:` arm), `cancel()` (anytime/any task), `deadline_at()`. Tokens are **flat** in
v1 (no parent/child derivation — tree propagation is a documented follow-up). Pure Chezzi
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
recoverable fault; binary work → the immutable `bytes` *sequence*, **shipped** — no `byte`/`u8` scalar). **Level-3 dynamic
C-ABI FFI is NO LONGER a non-goal — v1 shipped** (`extern "lib":` scalar calls via dlopen+libffi;
structs/callbacks/varargs/userdata still deferred — see "Done" below). **`yield`/generators are likewise
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
