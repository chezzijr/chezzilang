# Root causes — bug-hunt wave 22 (JIT sweep #7, 2026-10-09)

Sweep #7 ran five hunters on `main` at `19b36f77`: carriers end to end, inference and generics, Executor
and scheduler, parser/names/modules, and airlock plus stdlib vs CPython. Extra weight went on
TICKET-226..234 (generics and carriers, `None`, the `?`/`!` surface, the return rule, open-`None`
pinning, runner width, Executor job state). The main loop re-ran every finding below on the release
binary. **Not clean: 5 P0 and 4 P1.** The JIT entry rule's "two consecutive clean sweeps" count stays at
zero.

The pattern this wave: the new surface is sound where it has ONE owner (the wrap decision, the carrier
patterns, exhaustiveness, the display rule all came out clean), and unsound where an older mechanism was
left beside it.
- An untyped `None` / `[]` is still a hole with no identity, tracked by binding name. TICKET-225 built
  real type variables and used them for two cases only; TICKET-234 added five more name-keyed functions.
- TICKET-232 sealed a cut job's handle under the lock, but a reader parked on another scheduler is woken
  after the lock is dropped, and no verdict asks a parked fiber what it waits on.
- TICKET-229 made `None` an ordinary prelude import; TICKET-231 then made it the absent value and the
  void type. It is a literal in everything but the lexer.
- TICKET-227's implicit wrap puts a `Some` cell around a fresh spawn operand; the airlock unmarks the root
  object only.
- TICKET-227 made one wrap decision, but the expected type reaches it through three channels, and the
  generic call paths infer the argument before they read an explicit type argument.

A separate crash found the same day outside the sweep (a one-line `spawn` of a native call has no frame)
is TICKET-235, family Blocking; it is not repeated here.

## Findings

| ID | Sev | Repro (short) | Chezzi | Ancestor |
|---|---|---|---|---|
| A1 | P0 | `z := None` / `g := fn() -> str?: z` / `z = 7` / `g()` matched as `?s: s.upper()` | check OK; run: `type int has no method 'upper'` | Rust: `mismatched types` |
| A2 | P0 | `z := None`; loop: `match z: ?s: acc.push(s)` (`acc: List[int]`), then `z = "str"` | check OK; `['str']` in a `List[int]`, then `cannot apply Add to str and int` | Rust rejects |
| A2b | P0 | global `g := None`; `show()` reads `match g: ?s: s.upper()`; `seta()` sets `g = 7` | check OK when `show` is declared first, rejected when second | order must not matter |
| A3 | P0 | `h := [None]` / `fn r() -> str: return h[1] ?? "e"` / `h.push(2)` / `x: str = r()` | check OK; prints `2` (an int in `x: str`) | Rust rejects |
| A4 | P0 | `xs := [None]` / `ys := [xs]` / `ys[0].push("s")` / `xs.push(7)` | check OK; rc 0; a `str` in a `List[int?]` | Rust rejects |
| A5 | P0 | `c := Cell(None)` / `a: Cell[int] = c` / `b: Cell[str] = c` / `a.v = 5` / read `b.v` | check OK; run: `type int has no method 'upper'` | Rust rejects |
| A6 | P0 | `h := [None]` / `a: List[str?] = h` / `h.push(2)` | check OK; prints `[None, 2]` | Rust rejects |
| A7 | P2 | in a fn: `z := ?5` / `z ?? 0` (also `match`, `z?`, `z?.len()`, `else`) | `'??' applies to a T? or T!E value, found _`; works at top level | Rust `5` |
| A8 | P2 | `v := Box.new()` (nothing pins `T`) / `v.add(1)` | accepted, then `expected T, found int` | Rust infers `Bx<i32>` |
| B1 | P0 | a nursery task waits on `submit_result(...).recv()`; a sibling calls `shutdown_now()` 1-2 ms later | false `deadlock`: 10/10 at T=2, 9/10 default, 0/10 at T=1; pre-226 binary 0/10 | CPython 10/10 complete |
| C1 | P1 | `None := 5` (also a param, a loop var), then `return None` in a `-> int?` fn | check OK; returns 5 | CPython: `SyntaxError: cannot assign to None` |
| C1b | P1 | `fn g(p: P?, None: int?) -> int?: return p?.x`, `p` absent, `g(q, 7)` | prints 7: the `?.` lowering reads the user's `None` | must print `None` |
| D1 | P1 | `spawn opt([])` with `acc: List[int]?`; the task pushes | false fault `this value is this task's copy` | Go, CPython: runs |
| D1b | P1 | `spawn f(S([]))` then `s.xs.push(1)`; `spawn f([[]])` then `xs[0].push(1)` | same false fault | Go, CPython: run |
| E1 | P1 | `x := if c: a() else: b()` (both `int!str`), then `x?` in a `-> int!str` fn | `'?' propagates error Error, but the enclosing function's error type is str` | Rust accepts |
| E2 | P1 | `take(fn(a: int) -> int: a + 1)` into `cb: (fn(int) -> int)?` | rejected; the same closure through a variable is accepted | CPython runs |
| E3 | P2 | `Box[int?](5)`, `opt[int?](5)`; at module scope `id[int!str](!"e")` | rejected though the type is written | Rust accepts |
| E4 | P2 | `b: int8? = id(300)` | check OK; prints 300 (`b: int8 = id(300)` is rejected) | Rust: literal out of range |
| E5 | P2 | `[w, 3]` with `w: int?` is rejected; `[w, 3, None]` is accepted | adding an element changes the verdict on two others | owner decision |
| E6 | P2 | `c: C? = C(0)` / `c?.bump()` where `bump` returns nothing | `expression returns no value (None)` plus a spurious `<unknown>?` warning | Rust `as_mut().map(..)` runs |
| F1 | P2 | `fn mk(): fn(x: int): x + 1` | `expected identifier, found '('`; parenthesised works | `docs/grammar.bnf` allows it |
| F2 | P2 | `y: int!= 6`, `fn f(a: int!=5)` | `expected '=', found '!='`; `x: int?= 5` works | no ancestor |
| F3 | P2 | `a, b := pair(n) else e:` | `expected end of line, found 'else'` | Rust `let Ok((a, b)) = .. else` |
| G1 | P2 | messages: an unknown type prints as a bare `?` (`?!str`, `fn(?) -> ?`); a `??` mismatch says "branches"; a variant-import clash cites the removed `Option` | | |

Clean: the carrier core (wrap sites, nested depth, patterns, exhaustiveness and unreachable arms,
propagation, guards, ordering, equality, display); the Executor state grid, handles, cancellation with
`defer`, contention counts; 13,000 fuzzed inputs through `ast`/`check`/`--errors=json` (no panic, no
`internal:` text, no invalid JSON); eleven generated CPython differentials over strings, slices, format
specs, math, parsing, csv, encoding (zero unexplained diffs); write-to-copy detection (always a compile
error or a fault, never a silent lost write).

## Family A — an open slot has no identity (A1..A8)

**The fact:** what type an untyped `None`, `[]`, `[None]`, `{}`, `Cell(None)` or `Box.new()` ends up
with.

**Where it is decided today.** An open slot is a `Ty::Unknown` leaf (`src/checker/ty.rs:443`). `Unknown`
has no id and is compatible with everything (`ty.rs:606`, `src/checker/proto.rs:1233`). The slot's type
lives at `scopes[scope][name]`, and three side tables are keyed on (scope index, binding name):
`empty_coll_sites` (`src/checker/mod.rs:2674`), `empty_coll_aliases` (`:2680`), `carrier_pins` (`:2687`).
"Pinning" is about twenty statement-specific call sites that find a NAME and overwrite its entry:
`repin` (`src/checker/setup.rs:2659`), `repin_place` (`:3038`), `drop_empty_site` (`:3204`),
`drop_value_escape_sites` (`:3302`), `refine_receiver` (`proto.rs:1051`), `refine_index_receiver`
(`:1161`), `constrain_empty_arg` (`src/checker/expr.rs:4388`), `pin_carrier_use` (`setup.rs:3109`),
`link_empty_alias` (`:2792`), `propagate_alias_pin` (`:2831`), `pin_open_slot` (`:2923`),
`merge_unknown` (`mod.rs:3380`, 30 sites). Four non-equivalent predicates answer "is it open":
`is_unrefined_empty_coll` (`setup.rs:3187`), `is_open_coll` (`:3132`), `is_unpinned_carrier` (`:2904`),
`contains_unknown_in_slot` (`mod.rs:3348`).

A second, correct mechanism exists beside it: real unification variables (`Ty::Var`,
`src/checker/tyvar.rs`), bound inside `assignable` and `join_ty` and judged when the frame closes. It is
used for two things only: a generic fn value read (`src/checker/pattern.rs:2820`) and an un-hinted `?x` /
`!e` in a fn body (`pattern.rs:4113`, `:4137`).

**How they disagree — the miss site of each finding:**
- A1: the return pin is one line in `check_return` (`src/checker/sig.rs:4620-4629`). The two sibling
  return checks, a named inline body (`sig.rs:5105-5136`) and a closure body (`pattern.rs:6192`), only
  call `assignable`, which is true for `Unknown`.
- A2: a read has nothing to write. A match payload takes the scrutinee's `Unknown`
  (`pattern.rs:791-793`); a method on an `Unknown` receiver is accepted (`expr.rs:3931-3934`); an index
  and a `for` return the `Unknown` element (`sig.rs:5293`); `?.` has an `Unknown` arm (`pattern.rs:5323`).
  The verdict depends on order because `repin` mutates the stored type in place (`setup.rs:2661`) and
  uses walked earlier are never revisited.
- A3: the `??` pin is guarded by `if let ExprKind::Ident(n)` (`pattern.rs:5388-5404`); a projection has
  no name.
- A4: the alias link is syntactic (`sig.rs:2602`, `:2795`: the value must be an `Ident`). `[xs]`,
  `id(xs)`, `B(xs)`, `{"a": xs}` and an `if` value produce a fresh `List[Unknown]`; the source name is not
  recoverable from a `Ty`. `docs/syntax.md:4402-4404` documents it as a ceiling.
- A5: an annotated `let` calls `drop_empty_site`, which pins only `is_unrefined_empty_coll` bindings
  (`setup.rs:3235`). DEC-064 says a carrier pin is recorded by a read and checked only at a write of the
  same name; that is sound for an immutable nullary variant and unsound for a mutable or payload slot. A
  typed PARAMETER does pin a struct (`expr.rs:4427`): one sink kind, two predicates.
- A6: same gate; the argument path uses `is_open_coll`, the `let`/`return` path `is_unrefined_empty_coll`.
- A7: `infer_wrap_val` returns a bare `Ty::Var` (`pattern.rs:4137`). `??` (`:5373`), `?.` (`:5296`),
  `match` (`sig.rs:5799`) and the `else` guard (`pattern.rs:2383`) destructure the type with a Rust
  `match`, and a `Var` falls to the error arm. Recorded as a known gap in TICKET-227.
- A8: `expr.rs:1303-1318` leaves the enclosing type's `T` as a leaked `Ty::Param("T")`.

**The grid:** how a value reaches a use (direct name, name alias, alias through a literal / call / field
/ `if`, holder, projection `x[i]` / `x.f` / `m[k]`, closure capture, module global from another fn,
generic instance) x kind of use (write, typed `let`, block return, inline or closure return, typed
argument, generic argument, method argument, pattern payload / `?.` / index / `for` read). Broken cells:
every read column; inline/closure return; every expression-alias and holder row; every projection row
except a write; the generic-instance row at a typed `let` and a return; a nested-open collection at a
typed `let` and a block return.

**Past patches:** eleven tickets or waves and about 21 fix commits, each adding a site, a predicate or a
table: `08ddf6a6`, `9eef288c`, `047d4a8a` and three false-positive follow-ups, W8-45, `8132230a`, W8-46
("seven routes"), TICKET-032 (alias group), TICKET-064 (`carrier_pins`), `3f8b6d06`, TICKET-225/227
(`Ty::Var`, two cases), TICKET-234 (`pin_open_slot`, `repin_place`, `unify_arg`, `join_fill`,
`is_open_coll`). No root-cause write-up covered this class before. The design already promised the fix:
`docs/design-generics-and-carriers.md` R5, "a generic value read without a pin gets a type variable ...
rejected only if still unpinned at the end". TICKET-225 was scoped to fn values, and DEC-225 records "the
empty-collection refine is a second hole mechanism ... moving it onto the var store is a separate change".

**Wrong assumption:** a hole in a type belongs to the binding name that first held it, so pinning means
"find the name and overwrite its scope entry", and a site that finds no name can safely do nothing.

**Single source:** every open literal or instantiation mints one `Ty::Var` at its AST node. The variable
travels inside the type of every expression that reaches the value, so aliases, holders, projections,
generic calls and joins share it. Pinning is `solve`, which already runs in `assignable` and `join_ty`.
The structural consumers (`??`, `?.`, `match`, `else`, index, `for`, method lookup) unify the operand
with `Option[fresh]` / `List[fresh]` instead of matching on the `Ty`. A variable still unbound when its
frame closes is an error or a documented default.

**It deletes:** the three name tables and their `DiagMark` fields; about 23 functions (every pin writer
listed above, plus `none_slot_from`, `none_sibling_slot`, `branch_slot`, `unify_arg`,
`finalize_empty_coll_sites`); the five predicates; `merge_unknown`; the leaked-`Param` branch.
`Ty::Unknown` remains only as the error-cascade sentinel. Rough size: 110 call sites, 93 predicate uses,
and a classification of the `Ty::Unknown` producers (444 lines mention it).

**Owner decisions this needs:**
1. A read of an open value at two types. `e := Box.Empty` (or `z := None`), then `a: Box[int] = e` and
   `b: Box[str] = e`. DEC-064 keeps this legal today. With one variable per value it is an error, unless
   an immutable nullary value is given a fresh variable at each read.
2. A `None` or `[]` that nothing ever pins. The design's D3 table says a never-pinned `None` "stays
   open"; `xs := []` never used is already an error.
3. An open module global pinned from a fn body (`g := None` at top level, `g = 7` inside a fn). A frame
   is one fn body or one top-level statement (DEC-183, DEC-225), so this needs a module-level solve, or
   the rule "an open global must be annotated".

**Constraints the plan must carry:** speculative walks and rollback retire variables (DEC-157), so
minting needs the per-node memo the two existing cases use; a `spawn` capture is a deep copy and must not
share the parent's variable; a rebind breaks the pin group (DEC-032); TICKET-225 measured 1-5% on two
benches with variables in use.

## Family B — a parked reader is judged without reading its channel (B1)

**The fact:** can anything still wake this waiter.

**Where it is decided.** One latch, `QuiesceState::decide` (`src/vm/quiesce.rs:574-598`), over
`verdict_on` (`:608-626`); and the per-sched predicate `MnSched::quiesced_core_given`
(`src/vm/mod.rs:5503-5609`) with its wrappers `is_deadlocked_given` (`:5417`) and
`is_deadlocked_ignoring_jobs_given` (`:5683`). A blocked party is represented three ways:

| Reader | Record | Is its channel read by the verdict? |
|---|---|---|
| main (`BlockCtx::OwnThread`) | `PartyWait::Recv` | yes, `satisfiable()` (`quiesce.rs:156-162`) |
| a fiber inside a callback or `defer` (`Demote`) | `Waiter` | yes, `any_waiter_satisfiable` (`mod.rs:3018`) |
| a nursery fiber or an Executor job (`Park`) | `c.parked` | **no**: counted as `parked_n` only (`mod.rs:5568`) |

**The window.** `MnSched::finish` (`mod.rs:5078-5145`) for a cut job: `scopes[..].done += 1` (`:5112`,
the job leaves every "still a waker" count), seal the handle (`job_event` → `settle_job`, `:6131`), drop
the core lock (`:5142`), and only then `wake_sealed` (`:5143`), which locks the READER's core and requeues
it (`:5021-5022`). Between `:5142` and `:5022` the Executor's sched has no incomplete scope, the reader's
sched has `parked_n > 0` and zero counters, and the value is sealed and visible to nobody who is asked.
"Is the job still a waker" has four hand-kept copies that flip at the same line: `peer_can_move`
(`mod.rs:5635`), `live_eager_bodies` (`quiesce.rs:682`), `may_fault_unproven` (`mod.rs:4392-4399`), the
Join arms (`mod.rs:3007`, `quiesce.rs:194`).

**Why it is irrevocable.** `latch_own_verdict` (`mod.rs:5393-5400`, TICKET-232 `d2c1f4db`) calls `decide`
before the victim licence (`:4346`) and leaves the latch set even when the re-derive returns `None`.
Before that commit this path waited 5 ms and retried. Measured: the old-syntax twin of the repro on the
binary of `832838aa` faults 0/10 at T=2 and 0/10 at the default count; `main` faults 10/10 and 9/10.

**Why T=1 is safe:** the single runner permit serialises the cutter and the wake (`src/vm/sched.rs:2533`).
It is not a guard.

**The grid:** reader kind (main, nursery fiber, fiber in a callback or `defer`, job of another Executor,
job of the same Executor) x what it waits on (result channel, `Task.get`, a plain channel the job feeds)
x how the job ends (returns, faults, cut while held, cut while running / sleeping / parked, a held job
dropped from another job's `finish`) x who cuts. Broken: every `Park` reader x "cut while running". The
existing `tests/executor_stop_grid.rs` has no reader-kind dimension; main always reads.

**Past patches:** 105 commits mention deadlock, 48 are fixes; false-verdict tickets 062, 099, 101, 103,
118, 125, 129, 134, 136, 148, 223, 232. `docs/lessons.md:240-253` already states the rule ("derive 'can
this waiter still be satisfied?' from the waiter itself ... in one registry of blocked waiters").
TICKET-181 built that registry for demoted waiters and parties only.

**Wrong assumption:** a fiber parked on a channel can only be woken by something the counters see, so
"write the value" and "requeue the reader" may be two steps with the writer leaving the counts in
between.

**Single source:** one "can anything still wake this waiter" answer inside `quiesced_core_given`, asked
of every blocked thing, each `ParkedEntry` included, through the `PendingOp` it already carries
(`mod.rs:4462`). A parked fiber whose channel is ready, closed or sealed vetoes, as a `Waiter` and a
`PartyWait::Recv` do. It deletes the `parked_n`-only clause as evidence of being stuck, the three-way
split of one question, the four "job still live" copies, and the latch-before-licence order.

## Family C — which names may be bound (C1, C1b, part of G1)

**The fact:** may this name be declared here.

**Where it is decided.** `None` is not a keyword: the lexer emits `Ident("None")`. It is a prelude
variant import (`std/prelude.chz:238-250`), and `Checker::classify_ident`
(`src/checker/resolve.rs:113-165`) resolves a local, a global and a user fn before it. `None` also has
three jobs keyed on its spelling and blind to shadowing: the void type (`proto.rs:1980`, and the prefix
`!E` type at `src/parser/mod.rs:2616`), "the literal None" (`is_none_lit`, `setup.rs:2967`;
`is_inline_default`, `src/desugar/mod.rs:160`), and a compiler-synthesised identifier in the `?.` lowering
(`desugar/mod.rs:1680`), which is why C1b returns the user's parameter.

"Reserved" is five name lists in `src/checker/mod.rs` (`is_reserved_type` `:208`, `is_reserved_name`
`:396`, `is_reserved_alias_target` `:405`, `is_builtin_variant` `:412`, `is_reserved_module_bind` `:428`,
`is_reserved_protocol` `:617`), plus `REMOVED_NAMES` in the parser, `ffi::is_width` and the lexer keyword
table, read by about ten hand-placed guards, one per declaration kind. Every VALUE binding (`:=`, typed
binding, param, lambda param, loop var, comprehension var, pattern binder, `else e`, `wait` arm, nested
fn name) goes through one function, `Checker::declare` (`setup.rs:2573`, 21 call sites), which checks
nothing.

**Measured grid** (name x position `X := 5` / param / loop var / fn name): `None`, `self`, `len`,
`Error`, `Iterator`, `tuple` are accepted everywhere; `print`, `int`, `str`, `List`, `Channel`, `range`,
`panic` are accepted as a local, a param and a loop var and rejected as a fn name; `true`, `false`,
`assert` are parse errors. Consequences: `int := 5` then `int("3")` gives `int is not callable`; `struct
print:` is accepted and then unreachable.

**Past patches:** 46 commits mention "reserved", about 13 are point patches of one binding form each.
`docs/root-causes-w16.md` Family 1 fixed name RESOLUTION (TICKET-180/182) and never touched binding.

**Wrong assumption:** "reserved" is a property of each declaration kind, and `None` is one more
shadowable prelude name.

**Single source:** one predicate "this name cannot be bound", read at `Checker::declare` and by the item
guards; the lists collapse into it. `None` becomes a lexer keyword, which retires the three spelling
tests and the unhygienic lowering at once. A binder x name grid test pins it.

**Owner decision:** which names a local, a param or a loop variable may not take. `None` (and `self`
outside a method) is forced by C1. For the builtin callables and types (`print`, `int`, `len`, `List`)
CPython allows the shadow (`print = 5` is legal Python) and Go allows it too (`len := 5`).

## Family D — freshness is decided for the root object only (D1, D1b)

**The fact:** can the parent still reach this value after the crossing.

**Where it is decided.** `Checker::crossing_of` (`sig.rs:2355-2387`) matches the operand's AST kind: a
list / map / set literal, a comprehension, a container `.copy()` and a struct constructor are `Move`;
everything else is `Copy`. It runs before the implicit wrap (`record_wrap`, `proto.rs:1447`; the compiler
emits the wrap on the parent side, `src/compiler/mod.rs:3419`). The runtime then clears the copy mark of
the ROOT handle only (`Heap::unset_copied`, `src/vm/heap.rs:737`; callers `sched.rs:5257-5274` and
`:4374-4390`), by a recorded rule: "its ROOT is unmarked. Its children stay marked (`copy()` is shallow,
DEC-160)" (`src/vm/crossing.rs:10-11`). Under a carrier the root is the `Some` cell. The same shape test
feeds `slot_crossing` (`sig.rs:2392`) and generator frame privacy (`:2517`, `:2723`).

**Measured grid** (operand → parameter, write in the task): a root write runs for `[]`, `S([])`, `[[]]`,
`{"a": []}`. A false fault for: any fresh operand under a carrier parameter (`List[int]?`,
`List[int]!str`, `S?`, a literal default, a comprehension, a generator frame-local); every fresh object
one level below a fresh root (`s.xs.push(1)`, `xs[0].push(1)`, `m["a"].push(1)`, a variadic pack);
three wrapper kinds that are never fresh at all (an explicit `?[]`, a tuple literal, an enum variant
constructor). The true-positive controls hold: `spawn f([xs])` with a named `xs`, and a named value into
a carrier parameter, both fault.

**Past patches:** TICKET-179, 189 and 190 each extended the list of fresh root shapes. None changed the
root-only unmark. Earlier airlock tickets: 137, 154, 165, 169, 170, 171.

**Wrong assumption:** freshness is a property of the operand's root object, read from the root AST node's
kind.

**Single source:** freshness of the value graph, computed once by the checker, recursively over the
operand expression: a constructor-like node (list, map, set, tuple literal, comprehension, struct or
variant constructor, an implicit or explicit carrier wrap, a literal default, a pack) is fresh, and each
child is fresh or not by the same rule; a named or call-result child keeps its mark. The runtime unmarks
exactly the objects fresh nodes built. The one-bit-per-slot mask (`Crossing::mask`, `frame_mask`) cannot
express "fresh root, marked third child"; replacing it is the structural part. The three other callers
of `crossing_of` read the same decider.

## Family E — the expected type reaches an expression through three channels (E1..E6)

**The fact:** what type an expression is checked against.

**Where it is decided.** The wrap decision has one owner: `Checker::infer` (`pattern.rs:1795-1834`) →
`meet_slot` (`:1840`) → `wrap_mode` (`proto.rs:1427`). The expected type is delivered to it three ways:
an OWNED hint (`install_hint(.., true)`: wraps, checks constant width, feeds `!e` and `?x`); a SEED hint
(`infer_arg_seeded`, `expr.rs:4074`: feeds, never wraps); and a closure side parameter
(`infer_closure(.., expected)`, `pattern.rs:6042`; the `Closure` arm of `infer` passes `None`,
`pattern.rs:2097`).

- E2: `infer_arg` (`expr.rs:4053-4069`) sends a closure to `infer_closure` only when the expected type is
  exactly a fn type; otherwise the closure is inferred with no hint. The gate is copied at
  `sig.rs:567-573` and `expr.rs:4075`. "Look through one carrier layer" exists four times with no shared
  caller: `Ty::carrier_payload` (`ty.rs:515`, one caller), `sink_payload` (`pattern.rs:3690`), and hand
  matches in `infer_wrap_val` (`:4133`), `infer_err_val` (`:4093`) and `wrap_mode` (`proto.rs:1437`).
- E3, E4: the six generic call paths (generic fn, generic constructor, generic variant, generic static
  method, generic method) share `infer_generic_arg_tys` (`expr.rs:4094-4130`), which infers the argument
  BEFORE the explicit type arguments are substituted; `check_generic_arg` (`expr.rs:4144-4181`) then
  re-infers only closures and constants and compares everything else as its first-pass type (`:4168`).
  The non-generic paths substitute first. `!e` at a pinned generic slot fails at module scope only,
  because a fn body gives it a frame variable (`pattern.rs:4113`) and the top level does not (`:4117`).
  The width re-bind arm in `unify` matches a bare width only (`mod.rs:3754`).
- E1: `default_expr_result_e` (`pattern.rs:1570-1589`, callers `:1476`, `:1554`) rewrites the error type
  of an un-annotated `if` / `match` / `??` result to `Error`. Its comment says it mirrors the
  return-inference default; TICKET-228 deleted that default (`0302f946`). The rule is dead for its
  purpose and now only widens two branches that already agree. `docs/spec.md:579-588` still documents the
  removed rule.
- E5: a join is `join_fill` (`tyvar.rs:548`), equality up to open slots. Wrap tolerance comes only from
  `none_sibling_slot` / `branch_slot` (`setup.rs:2979-2999`), which fire when a literal `None` is written
  (`is_none_lit`). The same trigger is missing for `!e` beside a `T!E` sibling.
- E6: one site. The `?.` lowering for an optional receiver wraps the call in `Some(..)`
  (`desugar/mod.rs:1651-1683`), a value position, so a method returning nothing is rejected. A `T!E`
  receiver is lowered differently and works.

**The grids** are in the investigation notes: slot kind (19 rows: typed binding, fn argument, six generic
shapes, struct constructor, static method, method, field assignment, tuple element, list element, map
value, return, default parameter) x value kind (plain value to wrap, `?x`, `!e`, `None`, untyped closure,
closure into an optional fn slot, width constant, empty collection). The wrong cells sit in three
places: closures into a carrier slot (every row), the six generic rows, and `b: int8? = id(300)`.

**Past patches:** at least eleven tickets extended the expected type or the wrap one slot kind at a time
(TICKET-025, 054, 034, 094, 106, 107, 124, 225, 227, 228, 234).

**Wrong assumptions:** a closure's expected type is a fn type handed over as a parameter; a declared `T`
slot is an unpinned slot even when the caller wrote the type argument; an un-annotated branch join must
default its error type.

**Single source:** the expected type has one channel. A closure reads `expected_hint` like a list
literal does, through one shared one-layer carrier strip. A generic call substitutes its explicit type
arguments first and gives every argument whose substituted slot is concrete the owned slot.
`default_expr_result_e` is deleted. With Family A's variables, the annotation-only generic cells
(`c: Box[int?] = Box(5)`, `b: int8? = id(300)`) become "infer, then solve".

**Owner decision:** does `T?` beside a plain `T` join without a written `None` (`[w, 3]`,
`if c: w else: 5`)? The design sanctions only "a literal `None` beside a value". The smallest-depth rule
decided for TICKET-234 would give `List[int?]`.

## Family F — parser, three independent sites (F1, F2, F3)

- F1: `parse_stmt` treats a statement-initial `fn` as a declaration (`src/parser/mod.rs:615`), with no
  lookahead for `fn (`. The inline block (`:2508-2521`) calls the full statement parser after a four-token
  deny-list, while `docs/grammar.bnf:144` says `"COLON" <simpleStmt>`. So it also over-accepts:
  `fn outer(): fn inner(): pass`, `fn o(): defer print(1)`. The conformance test did not catch it because
  it runs the grammar and the parser over 118 hand-written corpus files and never samples the grammar.
- F2: the lexer makes `!` + `=` one token (`src/lexer/mod.rs:1043`). `parse_type_postfix`
  (`parser/mod.rs:2681-2711`) already accepts one fused token (`??`); `!=` needs the same and must leave
  an `=` behind.
- F3: `parse_else_guard` (`parser/mod.rs:1953`) has three callers (typed binding, bare expression
  statement, single-name `:=`). A destructuring `:=`, an assignment, a compound assignment and a `return`
  do not call it, and the grammar agrees. Where the guard is legal is an owner decision; the destructuring
  `:=` has a Rust twin (`let Ok((a, b)) = f() else { .. }`).

These share no mechanism. Each is a small fix at one site. The conformance gap is the only structural
item: the harness should sample the grammar.

## Messages (G1)

`Ty::Unknown` still prints as a bare `?`, which now reads as carrier syntax (`?!str`, `fn(?) -> ?`); the
open-`None` case prints `<unknown>?`. A `??` mismatch reports "branches have incompatible types"
(`pattern.rs:1621-1627`, because `??` is lowered to a match). A variant-import clash cites the removed
`Option` (`setup.rs:1719-1728`). The first one closes with Family A (an unknown that survives to a message
is an error-cascade value and should not be printed as a type); the other two are one-line wording fixes.

## Plan

One structural ticket per family, each with a whole-grid test, in this order:

1. **Family B** (false deadlock). A regression on `main`, 10/10 at two workers. Smallest structural change.
2. **Family C** (`None` keyword and one binding predicate). Small; removes a silent wrong value.
3. **Family A** (open slots become type variables). The largest; needs the three owner decisions above
   before planning.
4. **Family E** (one channel for the expected type). Plan after A, because the generic rows reuse A's
   variables. E1 (`default_expr_result_e`) and E6 can land first as deletions.
5. **Family D** (freshness of the value graph).
6. **Family F** and the two wording fixes: in place.
