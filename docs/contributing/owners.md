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

| Constant value of an expression | `eval` (`ast/consteval.rs`) | overflow lint, width check, const |
| Fewest args a decl may be entered with | `min_callable_params` (`ast/mod.rs`) | one owner: the checker's `FnSig.min_params`, the compiler's `Proto::min_arity` and the natives read it |
| Value type of a named fn or builtin | `fn_value_ty` (`checker/mod.rs`) | one builder for every `FnSig` value and hover type; a builtin value is a `Ty::Func`, and the variadic flag is part of the type (TICKET-264) |
| Native decl signature and slots | `harvest_native_fn_sig` (`checker/harvest.rs`) | prelude natives included; `register_native_decl` calls it |
| Call slots of a param list | `slots_of` (`desugar/mod.rs`) | `param_slots` (user decls) and `native_param_slots` (natives, positional-only before the variadic) call it |
| print's separator and terminator | the `native fn print` declaration (`std/prelude.chz`), read by `prelude_print_defaults` (`compiler/call.rs`) | `Program.print_defaults` carries them to `Op::CallPrint` and the VM value arm |
| Is this name the blank `_` | `is_blank_name` (`ast/mod.rs`) | the parser encodes a blank at every binding site (`Pattern::Wildcard`, an `else _:` binder as no binder); `tests/owner_guards.rs` rejects a `== "_"` anywhere else |
| Valid assignment target | `is_place` (`ast/mod.rs`) | the parser admits only places; the non-place arms of `check_assign` (`checker/assign.rs`), `compile_assign` (`compiler/assign.rs`) and `emit_assign_value_first` (`compiler/stmt.rs`) are `internal:` errors, never a second decision |
| May `head[X]` be a type application | `is_named_head` (`ast/mod.rs`) | all three parser sites read it; the checker still probes the receiver twice per chained link (`checker/expr.rs`, U17's remaining half) |
| Grammar | `docs/reference/grammar.bnf` | DUPLICATE with the hand parser; `conformance.rs` compares accept/reject only; waits on G5 |
| Format mini-language (`{x:spec}`), float display | `repr_float` (`fmtspec.rs`) | |
| Slice and negative index | `norm_index` (`slice.rs`), `range_indices` (`slice.rs`) | |

## Names and resolution

| Fact | Owner | Notes |
|---|---|---|
| What a name head means (Resolution per NodeId) | `resolve_path` (`checker/resolve.rs`), `classify_path` (`checker/resolve.rs`), `commit_resolution` (`checker/resolve.rs`) | TICKET-180/222 |
| Module slot facts (kind, fn decl, keyword labels) | `GlobalBinding` (`checker/globals.rs`), `labels_certain` (`checker/globals.rs`) | TICKET-186 |
| Does a head name a user type, which kind | `type_head` (`checker/scope.rs`) | |
| Builtin callable names, ids, lowering, first-classness | `Builtin` (`base/builtin.rs`) | `builtin_callee` (`checker/witness_sig.rs`) is the one reader that resolves a bare callee to an id; `prelude_fn` (`checker/mod.rs`) and `has_opcode_ctor` (`checker/builtin_type.rs`) are views of it. `Resolution::Builtin`, `Op::CallBuiltin`/`Op::LoadBuiltin`, `Obj::Builtin`, `FnKey::Builtin`, `WireValue::Builtin` carry the id, and `infer_builtin_call` (`checker/builtin_call.rs`), `compile_builtin_call` (`compiler/call.rs`), `do_builtin` (`vm/builtins.rs`) and `invoke_value` (`vm/call.rs`) match it exhaustively |
| Builtin type names | `from_name` (`checker/builtin_type.rs`) | a module's own set is `ModuleSig::builtins`; a qualified `m.X` reads it (`module_owns_builtin` (`checker/witness_sig.rs`)) |
| Which std module owns a builtin type (`Shared`, `Socket`, `ptr`, `int32`, …); none for a global one | `owner` (`checker/builtin_type.rs`), `builtin_owner` (`checker/builtin_type.rs`) | an owned type is an ordinary member of its module: `module_builtins` (`checker/builtin_type.rs`) lists them |
| What a bare builtin type name means in this module | `builtin_type_named` (`checker/witness_sig.rs`), `bound_builtin` (`checker/witness_sig.rs`) | a global row, or a module-owned one an import bound (alias included, `tests/module_member_grid.rs`) |
| Names a program may not declare | `is_keyword` (`lexer/mod.rs`) | keywords only. A module binds a name once per namespace (type or value; a top-level `:=` is a value declaration): `clashes` (`checker/globals.rs`) over the `GlobalBinding` records `collect_module_globals` (`checker/globals.rs`) builds for every declaration and import; `note_import_bind` (`checker/module_imports.rs`) owns two imports of one name. A name repeated in one binding target or parameter list: `reject_repeated_binders` (`parser/pattern.rs`), `parse_params_ext` (`parser/decl.rs`); in a match pattern, `first_duplicate_binder` (`checker/ty_util.rs`). `tests/redeclaration_grid.rs` runs every cell. Every other name a module declares or imports shadows the builtin of that name in the module (`tests/shadowing_grid.rs`); lookup tries this module's own declarations before builtins (`declares_type_here` (`checker/resolve_type.rs`), `classify_ident` (`checker/resolve.rs`), `protocol_key` (`checker/setup.rs`)) |
| The carriers' own names (`Option`, `Result`, `Some`, `Ok`, `Err`) | `private_name` (`ast/mod.rs`) | a user module reaches the carriers only through `T?`/`T!E`/`?v`/`!e`, which the parser and desugar spell privately; `seed_carrier_enums_from` (`checker/harvest.rs`) binds them so |
| Is this name / param the receiver `self` | `is_self_name` (`ast/mod.rs`), `SELF` (`ast/mod.rs`) | `Param::is_self` and `FnDecl::takes_self` read it; whether a `self` param is a VALID receiver is `receiver` (`checker/sig.rs`); `tests/owner_guards.rs` rejects a `== "self"` anywhere else |
| A user type's identity key `<module-key>::Name` | `user_type_keys` (`resolver/mod.rs`) | the checker's and the compiler's `type_keys` tables are both built from it |
| The names a module binds as run-time globals, in slot order | `module_global_binds` (`resolver/mod.rs`) | the compiler's `collect_globals` builds the slot table from it; the checker's `entry_module_binds` (`checker/tables.rs`) reads it. DUPLICATE still: `collect_module_globals` (`checker/globals.rs`) walks the same statements for its per-slot records |
| Unused local | `unused_locals` (`checker/unused.rs`) | |
| Native module path to its `std.*` name | `native_name` (`native/mod.rs`) | |

## Types and protocols

| Fact | Owner | Notes |
|---|---|---|
| Type lattice | `Ty` (`checker/ty.rs`) | |
| Type variables, solving | `solve` (`checker/tyvar.rs`) | |
| Is a fn value fully pinned | `pin_generic_fn_value` (`checker/ty_util.rs`) | |
| Which type params an expected type fills (a protocol hint fills none) | `hint_bindings`, `hint_fixes` (`checker/ty_util.rs`) | read by `slot_seed` and `widen_targs_from_hint` (`checker/targs.rs`), `seed_from_hint` and `pin_generic_fn_value` (`checker/ty_util.rs`), `generic_fn_value_ty` (`checker/fn_value.rs`). `tests/generic_hint_grid.rs` runs every cell |
| Is `a` assignable to `b` | `assignable` (`checker/assignable.rs`) | |
| Does a type satisfy a protocol | `satisfies` (`checker/satisfies.rs`) | |
| Does the run time dispatch a hook | `declares_hook` (`checker/hooks.rs`) | |
| Operator hooks (hook → protocol → method) | `HOOKS` (`base/hooks.rs`), `Hook` (`base/hooks.rs`) | |
| Operator → hook → protocol and method | `of_binary` (`base/hooks.rs`), `HOOKS` (`base/hooks.rs`) | `binary_protocol` (`checker/ty_util.rs`), the protocol shapes harvested by `seed_std_protocols` (`checker/harvest.rs`) from the `protocol` decls in `std/*.chz` and the operator typing in `checker/infer_ops.rs` derive from it. DUPLICATE still: the method column of `INTRINSIC_PROTO_METHODS` (`checker/proto.rs`) |
| Which std module declares a protocol, and is its name global | `std_protocol_owner` (`checker/graph_pass.rs`, re-exported as `checker::std_protocol_owner`), read from the `protocol` decls in `std/*.chz` | `is_std_protocol` and `is_global_protocol` (`checker/mod.rs`), `user_protocol_key` (`checker/setup.rs`), the owner of the builtin type Iterator (`checker/builtin_type.rs`) derive from it. No other list of protocol names exists. |
| Which builtin type satisfies which operator protocol | `INTRINSIC_PROTO_METHODS` (`checker/proto.rs`) | `builtin_arith_kind` (`checker/op_sendable.rs`) derives from it (`builtin_operator_protocol_grid`) |
| Overflow fault label of an arithmetic op | DUPLICATE: `checked` in `ast/consteval.rs`, the op labels in `vm/arith.rs` and `vm/shared_methods.rs` | |
| What an Iterable yields | DUPLICATE: `iterable_elem` (`checker/iter_index.rs`), `iter_elem` (`checker/iter_index.rs`), `struct_iter_elem` (`checker/iter_index.rs`), `struct_iterable_elem` (`checker/iter_index.rs`), `for_bindings` (`checker/iter_index.rs`), the for-lowering in `compiler/loops.rs` | waits on the U6 design |
| Does a write change a value no name holds | `temporary_write` (`checker/fresh.rs`) | TICKET-254 |
| Can an operator result alias an operand | `aliases_operand` (`checker/fn_writes.rs`) | Family C |
| Writes rooted at `self` | `checker/self_writes.rs` | D4 rule 2 |
| Match exhaustiveness, enum ctor domain | `checker/exhaust.rs` | |
| Control-flow fall-through | `Flow` (`checker/flow.rs`) | TICKET-184 |
| Width of a struct field (FFI) | `note_field_widths` (`checker/setup.rs`) | `FieldWidthTable` |
| C width ranges, vararg promotion | `vararg_promoted` (`native/cffi.rs`), `int_range` (`native/cffi.rs`) | |
| Builtin method set per type | `std/prelude.chz` native decls | DUPLICATE: the hover lists such as `STR_METHODS` (`checker/builtin_methods.rs`), `builtin_container_sig` (`checker/builtin_methods.rs`), the VM name dispatch in `core_method` (`vm/core_method.rs`), `vm/socket.rs`, `vm/shared_methods.rs` and `vm/fileio.rs` |
| Native method mutates its receiver | `is_mutating_native_kind` (`native/mod.rs`) | |

## Patterns and bindings

| Fact | Owner | Notes |
|---|---|---|
| The bare-name nodes of a pattern | `bare_names` (`ast/mod.rs`) | `binders` lists them (over-collecting variants, which capture analysis needs); the compiler's or-pattern names filter them through `pat_is_variant` |
| Bare name in a pattern: binder or variant (Rust's rule) | `bare_pattern_name` (`checker/pattern.rs`) | written to the resolution table the compiler's `pat_is_variant` (`compiler/pattern.rs`) reads |
| A duplicate binder in one pattern (or-alternatives scoped apart) | `first_duplicate_binder` (`checker/ty_util.rs`) | |

## Runtime

| Fact | Owner | Notes |
|---|---|---|
| Value tag layout | `vm/value.rs` | |
| Bytecode | `Op` (`base/op.rs`) | |
| `==` between values | `values_equal_guarded` (`vm/equality.rs`) | |
| May this op block here, and how | `mode` (`vm/block.rs`), `block_ctx` (`vm/block.rs`) | TICKET-181 |
| CPU width / runner permits | `RUNNERS` (`vm/width.rs`) | TICKET-205/230 |
| Quiescence verdict | `decide_since` (`vm/quiesce.rs`) | |
| Airlock crossing policy | `base/crossing.rs` | D4 |
| Cross-heap store check | `ensure_crossable` (`vm/worker_vm.rs`), `to_wire_crossable` (`vm/airlock_out.rs`) | a missed guard is UB |
| `WireValue` walks | DUPLICATE: `wire_summary` (`vm/core.rs`), `collect_core_gcrefs` (`vm/core.rs`), the `to_wire` budget in `vm/airlock_out.rs` | |
| Hook calls | `invoke_hook` (`vm/method_call.rs`) | DUPLICATE: direct `run_proto` (`vm/frames.rs`) calls in `vm/call.rs`, `vm/method_call.rs` and `vm/step.rs`; waits on Family D |
| Shared/RwShared box heap | `vm/boxctx.rs` | |
| Range value at run time | `as_range` (`vm/range.rs`) | |
| Runtime key of the `Range` struct `a..b` builds | `range_key` (`resolver/mod.rs`) | read by the checker's `range_ty` and `Program::range_key` |
| The std fns a program does not reach (TICKET-265) | `unreached_std_fns` → `ModuleGraph::skipped_fns` (`resolver/mod.rs`); the skip test is `SkippedFns::skips` | desugar's range gate, the checker's `skips_body` (`checker/graph_pass.rs`) and the compiler's top-level loop take their module's set from `ModuleGraph::fn_skips` and call `SkippedFns::skips`; `JSON_DECODE_PARSE` is the one fn the compiler calls by name |
| How a native fn runs | `Kind` (`native/mod.rs`) | the third element of each `MEMBERS` row |

## Tests

| Fact | Owner | Notes |
|---|---|---|
| Unit-test fixture temp directory | `TmpDir` (`test_support.rs`) | one counter, so two live fixtures never share a path; each crate's tests include the file; `main.rs` and `tests/` keep their own |
| C callback fixture library | `build_callback_so` (`test_support.rs`) | the native FFI tests and the VM FFI goldens (`vm/golden_tests/ffi.rs`) |
| VM production sources for a source-text rule | `vm_sources` (`vm/tests/mod.rs`) | |
| Generated-program grid runner | the fn `run_grid` in `tests/support/grid_cell.rs` | 15 grid files under `tests/` include that file: 14 call `run_grid`, `tests/open_binding_grid.rs` calls `run_cell`; a cell expects a stdout, a rejection fragment or a twin program run at test time |

## File map

Put new code in the file that owns its topic. Each file's first line names its topic; read that
line (or this map) before you open the file. `cargo test --test source_layout` caps a production
file at 2,500 lines and a test file at 3,500.

| Area | Files |
|---|---|
| Crates | One crate per layer, each building its files under `src/` by `#[path]` and re-exporting the layers below at their old paths: `chezzi-syntax` (`lexer`, `ast`, `parser`, `fmtspec`, `printable`, `slice`) ← `chezzi-native` (`native/`) ← `chezzi-base` (`base/`) ← `chezzi-front` (`resolver/`, `desugar/`, `checker/`, `compiler/`) and `chezzi-vm` (`vm/`) ← `chezzi` (`lib.rs`: `run.rs`, `editor/`, `test_runner.rs`, `manifest.rs`, the binaries). `chezzi-vm` never depends on `chezzi-front`; its tests do (dev-dependency) |
| Source checkout path (std dir, test fixtures) | `checkout_root` (`native/std_embed.rs`) | `resolver::std_root` and every test fixture path read it; never `env!("CARGO_MANIFEST_DIR")`, which names each crate's own directory |
| AST and span file id of an embedded std module | `shared_std_parse` (`resolver/mod.rs`): one lex and parse per process, ids `1..=STD_FILES.len()` by position | `Builder::parse` copies it into each graph; `std_module_ast` gives `std_type_owners` the same AST |
| Compile and run a program in a front-end test | `run_capture`, `run_file` (`compiler/run_for_tests.rs`) | through `vm::run_built`, the CLI's entry |
| Lexer | `lexer/mod.rs` (tokens, token extents such as `word_end_col`), `lexer/tests.rs` |
| Parser | `parser/mod.rs` (entry points, `Parser`, value sites), `parser/cursor.rs` (peek, expect, line and block ends), `parser/stmt.rs` (items, statements, imports), `parser/decl.rs` (fn, protocol, extern, native, struct, enum), `parser/control.rs` (if, for, while, match, else guards), `parser/pattern.rs` (patterns and binding targets), `parser/types.rs` (blocks, types), `parser/expr.rs` (Pratt expressions), `parser/interp.rs` (interpolation, closures), `parser/tests/` |
| AST | `ast/mod.rs` (node shapes and pure queries), `ast/consteval.rs` |
| Desugar, resolver | `desugar/mod.rs`, `resolver/mod.rs` |
| Checker: entry and tables | `checker/mod.rs` (`Checker`, reserved names, sig structs), `checker/entry.rs` (check entry points, call/compile table passes), `checker/tables.rs` (table types, witness keys), `checker/json_decode.rs` (the `json.decode` descriptor builder), `checker/graph_pass.rs`, `checker/setup.rs` (keys, aliases, names) |
| Checker: pass 1 | `checker/harvest.rs` (stdlib and native harvest), `checker/hoist.rs` (collect and hoist decls), `checker/module_imports.rs` (diagnostics, import binding), `checker/scope.rs` (scopes, lookup, heads), `checker/writes.rs` (captures, task writes) |
| Checker: signatures and types | `checker/sig.rs` (receivers, `fn_sig`), `checker/infer_ret.rs`, `checker/witness_sig.rs`, `checker/resolve_type.rs`, `checker/resolve_ctype.rs`, `checker/ty.rs`, `checker/ty_util.rs`, `checker/tyvar.rs`, `checker/builtin_type.rs`, `checker/builtin_methods.rs` |
| Checker: protocols | `checker/proto.rs` (intrinsic receivers, protocol hoisting), `checker/assignable.rs`, `checker/satisfies.rs`, `checker/hooks.rs`, `checker/op_sendable.rs`, `checker/eq.rs`, `checker/targs.rs`, `checker/generic_call.rs` |
| Checker: statements | `checker/stmt.rs`, `checker/assign.rs`, `checker/fn_body.rs`, `checker/fresh.rs`, `checker/flow.rs`, `checker/unused.rs`, `checker/fn_writes.rs`, `checker/self_writes.rs` |
| Checker: expressions | `checker/infer.rs` (core), `checker/infer_ops.rs` (literals, operators), `checker/infer_access.rs` (field, index, carriers), `checker/fn_value.rs`, `checker/closure.rs`, `checker/branch.rs` (wait, match, if) |
| Checker: calls | `checker/expr.rs` (call dispatch), `checker/ctor_call.rs`, `checker/builtin_call.rs`, `checker/fn_call.rs`, `checker/method_call.rs`, `checker/args.rs` |
| Checker: patterns, names | `checker/pattern.rs`, `checker/exhaust.rs`, `checker/iter_index.rs`, `checker/resolve.rs`, `checker/globals.rs`, `checker/suggest.rs` |
| Checker tests | `checker/tests/` (one file per topic, helpers in `checker/tests/mod.rs`), `checker/graph_tests.rs` |
| Compiler | `compiler/mod.rs` (entry, `Compiler`, keys, globals), `compiler/fn_body.rs`, `compiler/stmt.rs`, `compiler/assign.rs`, `compiler/loops.rs`, `compiler/pattern.rs`, `compiler/expr.rs`, `compiler/witness.rs`, `compiler/call.rs`, `compiler/capture.rs` (free variables), `compiler/fncomp.rs`, `compiler/peephole.rs`, `compiler/run_for_tests.rs` (compile and run, for the front end's tests), `compiler/tests/` |
| VM: core | `vm/mod.rs` (types, errors, traces, module declarations), `vm/value.rs`, `vm/heap.rs`, `vm/chzstr.rs`, `vm/entry.rs` (`run_built`, test run helpers), `vm/host.rs` |
| Base (read by the front end and the VM) | `base/op.rs` (bytecode, `Program`, `CompileError`), `base/crossing.rs`, `base/hooks.rs`, `base/json_desc.rs`, `base/module_id.rs`, `base/provider_name.rs`, `base/words.rs` |
| Run drivers (front end, then VM) | `run.rs` |
| VM: execution | `vm/exec.rs` (setup, invoke), `vm/frames.rs` (frames, halts), `vm/step.rs` (dispatch loop), `vm/call.rs` (calls, natives), `vm/method_call.rs` (methods, hooks), `vm/json_decode.rs` |
| VM: values | `vm/arith.rs`, `vm/compare.rs`, `vm/equality.rs`, `vm/hash.rs`, `vm/field_index.rs`, `vm/stmt.rs` (defer, unwind, structs, enums), `vm/builtins.rs`, `vm/globals.rs`, `vm/display.rs`, `vm/core_method.rs`, `vm/list_hof.rs`, `vm/range.rs` |
| VM: scheduler | `vm/sched_types.rs`, `vm/mnsched.rs`, `vm/mnsched_take.rs`, `vm/mnsched_park.rs`, `vm/mnsched_verdict.rs`, `vm/sched_core.rs`, `vm/sched.rs` (spawn targets, runner guards), `vm/nursery.rs`, `vm/demote.rs`, `vm/worker_loop.rs`, `vm/worker_vm.rs`, `vm/block.rs`, `vm/width.rs`, `vm/quiesce.rs`, `vm/pool.rs`, `vm/blocking_pool.rs`, `vm/runner_cache.rs`, `vm/sched_seed.rs`, `vm/timer.rs` |
| VM: concurrency values and I/O | `vm/netio.rs` (core accessors, guards), `vm/channel.rs`, `vm/blocking.rs`, `vm/socket.rs`, `vm/shared_methods.rs`, `vm/core.rs`, `vm/boxctx.rs`, `vm/poller.rs`, `vm/fileio.rs`, `vm/stream.rs` |
| VM: airlock | `vm/airlock_out.rs`, `vm/airlock_in.rs`, `vm/wire.rs`, `base/crossing.rs` |
| VM tests | `vm/tests/`, `vm/golden_tests/`, `vm/gc_tests.rs` |
| Natives | `native/mod.rs` (the seam), one `std.*` module per file such as `native/fs.rs`; `native/cffi.rs` (C FFI), `native/std_embed.rs` (the embedded `std/*.chz`) |
| Tools | `editor/mod.rs`, `test_runner.rs`, `test_support.rs`, `conformance.rs`, `difftest/`, `panicfuzz/`, `schedfuzz/` |
