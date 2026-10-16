# Progress: old header and miscellaneous sections (moved verbatim from `PROGRESS.md` lines 1-8, 15821-15947)

# Chezzi — Progress Tracker

Single source of truth for "what am I doing next." Update after every work session.

> **Reading note.** Entries dated before **2026-08-16** that measure `--serial`, compare `serial`
> against `M:N`, or call `--serial` "the parity oracle" are **dated records of what was true then**
> and are kept verbatim — that is what this tracker is for. Since 2026-08-16 there is **one engine**
> and no cross-engine gate; see the entry directly below.
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
  checker); first-class compiled `Regex` — deferred, blocked on Level-3 Userdata (see `docs/reference/spec.md`).

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
  `<primary>`/`<params>`/`<argList>` productions in `docs/reference/grammar.bnf` (conformance green). Golden:
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

- **M25 — killable subprocesses** (spec written 2026-08-12, not implemented). Cancellation is
  cooperative, so a task inside `process.cmd("sleep 5")` ignores a sibling fault (5013 ms), `os.exit`
  (5015 ms) and `chezzi test --timeout=500` (5008 ms) — measured. We own a child's PID, so we can end
  what is being waited on, exactly as Go's `exec.CommandContext` does. **`std.process` only**:
  `std.request` wants a client-side timeout, and `std.fs`/`std.io` stay uninterruptible because Go
  does not cancel those either. Full design, the three measured traps (process **group** not PID; kill
  `.output()`; `TERM`→grace→`KILL`), the ambient-vs-explicit divergence from Go, the four phases and
  the verification bar: [`docs/reference/concurrency.md` §6h](docs/reference/concurrency.md).

- VM/GC optimizations beyond M19 — NaN-boxing (own milestone), register VM, generational/incremental GC,
  Cranelift AOT/JIT. Written up in [`docs/design/roadmap.md`](docs/design/roadmap.md).
- **Bug-discovery track (pre-JIT)** — automated bug finding. ✅ **CPython output-differential built**
  (`src/difftest/`, see Current focus). Remaining: cargo-fuzz parser (lever #1), Miri/sanitizers,
  proptest, metamorphic. Ranked plan + rationale in [`docs/contributing/bug-discovery.md`](docs/contributing/bug-discovery.md).
  Recommended to stand up Tier 1 before the JIT, so the reference semantics are fuzzed + differentially validated first.
- ~~**M-C — implicit nurseries**~~ — **shipped 2026-06-12** (see Concurrency above).

### Ideas — record-only (not scheduled)

- **Native FFI / Rust-library bindings** — let Chezzi call into Rust libs; design sketch in `docs/reference/spec.md`
  → *Standard library* → "Future idea — native FFI". **Dynamic C-ABI FFI v1 has since shipped** (`extern
  "lib":` scalar calls via dlopen+libffi — see "Done" below; **plus opaque `ptr` handles, `char*`
  ownership (`owned_str`/`str?`), flat-scalar structs by value, and sync scalar callbacks — all
  shipped**); remaining surface (nested structs-by-value, `str` struct fields, stored/cross-thread
  callbacks + pointer-deref builtins, varargs, the rich Rust `Box<dyn Any>` userdata handle) is still
  deferred.

---

## Known friction / open (document-only)

Surfaced by coverage passes; no `src/` changes pending, recorded for when they bite:

- **Top-level `return` — open owner question (TICKET-178).** `return` at module level parses and silently ends module execution (`print(1)` / `return` / `print(2)` prints `1`, exit 0); CPython rejects it with `SyntaxError: 'return' outside function`. Undocumented; left unchanged pending an owner decision.
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
