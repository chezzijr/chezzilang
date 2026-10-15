# Owner index

Look up a fact here BEFORE you grep. Each fact has ONE owner. Read the owner, call it, and never
write a second copy next to it (CLAUDE.md "Derive, don't mirror").

- **Owner**: the one function, table or type that decides the fact, written `` `name` (`path`) ``.
  Paths are `src/`-relative. `cargo test --test source_layout` checks that each named file exists
  and defines the named item, so a move that forgets this index fails the build.
- **DUPLICATE**: the fact has more than one owner today. The row lists every copy. Do not add a
  copy. If your change touches the fact, delete a copy (or ask) instead of patching one.
- A new fact gets a row in the same commit that adds its owner.

## Syntax and parsing

| Fact | Owner | Notes |
|---|---|---|
| Where a block ended (dedent, `else`, EOF) | `ended_at_block` (`parser/cursor.rs`) | Family A |
| Where a block expression may start (value site) | `parse_value` (`parser/expr.rs`), `open_value` (`parser/expr.rs`) | U8 |
| A removed spelling used as a bare name | `reject_removed_name` (`parser/mod.rs`) | |
| Constant value of an expression | `eval` (`ast/consteval.rs`) | overflow lint, width check, const |
| Fewest args a decl may be entered with | `min_callable_params` (`ast/mod.rs`) | DUPLICATE: `optional_tail` (`checker/mod.rs`) and its runtime copy in `native/mod.rs` |
| `_` wildcard | DUPLICATE: `bind_leaf` (`parser/pattern.rs`), `parse_pattern` (`parser/pattern.rs`) and the wait-arm head in `parser/stmt.rs`, `is_blank` (`ast/mod.rs`), `collect_module_globals` (`checker/globals.rs`), the recover binder in `compiler/expr.rs`, the ident read in `checker/infer_ops.rs` | target: one `ast::is_blank_name` |
| Valid assignment target | DUPLICATE: `is_assign_target` (`parser/pattern.rs`), `check_assign` (`checker/assign.rs`), the store matches in `compile_assign` (`compiler/assign.rs`) | target: `ExprKind::is_place` |
| `head[X]` is a type application | `is_named_head` (`ast/mod.rs`) | DUPLICATE: an inline copy in `parse_postfix` (`parser/expr.rs`) and 2 checker probes |
| Grammar | `docs/grammar.bnf` | DUPLICATE with the hand parser; `conformance.rs` compares accept/reject only; waits on G5 |
| Format mini-language (`{x:spec}`), float display | `repr_float` (`fmtspec.rs`) | |
| Slice and negative index | `norm_index` (`slice.rs`), `range_indices` (`slice.rs`) | |

## Names and resolution

| Fact | Owner | Notes |
|---|---|---|
| What a name head means (Resolution per NodeId) | `resolve_path` (`checker/resolve.rs`), `classify_path` (`checker/resolve.rs`), `commit_resolution` (`checker/resolve.rs`) | TICKET-180/222 |
| Module slot facts (kind, fn decl, keyword labels) | `GlobalBinding` (`checker/globals.rs`), `labels_certain` (`checker/globals.rs`) | TICKET-186 |
| Does a head name a user type, which kind | `type_head` (`checker/scope.rs`) | |
| Builtin callable names | `PRELUDE` (`checker/mod.rs`) | DUPLICATE: `RESERVED_CALLABLE` (`checker/mod.rs`), `REMOVED_CALLABLE` (`checker/mod.rs`), `BUILTIN_CALLEES` (`checker/resolve.rs`), the arms of `infer_builtin_call` (`checker/builtin_call.rs`), the VM name matches in `do_builtin` (`vm/builtins.rs`) and `invoke_value` (`vm/call.rs`) |
| Builtin type names | `from_name` (`checker/builtin_type.rs`) | DUPLICATE: `is_builtin_type_name` (`checker/builtin_type.rs`), `is_reserved_type` (`checker/mod.rs`), the `Option`/`Result` names in `checker/exhaust.rs` |
| Is this param the receiver (`self`) | `receiver` (`checker/sig.rs`) | DUPLICATE: `is_static_method` (`compiler/mod.rs`) and `"self"` tests in desugar, setup, proto and the parser |
| Checker↔compiler keys (type key, globals, fn names, witness refs) | DUPLICATE: `type_key` (`checker/setup.rs`) and `type_key` (`compiler/mod.rs`), `entry_module_binds` (`checker/tables.rs`) and `collect_globals` (`compiler/mod.rs`) | target: checker tables the compiler reads |
| Unused local | `unused_locals` (`checker/unused.rs`) | |
| Native module path to its `std.*` name | `native_name` (`native/mod.rs`) | |

## Types and protocols

| Fact | Owner | Notes |
|---|---|---|
| Type lattice | `Ty` (`checker/ty.rs`) | |
| Type variables, solving | `solve` (`checker/tyvar.rs`) | |
| Is a fn value fully pinned | `pin_generic_fn_value` (`checker/ty_util.rs`) | |
| Is `a` assignable to `b` | `assignable` (`checker/assignable.rs`) | |
| Does a type satisfy a protocol | `satisfies` (`checker/satisfies.rs`) | |
| Does the run time dispatch a hook | `declares_hook` (`checker/hooks.rs`) | |
| Operator hooks (hook → protocol → method) | `HOOKS` (`checker/tables.rs`), `Hook` (`checker/tables.rs`) | |
| Operator → protocol name | DUPLICATE: `binary_protocol` (`checker/ty_util.rs`), `RESERVED_PROTOCOLS` (`checker/mod.rs`), the seed loop in `prebuilt_protocols` (`checker/ty_util.rs`), `INTRINSIC_PROTO_METHODS` (`checker/proto.rs`), `builtin_arith_kind` (`checker/op_sendable.rs`), literals in `infer_unary` (`checker/infer_ops.rs`), the VM label maps in `vm/arith.rs`, `vm/method_call.rs` and `vm/shared_methods.rs` | target: `HOOKS` rows carry the operator |
| What an Iterable yields | DUPLICATE: `iterable_elem` (`checker/iter_index.rs`), `iter_elem` (`checker/iter_index.rs`), `struct_iter_elem` (`checker/iter_index.rs`), `struct_iterable_elem` (`checker/iter_index.rs`), `for_bindings` (`checker/iter_index.rs`), the for-lowering in `compiler/loops.rs` | waits on the U6 design |
| Does a write change a value no name holds | `temporary_write` (`checker/fresh.rs`) | TICKET-254 |
| Can an operator result alias an operand | `aliases_operand` (`checker/fn_writes.rs`) | Family C |
| Writes rooted at `self` | `checker/self_writes.rs` | D4 rule 2 |
| Match exhaustiveness, enum ctor domain | `checker/exhaust.rs` | |
| Control-flow fall-through | `Flow` (`checker/flow.rs`) | TICKET-184 |
| Width of a struct field (FFI) | `note_field_widths` (`checker/setup.rs`) | `FieldWidthTable` |
| C width ranges, vararg promotion | `vararg_promoted` (`native/cffi.rs`), `int_range` (`native/cffi.rs`) | |
| Builtin method set per type | `std/prelude.chz` native decls | DUPLICATE: the hover lists such as `STR_METHODS` (`checker/builtin_methods.rs`), `builtin_container_sig` (`checker/builtin_methods.rs`), the VM name dispatch in `core_method` (`vm/core_method.rs`), `vm/socket.rs`, `vm/shared_methods.rs` and `vm/fileio.rs` |
| Native method mutates its receiver | `is_mutating_native_kind` (`vm/call.rs`) | |

## Patterns and bindings

| Fact | Owner | Notes |
|---|---|---|
| Names a pattern binds | DUPLICATE: `binders` (`ast/mod.rs`), `collect_binding_names` (`compiler/pattern.rs`), `first_duplicate_binder` (`checker/ty_util.rs`) | |
| Bare name: binder or nullary variant | DUPLICATE: `binders` (`ast/mod.rs`) (always a binder), `pat_is_variant` (`compiler/pattern.rs`), `bare_pattern_name` (`checker/pattern.rs`) | target: one `bare_pattern_name` record |

## Runtime

| Fact | Owner | Notes |
|---|---|---|
| Value tag layout | `vm/value.rs` | |
| Bytecode | `Op` (`vm/op.rs`) | |
| `==` between values | `values_equal_guarded` (`vm/equality.rs`) | |
| May this op block here, and how | `mode` (`vm/block.rs`), `block_ctx` (`vm/block.rs`) | TICKET-181 |
| CPU width / runner permits | `RUNNERS` (`vm/width.rs`) | TICKET-205/230 |
| Quiescence verdict | `decide_since` (`vm/quiesce.rs`) | |
| Airlock crossing policy | `vm/crossing.rs` | D4 |
| Cross-heap store check | `ensure_crossable` (`vm/worker_vm.rs`), `to_wire_crossable` (`vm/airlock_out.rs`) | a missed guard is UB |
| `WireValue` walks | DUPLICATE: `wire_summary` (`vm/core.rs`), `collect_core_gcrefs` (`vm/core.rs`), the `to_wire` budget in `vm/airlock_out.rs` | |
| Hook calls | `invoke_hook` (`vm/method_call.rs`) | DUPLICATE: direct `run_proto` (`vm/frames.rs`) calls in `vm/call.rs`, `vm/method_call.rs` and `vm/step.rs`; waits on Family D |
| Shared/RwShared box heap | `vm/boxctx.rs` | |
| Range value at run time | `as_range` (`vm/range.rs`) | |
| How a native fn runs | `Kind` (`native/mod.rs`) | the third element of each `MEMBERS` row |

## Tests

| Fact | Owner | Notes |
|---|---|---|
| Unit-test fixture temp directory | `TmpDir` (`test_support.rs`) | one counter, so two live fixtures never share a path; `main.rs` and `tests/` are other crates and keep their own |
| VM production sources for a source-text rule | `vm_sources` (`vm/tests/mod.rs`) | |

## File map

Put new code in the file that owns its topic. Each file's first line names its topic; read that
line (or this map) before you open the file. `cargo test --test source_layout` caps a production
file at 2,500 lines and a test file at 3,500.

| Area | Files |
|---|---|
| Lexer | `lexer/mod.rs` (tokens), `lexer/tests.rs` |
| Parser | `parser/mod.rs` (entry points, `Parser`, value sites), `parser/cursor.rs` (peek, expect, line and block ends), `parser/stmt.rs` (items, statements, imports), `parser/decl.rs` (fn, protocol, extern, native, struct, enum), `parser/control.rs` (if, for, while, match, else guards), `parser/pattern.rs` (patterns and binding targets), `parser/types.rs` (blocks, types), `parser/expr.rs` (Pratt expressions), `parser/interp.rs` (interpolation, closures), `parser/tests/` |
| AST | `ast/mod.rs` (node shapes and pure queries), `ast/consteval.rs` |
| Desugar, resolver | `desugar/mod.rs`, `resolver/mod.rs`, `resolver/std_embed.rs` |
| Checker: entry and tables | `checker/mod.rs` (`Checker`, reserved names, sig structs), `checker/entry.rs` (check entry points, call/compile table passes), `checker/tables.rs` (`HOOKS`, table types, witness keys), `checker/graph_pass.rs`, `checker/setup.rs` (keys, aliases, names) |
| Checker: pass 1 | `checker/harvest.rs` (stdlib and native harvest), `checker/hoist.rs` (collect and hoist decls), `checker/module_imports.rs` (diagnostics, import binding), `checker/scope.rs` (scopes, lookup, heads), `checker/writes.rs` (captures, task writes) |
| Checker: signatures and types | `checker/sig.rs` (receivers, `fn_sig`), `checker/infer_ret.rs`, `checker/witness_sig.rs`, `checker/resolve_type.rs`, `checker/resolve_ctype.rs`, `checker/ty.rs`, `checker/ty_util.rs`, `checker/tyvar.rs`, `checker/builtin_type.rs`, `checker/builtin_methods.rs` |
| Checker: protocols | `checker/proto.rs` (intrinsic receivers, protocol hoisting), `checker/assignable.rs`, `checker/satisfies.rs`, `checker/hooks.rs`, `checker/op_sendable.rs`, `checker/eq.rs`, `checker/targs.rs`, `checker/generic_call.rs` |
| Checker: statements | `checker/stmt.rs`, `checker/assign.rs`, `checker/fn_body.rs`, `checker/fresh.rs`, `checker/flow.rs`, `checker/unused.rs`, `checker/fn_writes.rs`, `checker/self_writes.rs` |
| Checker: expressions | `checker/infer.rs` (core), `checker/infer_ops.rs` (literals, operators), `checker/infer_access.rs` (field, index, carriers), `checker/fn_value.rs`, `checker/closure.rs`, `checker/branch.rs` (wait, match, if) |
| Checker: calls | `checker/expr.rs` (call dispatch), `checker/ctor_call.rs`, `checker/builtin_call.rs`, `checker/fn_call.rs`, `checker/method_call.rs`, `checker/args.rs` |
| Checker: patterns, names | `checker/pattern.rs`, `checker/exhaust.rs`, `checker/iter_index.rs`, `checker/resolve.rs`, `checker/globals.rs`, `checker/suggest.rs` |
| Checker tests | `checker/tests/` (one file per topic, helpers in `checker/tests/mod.rs`), `checker/graph_tests.rs` |
| Compiler | `compiler/mod.rs` (entry, `Compiler`, keys, globals), `compiler/fn_body.rs`, `compiler/stmt.rs`, `compiler/assign.rs`, `compiler/loops.rs`, `compiler/pattern.rs`, `compiler/expr.rs`, `compiler/witness.rs`, `compiler/call.rs`, `compiler/capture.rs` (free variables), `compiler/fncomp.rs`, `compiler/peephole.rs`, `compiler/tests/` |
| VM: core | `vm/mod.rs` (types, errors, traces, module declarations), `vm/value.rs`, `vm/op.rs`, `vm/heap.rs`, `vm/chzstr.rs`, `vm/entry.rs` (run entry points), `vm/host.rs` |
| VM: execution | `vm/exec.rs` (setup, invoke), `vm/frames.rs` (frames, halts), `vm/step.rs` (dispatch loop), `vm/call.rs` (calls, natives), `vm/method_call.rs` (methods, hooks), `vm/json_decode.rs` |
| VM: values | `vm/arith.rs`, `vm/compare.rs`, `vm/equality.rs`, `vm/hash.rs`, `vm/field_index.rs`, `vm/stmt.rs` (defer, unwind, structs, enums), `vm/builtins.rs`, `vm/globals.rs`, `vm/display.rs`, `vm/core_method.rs`, `vm/list_hof.rs`, `vm/range.rs` |
| VM: scheduler | `vm/sched_types.rs`, `vm/mnsched.rs`, `vm/mnsched_take.rs`, `vm/mnsched_park.rs`, `vm/mnsched_verdict.rs`, `vm/sched_core.rs`, `vm/sched.rs` (spawn targets, runner guards), `vm/nursery.rs`, `vm/demote.rs`, `vm/worker_loop.rs`, `vm/worker_vm.rs`, `vm/block.rs`, `vm/width.rs`, `vm/quiesce.rs`, `vm/pool.rs`, `vm/blocking_pool.rs`, `vm/runner_cache.rs`, `vm/sched_seed.rs`, `vm/timer.rs` |
| VM: concurrency values and I/O | `vm/netio.rs` (core accessors, guards), `vm/channel.rs`, `vm/blocking.rs`, `vm/socket.rs`, `vm/shared_methods.rs`, `vm/core.rs`, `vm/boxctx.rs`, `vm/poller.rs`, `vm/fileio.rs`, `vm/stream.rs` |
| VM: airlock | `vm/airlock_out.rs`, `vm/airlock_in.rs`, `vm/wire.rs`, `vm/crossing.rs` |
| VM tests | `vm/tests/`, `vm/golden_tests/`, `vm/gc_tests.rs` |
| Natives | `native/mod.rs` (the seam), one `std.*` module per file such as `native/fs.rs`; `native/cffi.rs` (C FFI) |
| Tools | `editor/mod.rs`, `test_runner.rs`, `test_support.rs`, `conformance.rs`, `difftest/`, `panicfuzz/`, `schedfuzz/` |
