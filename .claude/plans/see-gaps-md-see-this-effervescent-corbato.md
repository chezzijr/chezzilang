# Plan: Index & Field Assignment (gaps.md #1 + #2)

## Context

`examples/stats.chz` and probing surfaced the two highest-leverage blocking gaps:
mutable arrays (`xs[i] = v`) and mutable struct fields (`p.x = 5`) are rejected. The
parser already accepts both as assignment targets and `docs/grammar.bnf` already defines
the `<lvalue>` rule (IDENT | `<postfix> DOT IDENT` | `<postfix> LBRACKET expr RBRACKET`).
The block is **front-end-only**: the checker rejects non-`Ident` targets, and neither
engine has a write path. Both storage models already support in-place mutation
(interp: `Rc<RefCell<Vec<…>>>`; VM heap: `get_mut` over `Obj::List`/`Obj::Struct`).

Goal: allow `xs[i] = v`, `p.x = v`, and their `+=`/`-=` compounds, on both engines,
with full TDD, cross-engine parity, conformance corpus, and a golden example. Strings
stay immutable (`s[i] = …` rejected). Scope is **only #1 + #2** — no HOF/methods/map.

TDD order per layer: write the failing test first, then implement.

---

## 1. Checker — `src/checker/mod.rs`

Replace the `_ =>` reject in `check_assign` (lines 605–619) with explicit `Field`/`Index`
arms. Do **not** reuse `infer_field`/`infer_index` wholesale — they permit cases that are
illegal as targets (struct *methods*, *module* members, *string* index). Write target-
specific logic:

- **`ExprKind::Field { obj, name }`**: `let obj_ty = self.infer(obj)`. If
  `Ty::Struct(sname)` and `sname` has a data field `name` → `check_assign_value(field_ty,
  op, &val_ty, span)`. Methods / module members / non-struct → error
  (`"cannot assign to field '{name}' of {obj_ty}"` or `"type {obj_ty} has no field
  '{name}'"`). `Ty::Unknown` → silent (already errored upstream).
- **`ExprKind::Index { obj, index }`**: `self.expect_int(index, "index")`; match
  `self.infer(obj)`: `Ty::List(elem)` → `check_assign_value(&elem, op, &val_ty, span)`;
  `Ty::Str` → error `"cannot assign to an index of str (strings are immutable)"`;
  `Ty::Unknown` → silent; other → `"cannot index-assign into {other}"`.

`check_assign_value` (lines 621–639) already handles `=`/`+=`/`-=` type rules — reuse as-is.

**Tests** (`src/checker/tests.rs`, `ok`/`rejects` helpers):
`ok` — `xs[0] = 9`, `xs[0] += 1`, struct `p.x = 5`, `p.x += 1`.
`rejects` — `xs[0] = "a"` on `list[int]` ("cannot assign"); `s[0] = "x"` on str
("strings are immutable"); `p.nope = 1` ("no field"); `p.method = 1` (method target).

## 2. Interpreter — `src/interp/mod.rs`

Extend `exec_assign` (966–1004). Keep the `Ident` path. Add:

- **`ExprKind::Index { obj, index }`**: `eval(obj)` → expect `Value::List(items)`;
  `eval_int(index)` → `i`; bounds-check against `items.borrow().len()` (reuse the existing
  `"index {idx} out of bounds (len {len})"` message from the read path, 208–235). For `=`
  write `items.borrow_mut()[i] = rhs`; for `+=`/`-=` read current via `borrow()`, apply
  `eval_binary(Add/Sub, cur, rhs, span)`, then write. Non-list / str target → runtime error.
- **`ExprKind::Field { obj, name }`**: `eval(obj)` → expect `Value::Struct { fields, .. }`;
  `borrow_mut()`, find `(k,_)` where `k == name`; `=` replaces, `+=`/`-=` reads-then-writes
  via `eval_binary`. Missing field / non-struct → runtime error mirroring the read path
  (184–206). Avoid holding a `borrow_mut` across `self.eval` — eval `rhs` first.

Each subexpression is evaluated **once** (no double-eval of side effects) — the VM must match.

**Tests** (interp `#[cfg(test)] mod tests`, `run` helper): in-place set + print;
swap via index; compound `xs[0] += 5`; struct field set + read back; field `+=`.

## 3. VM — opcodes, compiler, execution

### 3a. Opcodes — `src/vm/op.rs` (after `GetIndex`, ~line 120)
Add four `Op` variants (Debug-derive only; no Display/disasm to update):
- `SetIndex` — stack `[obj, idx, val]` → `[]`, mutates `Obj::List`.
- `SetField(String)` — stack `[obj, val]` → `[]`, mutates `Obj::Struct`.
- `Dup` — `[a]` → `[a, a]` (for compound field).
- `Dup2` — `[a, b]` → `[a, b, a, b]` (for compound index).
  (`Value` is `Copy` for refs/ints, so Dup copies already-computed values — no re-eval.)

### 3b. Compiler — `src/compiler/mod.rs` `compile_assign` (256–273)
Keep the `Ident` path. Add `Field`/`Index` arms:
- `obj.f = v`  → compile obj; compile v; `SetField(f)`.
- `obj.f OP= v` → compile obj; `Dup`; `GetField(f)`; compile v; `Add`/`Sub`; `SetField(f)`.
- `obj[i] = v`  → compile obj; compile i; `AsInt`; compile v; `SetIndex`.
- `obj[i] OP= v`→ compile obj; compile i; `AsInt`; `Dup2`; `GetIndex`; compile v;
  `Add`/`Sub`; `SetIndex`.

### 3c. Execution — `src/vm/mod.rs` `step()` (add arms by `Op::GetIndex` ~468)
- `Op::Dup` / `Op::Dup2` — inline stack copies.
- `Op::SetIndex => self.set_index(span)?` — pop val, pop idx(Int), pop obj(`Value::Obj`);
  `heap.get_mut(h)` → `Obj::List`; bounds-check with the same out-of-bounds message as
  `get_index` (994–1027); `items[i] = val`. Non-list / OOB → `self.err(...)`.
- `Op::SetField(name) => self.set_field(name, span)?` — pop val, pop obj; `heap.get_mut(h)`
  → `Obj::Struct`; find field, update; mirror `get_field` errors (964–992) for missing
  field / non-struct.

**Tests**: VM unit tests (same snippets as interp) + add every snippet to the
`parity_tests` module so VM and interp stdout/errors agree.

## 4. Conformance corpus — `tests/corpus/accept/`
Add `index_assign.chz` and `field_assign.chz` (header `# rule: assignStmt`) exercising
`=` and `+=` on index and field. Grammar already permits them, so bnf-vs-hand-parser
agreement holds. (str-immutability is a checker reject, covered by unit tests, not parse-
level corpus.)

## 5. Golden example — `examples/`
Add `examples/mutate.chz` + `examples/mutate.expected`: an in-place algorithm using both
features (e.g. counting sort or in-place swap loop + a struct accumulator mutated in a
loop). Wire golden + parity asserts following the existing `golden_hello_chz_*` /
`*_matches_interpreter` pattern in `src/vm/mod.rs`.

---

## Verification
- `cargo test` (checker/interp/vm unit + parity + conformance all green).
- `cargo test conformance` (grammar drift check passes with new corpus).
- `cargo build` + `cargo clippy` clean.
- End-to-end both engines:
  `cargo run -- run examples/mutate.chz` and
  `cargo run -- run examples/mutate.chz --interp` → identical to `mutate.expected`.
- Manual smoke: `xs[i]=v`, `p.x=v`, `xs[i]+=1`, `p.x-=1`; confirm `s[0]="x"` is a
  type error and out-of-bounds index assign is a clean runtime error on both engines.
- Update `gaps.md` (move #1, #2 to 🟢 Verified) and `PROGRESS.md`; single-line
  conventional commit.

## Out of scope
HOF params (#3), list methods (#4), map (#5), literal/wildcard match (#6),
break/continue (#7), tuples (#8). No grammar.bnf change (lvalue rule already correct).
