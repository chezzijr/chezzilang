# Owner index

Look up a fact here BEFORE you grep. Each fact has ONE owner. Read the owner, call it, and never
write a second copy next to it (CLAUDE.md "Derive, don't mirror").

- **Owner**: the one function, table or type that decides the fact. Paths are `src/`-relative.
  A path stays valid until the file map below changes; line numbers are deliberately absent.
- **DUPLICATE**: the fact has more than one owner today. The row lists every copy. Do not add a
  copy. If your change touches the fact, delete a copy (or ask) instead of patching one.
- A new fact gets a row in the same commit that adds its owner.

## Syntax and parsing

| Fact | Owner | Notes |
|---|---|---|
| Where a block ended (dedent, `else`, EOF) | `Parser::ended_at_block` — `parser/mod.rs` | Family A |
| Where a block expression may start (value site) | `Parser::parse_value` / `Parser::open_value` — `parser/mod.rs` | U8 |
| A removed spelling used as a bare name | `Parser::reject_removed_name` — `parser/mod.rs` | |
| Constant value of an expression | `consteval::eval` — `ast/consteval.rs` | overflow lint, width check, const |
| Fewest args a decl may be entered with | `min_callable_params` — `ast/mod.rs` | `FnSig::optional_tail` (`checker/mod.rs`) and its runtime copy (`native/mod.rs`): DUPLICATE |
| `_` wildcard | DUPLICATE: `bind_leaf` (`parser/mod.rs`), `parse_pattern` + wait-arm inline (`parser/mod.rs`), `Expr::is_blank` (`ast/mod.rs`), `collect_module_globals` (`checker/globals.rs`), recover binder (`compiler/mod.rs`), ident read (`checker/pattern.rs`) | target owner: `ast::is_blank_name` |
| Valid assignment target | DUPLICATE: `is_assign_target` (`parser/mod.rs`), `check_assign` arm set (`checker/sig.rs`), 3 compiler store matches (`compiler/mod.rs`) | target owner: `ExprKind::is_place` |
| `head[X]` is a type application | `ExprKind::is_named_head` — `ast/mod.rs`; DUPLICATE inline copy in the parser's postfix loop, plus 2 checker probes (`checker/expr.rs`) | |
| Grammar | `docs/grammar.bnf`; DUPLICATE with the hand parser; `src/conformance.rs` compares accept/reject only (`GRAMMAR_LOOSER` exemptions) | waits on G5 |
| Format mini-language (`{x:spec}`) | `fmtspec.rs` | |
| Float display (`repr`) | `repr_float` — `fmtspec.rs` | |
| Slice and negative index | `slice.rs` | |

## Names and resolution

| Fact | Owner | Notes |
|---|---|---|
| What a name head means (Resolution per NodeId) | `resolve_path` / `classify_path` / `commit_resolution` — `checker/resolve.rs` | TICKET-180/222 |
| Module slot facts (kind, fn decl, keyword labels) | `GlobalBinding` + `labels_certain` — `checker/globals.rs` | TICKET-186 |
| Does a head name a user type, which kind | `Checker::type_head` — `checker/setup.rs` | |
| Builtin callable names | DUPLICATE: `PRELUDE`, `RESERVED_CALLABLE`, `REMOVED_CALLABLE` (`checker/mod.rs`), `BUILTIN_CALLEES` (`checker/resolve.rs`), `infer_named_call` arms (`checker/expr.rs`), VM name matches (`vm/stmt.rs`, `vm/call.rs`) | target owner: `PRELUDE` rows |
| Builtin type names | DUPLICATE: `BuiltinType::from_name` / `is_builtin_type_name` (`checker/builtin_type.rs`), `is_reserved_type` (`checker/mod.rs`), `"Option"|"Result"` in `checker/exhaust.rs` | U10 |
| Is this param the receiver (`self`) | `Checker::receiver` — `checker/sig.rs`; DUPLICATE `is_static_method` (`compiler/mod.rs`) and `"self"` tests in desugar, setup, proto, parser | TICKET-250 |
| Checker↔compiler keys (type key, globals, fn names, witness refs) | DUPLICATE: `type_key` (`checker/setup.rs` and `compiler/mod.rs`), `entry_module_binds` vs `collect_globals`, `local_fn_names` vs `fn_names`, `witness_scope` vs `witness_ref` | target: checker tables the compiler reads |
| Unused local | `unused_locals` — `checker/unused.rs` | |
| Near-miss suggestion | `checker/suggest.rs` | |

## Types and protocols

| Fact | Owner | Notes |
|---|---|---|
| Type lattice | `Ty` — `checker/ty.rs` | |
| Type variables, solving | `solve` — `checker/tyvar.rs` | |
| Is a fn value fully pinned | `pin_generic_fn_value` — `checker/mod.rs` | |
| Is `a` assignable to `b` | `assignable` — `checker/proto.rs` | |
| Does a type satisfy a protocol | `satisfies` — `checker/proto.rs` | |
| Does the run time dispatch a hook | `declares_hook` — `checker/proto.rs` | |
| Operator hooks (hook → protocol → method) | `HOOKS` table + `enum Hook` — `checker/mod.rs` | |
| Operator → protocol name | DUPLICATE: `binary_protocol`, protocol seed loop, `RESERVED_PROTOCOLS` (`checker/mod.rs`), `INTRINSIC_PROTO_METHODS`, `builtin_arith_kind` (`checker/proto.rs`), literals + `to_lowercase()` (`checker/pattern.rs`), VM label maps (`vm/arith.rs`, `vm/call.rs`, `vm/netio.rs`) | target owner: `HOOKS` rows carry the operator |
| What an Iterable yields | DUPLICATE: `iterable_elem`, `iter_elem`, `struct_iter_elem`, `struct_iterable_elem`, `for_bindings` (`checker/sig.rs`), compiler for-lowering | waits on U6 design |
| Does a write change a value no name holds | `Checker::temporary_write` — `checker/sig.rs` | |
| Can an operator result alias an operand | `aliases_operand` — `checker/fn_writes.rs` | Family C |
| Writes rooted at `self` | `checker/self_writes.rs` | D4 rule 2 |
| Match exhaustiveness, enum ctor domain | `checker/exhaust.rs` | |
| Control-flow fall-through | `flow::block` / `flow::stmt` — `checker/flow.rs` | TICKET-184 |
| Width of a struct field (FFI) | `Checker::note_field_widths` (`FieldWidthTable`) — `checker/setup.rs` | |
| C width ranges, vararg promotion | `CType` width range + `vararg_promoted` — `native/cffi.rs` | |
| Builtin method set per type | `std/prelude.chz` native decls; DUPLICATE hover lists `STR_METHODS`… (`checker/mod.rs`), `builtin_container_sig` (`checker/mod.rs`), methods outside the table (`checker/expr.rs`), VM name dispatch (`vm/call.rs`, `vm/netio.rs`, `vm/fileio.rs`) | |
| Native method mutates its receiver | `is_mutating_native_kind` — `vm/call.rs` | |

## Patterns and bindings

| Fact | Owner | Notes |
|---|---|---|
| Names a pattern binds | DUPLICATE: `Pattern::binders` (`ast/mod.rs`), `collect_binding_names` (`compiler/mod.rs`), `first_duplicate_binder` (`checker/mod.rs`) | |
| Bare name: binder or nullary variant | DUPLICATE: `Pattern::binders` (always binder), `pat_is_variant` (`compiler/mod.rs`), `bare_pattern_name` (`checker/pattern.rs`) | target owner: `bare_pattern_name` record |

## Runtime

| Fact | Owner | Notes |
|---|---|---|
| Value tag layout | `vm/value.rs` | |
| Bytecode | `Op` — `vm/op.rs` | |
| `==` between values | `values_equal_guarded` — `vm/arith.rs` | |
| May this op block here, and how | `block::mode` + `Vm::block_ctx` — `vm/block.rs` | TICKET-181 |
| CPU width / runner permits | `RUNNERS` — `vm/width.rs` | TICKET-205/230 |
| Quiescence verdict | `QuiesceState::decide_since` — `vm/quiesce.rs` | |
| Airlock crossing policy | `vm/crossing.rs` | D4 |
| Cross-heap store check | `ensure_crossable` / `to_wire_crossable` — `vm/sched.rs` | a missed guard is UB |
| `WireValue` walks | DUPLICATE: `wire_summary`, `collect_core_gcrefs` (`vm/core.rs`), `to_snap` / `to_wire` budget (`vm/sched.rs`) | |
| Hook calls | DUPLICATE: `invoke_hook` beside direct `run_proto` calls (`vm/call.rs`, `vm/exec.rs`) | waits on Family D |
| Shared/RwShared box heap | `vm/boxctx.rs` | |
| Range value at run time | `as_range` — `vm/range.rs` | |
| How a native fn runs | `Kind` in the `MEMBERS` rows — `native/mod.rs` | |

## File map

One line per source file: the topic it owns. Put new code in the file that owns its topic.

| File | Owns |
|---|---|
| `lexer/mod.rs` | source text → tokens |
| `parser/mod.rs` | tokens → AST (all productions; split pending) |
| `ast/mod.rs` | AST node shapes and their pure queries |
| `ast/consteval.rs` | constant evaluation |
| `desugar/mod.rs` | syntactic desugaring before the checker |
| `resolver/mod.rs`, `resolver/std_embed.rs` | module paths and the graph; embedded stdlib |
| `checker/mod.rs` | checker entry, tables, `Checker` struct, type utilities |
| `checker/setup.rs` | pass 1: hoisting declarations, type keys |
| `checker/sig.rs` | signatures, return inference, type resolution, pass-2 statements, assignment, fn bodies, iteration (split pending) |
| `checker/expr.rs` | call inference |
| `checker/pattern.rs` | pattern binding, and expression inference (split pending) |
| `checker/proto.rs` | protocols, assignability, satisfaction, Eq, generic calls (split pending) |
| `checker/{resolve,globals,flow,exhaust,unused,suggest,tyvar,ty,builtin_type,fn_writes,self_writes}.rs` | see the row above that names each |
| `compiler/mod.rs` | AST → bytecode (split pending) |
| `compiler/peephole.rs` | peephole pass |
| `vm/mod.rs` | VM types, M:N scheduler core, host, entry points (split pending) |
| `vm/sched.rs` | spawn, nursery, airlock wire walks, worker VMs (split pending) |
| `vm/{exec,arith,call,stmt,netio,fileio}.rs` | `impl Vm` by op family |
| `vm/{block,width,quiesce,crossing,boxctx,core,wire,heap,value,op,poller,pool,blocking_pool,runner_cache,sched_seed,range,stream,timer,chzstr,fxhash}.rs` | see each file's `//!` header |
| `native/*.rs` | one `std.*` module per file; `native/mod.rs` is the seam |
| `editor/mod.rs` | LSP-independent editor logic |
| `test_runner.rs` | `chezzi test` |
| `difftest/`, `panicfuzz/`, `schedfuzz/` | fuzz oracles (`#[path]`-included by `src/bin` and `tests/`) |
