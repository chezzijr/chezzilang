//! Compiler: calls: builtin calls, `compile_call`, type-applied fn values, plans and defaults, json
//! decode, closures, interpolation.
//! Split out of `src/compiler/mod.rs`. `super::*` is the parent module.

use super::*;
use crate::ast::{Chunk, Expr, ExprKind, Span, Stmt, StmtKind};
use crate::base::op::{CapEntry, Op, ProtoId};
use crate::checker::Resolution;
use std::collections::HashSet;

/// `print`'s `sep` and `end` defaults, read from the prelude's `native fn print(...args, sep, end)`:
/// the declaration owns them, and this is the one place the compiler reads them (the VM's
/// `Program.print_defaults`). `stmts` is the prelude module's statements.
pub(super) fn prelude_print_defaults(stmts: &[Stmt]) -> Result<[String; 2], CompileError> {
    let internal = || CompileError {
        message: "internal: the prelude declares no `native fn print(...args, sep: str = .., end: \
                  str = ..)` with literal defaults"
            .to_string(),
        span: Span::RUNTIME,
    };
    let mut decl = None;
    for s in stmts {
        if let StmtKind::Native(d) = &s.kind
            && Builtin::from_name(&d.name) == Some(Builtin::Print)
        {
            decl = Some(d);
        }
    }
    let decl = decl.ok_or_else(internal)?;
    let lit = |p: Option<&crate::ast::Param>| -> Option<String> {
        if let Some(Expr {
            kind: ExprKind::Str(s),
            ..
        }) = p?.default.as_ref()
        {
            Some(s.clone())
        } else {
            None
        }
    };
    let mut keyword_only = decl.params.iter().skip_while(|p| !p.is_variadic).skip(1);
    let sep = lit(keyword_only.next()).ok_or_else(internal)?;
    let end = lit(keyword_only.next()).ok_or_else(internal)?;
    Ok([sep, end])
}

impl Compiler {
    /// The opcode for a call whose head the checker resolved to [`Resolution::Builtin`].
    fn compile_builtin_call(
        &mut self,
        fc: &mut FnComp,
        b: Builtin,
        call_id: crate::ast::NodeId,
        args: &[Expr],
        named: &[(String, Expr)],
        span: Span,
    ) -> Result<(), CompileError> {
        match b {
            // `print(...)` lowers from its call plan `[Pack, sep, end]` (the prelude declaration's
            // slots): push each packed arg, then, when `sep` or `end` is written, push both (the
            // user expr or the declaration's default). With neither written the VM joins with the
            // declaration's defaults (`Program.print_defaults`).
            Builtin::Print => {
                let plan = self
                    .call_plans
                    .get(&(self.current_module_idx, call_id.0))
                    .cloned();
                let Some(plan) = plan else {
                    return Err(CompileError {
                        message: "internal: no call plan recorded for print".to_string(),
                        span,
                    });
                };
                let pick = |ci: usize| -> &Expr {
                    if ci < args.len() {
                        &args[ci]
                    } else {
                        &named[ci - args.len()].1
                    }
                };
                let [crate::checker::ArgFill::Pack(cis), sep, end] = &plan[..] else {
                    return Err(CompileError {
                        message: "internal: print call plan is not [pack, sep, end]".to_string(),
                        span,
                    });
                };
                for &ci in cis {
                    self.compile_expr(fc, pick(ci))?;
                }
                let tail = [sep, end];
                if tail
                    .iter()
                    .all(|f| !matches!(f, crate::checker::ArgFill::Arg(_)))
                {
                    fc.emit(Op::CallPrint(cis.len()), span);
                } else {
                    for fill in tail {
                        match fill {
                            crate::checker::ArgFill::Arg(ci) => self.compile_expr(fc, pick(*ci))?,
                            crate::checker::ArgFill::Provider(_)
                            | crate::checker::ArgFill::Inline { .. } => {
                                self.compile_default_fill(fc, fill, span)?
                            }
                            crate::checker::ArgFill::Pack(_) => {
                                return Err(CompileError {
                                    message: "internal: print tail slot is a pack".to_string(),
                                    span,
                                });
                            }
                        }
                    }
                    fc.emit(Op::CallPrintSep { argc: cis.len() }, span);
                }
            }
            Builtin::Ord
            | Builtin::Chr
            | Builtin::Panic
            | Builtin::Int
            | Builtin::Float
            | Builtin::Bool
            | Builtin::Str
            | Builtin::Bytes
            | Builtin::ByteArray
            | Builtin::List
            | Builtin::Map
            | Builtin::Set => {
                self.compile_args(fc, args)?;
                fc.emit(Op::CallBuiltin(b, args.len()), span);
            }
            // `Channel[T](cap)` pops the optional capacity; `Channel[T]()` is unbounded.
            Builtin::Channel => {
                self.compile_args(fc, args)?;
                fc.emit(Op::NewChannel(!args.is_empty()), span);
            }
            // The boxes copy their init value in (the checker validated one argument).
            Builtin::Shared => {
                self.compile_args(fc, args)?;
                fc.emit(Op::NewShared, span);
            }
            Builtin::RwShared => {
                self.compile_args(fc, args)?;
                fc.emit(Op::NewRwShared, span);
            }
            Builtin::Atomic => {
                self.compile_args(fc, args)?;
                fc.emit(Op::NewAtomic, span);
            }
            Builtin::AtomicInt => {
                self.compile_args(fc, args)?;
                fc.emit(Op::NewAtomicInt, span);
            }
            // `Executor()` is uncapped; `Executor(n)` caps the running jobs at `n`.
            Builtin::Executor => {
                self.compile_args(fc, args)?;
                fc.emit(
                    if args.is_empty() {
                        Op::NewExecutor
                    } else {
                        Op::NewExecutorN
                    },
                    span,
                );
            }
        }
        Ok(())
    }

    /// `fs[k](10)`: when the checker chose the bracket's index reading (`Resolution::IndexCall`),
    /// push the indexed element -- the head through its own recorded resolution, then the bracket,
    /// then `GetIndex` -- and answer `true`; the caller then calls that value. The one reader of
    /// `IndexCall`, for an eager call, a `defer` target and a `spawn` target alike.
    pub(super) fn compile_index_call_head(
        &mut self,
        fc: &mut FnComp,
        call_id: crate::ast::NodeId,
        callee: &Expr,
        bracket: Option<&Expr>,
        span: Span,
    ) -> Result<bool, CompileError> {
        if !matches!(
            self.resolutions.get(&(self.current_module_idx, call_id.0)),
            Some(Resolution::IndexCall)
        ) {
            return Ok(false);
        }
        let Some(ix) = bracket else {
            return Err(CompileError {
                message: "internal: an index-call has no bracket expression".to_string(),
                span,
            });
        };
        self.compile_expr(fc, callee)?;
        self.compile_expr(fc, ix)?;
        fc.emit(Op::GetIndex, span);
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)] // the call's parts + its bracket reading + span + id
    pub(super) fn compile_call(
        &mut self,
        fc: &mut FnComp,
        callee: &Expr,
        args: &[Expr],
        named: &[(String, Expr)],
        bracket: Option<&Expr>,
        span: Span,
        call_id: crate::ast::NodeId,
    ) -> Result<(), CompileError> {
        if self.compile_index_call_head(fc, call_id, callee, bracket, span)? {
            let argc = self.compile_call_args(fc, call_id, callee, args, named, span)?;
            fc.emit(Op::Call(argc), span);
            return Ok(());
        }
        // A default-argument provider call whose declaring module this one cannot name — no synthetic
        // import was (or could be) emitted for it, so there is no global slot to read. Lower it to a
        // direct, call-time reference to the definer's proto. See [`Op::MakeFuncIn`] and
        // `Checker::bind_call`. Same-module and in-closure providers have a global slot
        // and fall through to the ordinary path below.
        if let ExprKind::Ident(n) = &callee.kind
            && matches!(self.resolution(callee)?, Resolution::Provider)
        {
            let id = self.provider_id(n);
            fc.emit(Op::MakeFuncIn(id), span);
            fc.emit(Op::Call(0), span);
            return Ok(());
        }
        // TICKET-187: `json.decode[T](s)` — the checker recorded `Resolution::Decode` on the callee
        // only for `decode` on the `std.json` module. Reuse the module's own `parse`
        // (`obj.parse(arg)` → Result[Json]), then coerce into the target with the descriptor.
        if let ExprKind::Field { obj, .. } = &callee.kind
            && let Some(Resolution::Decode(desc)) = self
                .resolutions
                .get(&(self.current_module_idx, callee.id.0))
            && let [arg] = args
        {
            let desc = desc.clone();
            self.compile_expr(fc, obj)?;
            self.compile_expr(fc, arg)?;
            let ic = self.next_method_ic();
            fc.emit(
                Op::CallMethod {
                    name: crate::resolver::JSON_DECODE_PARSE.to_string(),
                    argc: 1,
                    ic,
                },
                span,
            );
            self.emit_json_decode(fc, &desc, span)?;
            return Ok(());
        }
        // Method / module-member call: `obj.name(args)`.
        if let ExprKind::Field {
            obj,
            name,
            name_span,
        } = &callee.kind
            && !crate::ast::is_tuple_index(name)
        {
            // TICKET-180: the checker's record on the callee `Field` is the only answer to what
            // `obj.name` names; a type head is never compiled, a module or value receiver is.
            match self.resolution(callee)?.clone() {
                Resolution::StructCtor(key) => {
                    let argc = self.compile_call_args(fc, call_id, callee, args, named, span)?;
                    self.emit_new_struct(fc, key, argc, span)?;
                    return Ok(());
                }
                Resolution::Variant { enum_key, variant }
                | Resolution::VariantFn {
                    enum_key, variant, ..
                } => return self.compile_variant_call(fc, &enum_key, &variant, args, span),
                Resolution::MethodFn { type_key, method } => {
                    self.emit_call_static(
                        fc,
                        type_key,
                        &method,
                        (call_id, callee, args, named),
                        *name_span,
                        span,
                    )?;
                    return Ok(());
                }
                // `T.get(v)`: the path value applied.
                Resolution::ParamMethodFn { .. } => {
                    self.compile_expr(fc, callee)?;
                    let argc = self.compile_call_args(fc, call_id, callee, args, named, span)?;
                    fc.emit(Op::Call(argc), span);
                    return Ok(());
                }
                Resolution::WitnessStatic(t) => {
                    let w = fc.witness_ref(&t).ok_or_else(|| CompileError {
                        message: format!("internal: no witness for type parameter '{t}' here"),
                        span,
                    })?;
                    let argc = self.compile_call_args(fc, call_id, callee, args, named, span)?;
                    fc.emit_witness(w, span);
                    fc.emit(
                        Op::CallStaticDyn {
                            method: name.clone(),
                            argc,
                        },
                        span,
                    );
                    return Ok(());
                }
                Resolution::Builtin(b) => {
                    return self.compile_builtin_call(fc, b, call_id, args, named, span);
                }
                Resolution::Member | Resolution::ModuleMember { .. } | Resolution::Fn { .. } => {}
                other @ Resolution::Local
                | other @ Resolution::Global { .. }
                | other @ Resolution::Provider
                | other @ Resolution::IndexCall
                | other @ Resolution::Module(_)
                | other @ Resolution::PatBinding
                | other @ Resolution::PatStruct(_)
                | other @ Resolution::Decode(_) => {
                    return Err(CompileError {
                        message: format!("internal: a call callee resolved to {other:?}"),
                        span,
                    });
                }
            }
            if let Some(srcs) = self.witness_srcs(callee, name, span)? {
                self.compile_expr(fc, obj)?;
                let argc = self.compile_call_args(fc, call_id, callee, args, named, span)?;
                let w = self.emit_witness_args(fc, &srcs, name, span)?;
                let ic = self.next_method_ic();
                fc.emit(
                    Op::CallMethod {
                        name: name.clone(),
                        argc: argc + w,
                        ic,
                    },
                    span,
                );
                return Ok(());
            }
            // `xs.sum()` over a `List[float]`: push a `0.0` SEED as `sum`'s one hidden argument. The
            // seed IS the answer for an EMPTY list, which is why it must come from here: the backend
            // is type-blind and an empty list carries no element to read a kind off. The checker
            // decided it; a miss falls
            // through to the plain numeric lowering below.
            if let Some(seed) = self.sum_seed(name, args, *name_span) {
                self.compile_expr(fc, obj)?;
                self.emit_sum_seed(fc, &seed, span);
                let ic = self.next_method_ic();
                fc.emit(
                    Op::CallMethod {
                        name: name.clone(),
                        argc: 1,
                        ic,
                    },
                    span,
                );
                return Ok(());
            }
            // W7-53 I1′ — `.eq(x)` through a generic bound (`fn f[T: Eq](a: T, b: T): a.eq(b)`) is
            // the PROTOCOL's equality, not whatever ordinary method the receiver happens to spell
            // `eq`. Lower it to the very opcode `==` uses, so the two spellings are one dispatch by
            // construction: `Op::Eq` runs `values_equal_guarded`, which already dispatches a real
            // user `eq` HOOK and falls back to structural equality otherwise. Rust resolves the same
            // call to `<T as PartialEq>::eq`; a CONCRETE receiver keeps the inherent-wins rule and
            // is recorded `false` by the checker, so it never lands here.
            let eq_key = crate::checker::carrier_key(
                self.current_module_idx,
                self.kw_frag_ctx,
                self.kw_frag_ord,
                *name_span,
            );
            if self.hook_calls.get(&eq_key) == Some(&crate::base::hooks::Hook::Eq)
                && args.len() == 1
            {
                self.compile_expr(fc, obj)?;
                self.compile_expr(fc, &args[0])?;
                fc.emit(Op::Eq, span);
                return Ok(());
            }
            // M24 Task 5 — an INSTANCE method that declares its own witnessed `[T]`
            // (`h.make(c)`): the hidden witness arguments ride LAST, after the declared args, so
            // `CallMethod`'s widened `argc` matches the proto the method was compiled with. The
            // checker's record is keyed on the method-name token, which is unique per link of a
            // postfix chain (`h.make(a).make(b)` shares one call span, not one name token).
            self.compile_expr(fc, obj)?;
            let argc = self.compile_call_args(fc, call_id, callee, args, named, span)?;
            let w = match self.member_witness_srcs(*name_span).cloned() {
                Some(srcs) => self.emit_witness_args(fc, &srcs, name, span)?,
                None => 0,
            };
            let ic = self.next_method_ic();
            // TICKET-250 — a call the checker typed through a reserved protocol's requirement
            // reads the hook table at run time; it never finds a method by name alone.
            let hook_key = crate::checker::carrier_key(
                self.current_module_idx,
                self.kw_frag_ctx,
                self.kw_frag_ord,
                *name_span,
            );
            let op = match self.hook_calls.get(&hook_key) {
                Some(&hook) => Op::CallHook {
                    hook,
                    name: name.clone(),
                    argc: argc + w,
                    ic,
                },
                None => Op::CallMethod {
                    name: name.clone(),
                    argc: argc + w,
                    ic,
                },
            };
            fc.emit(op, span);
            return Ok(());
        }
        if let ExprKind::Ident(_) = &callee.kind {
            match self.resolution(callee)?.clone() {
                Resolution::Builtin(b) => {
                    return self.compile_builtin_call(fc, b, call_id, args, named, span);
                }
                Resolution::StructCtor(key) => {
                    let argc = self.compile_call_args(fc, call_id, callee, args, named, span)?;
                    self.emit_new_struct(fc, key, argc, span)?;
                    return Ok(());
                }
                // A bare imported variant constructor (`Som(5)`, the prelude's `Some(5)`).
                Resolution::Variant { enum_key, variant }
                | Resolution::VariantFn {
                    enum_key, variant, ..
                } => return self.compile_variant_call(fc, &enum_key, &variant, args, span),
                // `std.json`'s own `_to_json` lowers to its opcode. A user module cannot declare a
                // `native fn`, so no user fn is ever this pair.
                Resolution::Fn { module, name }
                    if name == "_to_json" && self.json_to_value_home == Some(module) =>
                {
                    self.compile_args(fc, args)?;
                    fc.emit(Op::JsonToValue, span);
                    return Ok(());
                }
                Resolution::Local
                | Resolution::Global { .. }
                | Resolution::Fn { .. }
                | Resolution::Provider
                | Resolution::ParamMethodFn { .. }
                | Resolution::IndexCall
                | Resolution::MethodFn { .. }
                | Resolution::WitnessStatic(_)
                | Resolution::ModuleMember { .. }
                | Resolution::Module(_)
                | Resolution::Member
                | Resolution::PatBinding
                | Resolution::PatStruct(_)
                | Resolution::Decode(_) => {}
            }
        }
        // M24 — a call to a generic fn that takes hidden trailing witness params, by its bare name
        // (declared here, or `from`-imported): push the declared args, then one argument per witness
        // slot, and call with the widened `argc`. The compiler CONSUMES the checker's table; a
        // mismatch either way is a hard error, never a short `argc` (see `witness_srcs`).
        if let ExprKind::Ident(fname) = &callee.kind
            && let Some(srcs) = self.witness_srcs(callee, fname, span)?
        {
            self.compile_expr(fc, callee)?;
            let argc = self.compile_call_args(fc, call_id, callee, args, named, span)?;
            let w = self.emit_witness_args(fc, &srcs, fname, span)?;
            fc.emit(Op::Call(argc + w), span);
            return Ok(());
        }
        // General callable value: the checker's slot plan, when it bound this call, else the
        // arguments as written.
        self.compile_expr(fc, callee)?;
        let argc = self.compile_call_args(fc, call_id, callee, args, named, span)?;
        fc.emit(Op::Call(argc), span);
        Ok(())
    }

    /// THE one lowering of a type-applied fn value `head[T…]`, either carrier
    /// (`ast::type_application`). Generic-fn-as-value turbofish erase: the checker validated the
    /// type args, and the runtime is generic-ERASED — the value IS the underlying function, method
    /// or variant constructor — so drop the type args and lower only the head. The head is an
    /// `Ident` or a non-tuple `Field` (`m.f[int]`, `Bx[int].put[str]`), and the checker's `Fn`,
    /// `MethodFn` or `VariantFn` on it is the one fact this erase reads (TICKET-197/204). `false`
    /// when `e` is no such value (a real index), which then compiles as an index.
    pub(super) fn compile_type_applied_fn_value(
        &mut self,
        fc: &mut FnComp,
        e: &Expr,
    ) -> Result<bool, CompileError> {
        let Some(app) = crate::ast::type_application(e) else {
            return Ok(false);
        };
        if !app.head.kind.is_named_head() {
            return Ok(false);
        }
        let res = self.resolution(app.head)?.clone();
        // std.json's decode read as a value with its `T` written (TICKET-214).
        if let Resolution::Decode(desc) = &res
            && let ExprKind::Field { obj, .. } = &app.head.kind
        {
            self.compile_decode_fn_value(fc, obj, desc, e.span)?;
            return Ok(true);
        }
        if !matches!(
            res,
            Resolution::Fn { .. } | Resolution::MethodFn { .. } | Resolution::VariantFn { .. }
        ) {
            return Ok(false);
        }
        self.compile_expr(fc, app.head)?;
        Ok(true)
    }

    /// THE memo of compiler-synthesized fn values (TICKET-226): the proto of `key`, built by
    /// `build` on its first read. One proto per key is what makes two reads one `Vm::fn_value`.
    pub(super) fn synth_fn_proto(
        &mut self,
        key: SynthFn,
        build: impl FnOnce(&mut Self) -> Result<ProtoId, CompileError>,
    ) -> Result<ProtoId, CompileError> {
        if let Some(&p) = self.synth_fns.get(&key) {
            return Ok(p);
        }
        let p = build(self)?;
        self.synth_fns.insert(key, p);
        Ok(p)
    }

    /// The proto of `T.method` as a value: a fn of `arity` params that calls `method` on its
    /// first, so it dispatches on the receiver's runtime type and needs no witness. Memoised by
    /// [`Compiler::synth_fn_proto`].
    pub(super) fn build_param_method_fn(&mut self, method: &str, arity: usize) -> ProtoId {
        let mut vf = FnComp::new(method.to_string(), arity, false);
        for i in 0..arity {
            let slot = vf.add_local(format!("$v{i}"));
            vf.emit_get_local_raw(slot, Span::RUNTIME);
        }
        let ic = self.next_method_ic();
        vf.emit(
            Op::CallMethod {
                name: method.to_string(),
                argc: arity.saturating_sub(1),
                ic,
            },
            Span::RUNTIME,
        );
        vf.emit(Op::Return, Span::RUNTIME);
        self.finish(vf)
    }

    /// TICKET-180 — what the name head `e` denotes: the checker's recorded [`Resolution`], the
    /// ONLY source. A miss is an internal error: a checker arm returned without recording, so fix
    /// that arm and never add a fallback here. A compiler-synthesized node
    /// ([`crate::ast::NodeId::SYNTH`]) is answered by its shape: the only SYNTH `Ident`s are the
    /// `$`-named locals the compiler itself declares, and the lexer never produces a `$`; a SYNTH
    /// `Field` is `$comp.push`/`$comp.add`, a member of that local.
    ///
    /// The position rule: the compiler calls this only on a node it lowers as a name.
    /// (1) A call callee `Ident`. (2) A call callee `Field`, and the `Field` inside a callee
    /// `Index { obj: Field }` (`Type[T].m[U](..)`, `Type.m[U](..)`, `m.f[int](..)`). (3) An `Ident`
    /// it compiles as a value. (4) A value `Field`, including `MethodFn` (a type path's method
    /// read as a value; a payload variant read as a value is a type error, U10). (5) The `obj` of a `Field` is compiled,
    /// and so falls under (3), only when that `Field`'s entry is `Member`, `ModuleMember` or `Fn`.
    /// (6) The `Ident` or `Field` head of a type-applied fn value `Index`
    /// (`m.f[int]`, `pair[str, int]`, `Bx[int].put[str]`), recorded as `Fn` or `MethodFn`. It never asks
    /// about a type head (`T` in `T.m()`, `E` in `E.V`, `Box` in `Box[int].of()`) or a type
    /// argument, because it never lowers them. No `ast` shape predicate can state this rule:
    /// `xs[i].k` and `Box[int].k` share one shape.
    pub(super) fn resolution(&self, e: &Expr) -> Result<&Resolution, CompileError> {
        static SYNTH_LOCAL: Resolution = Resolution::Local;
        static SYNTH_MEMBER: Resolution = Resolution::Member;
        if e.id.0 == crate::ast::NodeId::SYNTH.0 {
            return match &e.kind {
                ExprKind::Ident(_) => Ok(&SYNTH_LOCAL),
                // `$comp.push` / `$comp.add` (`method_call_stmt`).
                ExprKind::Field { .. } => Ok(&SYNTH_MEMBER),
                ExprKind::Int(_)
                | ExprKind::Float(_)
                | ExprKind::Str(_)
                | ExprKind::Interp(_)
                | ExprKind::Bytes(_)
                | ExprKind::RawStr(_)
                | ExprKind::Bool(_)
                | ExprKind::NoneLit
                | ExprKind::Pass
                | ExprKind::List(..)
                | ExprKind::Tuple(_)
                | ExprKind::Map(_)
                | ExprKind::Set(_)
                | ExprKind::Comprehension { .. }
                | ExprKind::Unary { .. }
                | ExprKind::Binary { .. }
                | ExprKind::Compare { .. }
                | ExprKind::Range { .. }
                | ExprKind::Call { .. }
                | ExprKind::Index { .. }
                | ExprKind::Try(_)
                | ExprKind::OptChain { .. }
                | ExprKind::NullCoalesce { .. }
                | ExprKind::Closure { .. }
                | ExprKind::Match { .. }
                | ExprKind::IfElse { .. }
                | ExprKind::Recover(..)
                | ExprKind::ElseGuard { .. } => Err(CompileError {
                    message: "internal: a synthesized node has no name resolution".to_string(),
                    span: e.span,
                }),
            };
        }
        self.resolutions
            .get(&(self.current_module_idx, e.id.0))
            .ok_or_else(|| CompileError {
                message: "internal: no name resolution recorded for this expression -- the \
                          type-checker and the backend disagree"
                    .to_string(),
                span: e.span,
            })
    }

    /// Push the arguments of the call `call_id` in the checker's slot plan
    /// ([`crate::checker::CallPlanTable`]), and return, per pushed value, the user expression it came
    /// from (`None` for a default fill or a variadic pack). With no plan: `Ok(None)` when `named` is
    /// empty (the call is positional as written), and an internal error otherwise -- a positional
    /// fall-through would silently drop every named argument.
    fn compile_plan_args<'e>(
        &mut self,
        fc: &mut FnComp,
        call_id: crate::ast::NodeId,
        callee: &Expr,
        args: &'e [Expr],
        named: &'e [(String, Expr)],
        span: Span,
    ) -> Result<Option<Vec<Option<&'e Expr>>>, CompileError> {
        let Some(plan) = self
            .call_plans
            .get(&(self.current_module_idx, call_id.0))
            .cloned()
        else {
            if named.is_empty() {
                return Ok(None);
            }
            return Err(CompileError {
                message: "internal: no call plan recorded for this call -- the type-checker and \
                          the backend disagree"
                    .to_string(),
                span,
            });
        };
        let pick = |ci: usize| -> &'e Expr {
            if ci < args.len() {
                &args[ci]
            } else {
                &named[ci - args.len()].1
            }
        };
        let mut srcs = Vec::with_capacity(plan.len());
        for fill in &plan {
            match fill {
                crate::checker::ArgFill::Arg(ci) => {
                    let e = pick(*ci);
                    self.compile_expr(fc, e)?;
                    srcs.push(Some(e));
                }
                crate::checker::ArgFill::Pack(cis) => {
                    let pack = Expr::synthetic(
                        ExprKind::List(
                            cis.iter().map(|&ci| pick(ci).clone()).collect(),
                            Some(crate::checker::witness_key_span(callee, span)),
                        ),
                        span,
                    );
                    self.compile_expr(fc, &pack)?;
                    srcs.push(None);
                }
                crate::checker::ArgFill::Provider(_) | crate::checker::ArgFill::Inline { .. } => {
                    self.compile_default_fill(fc, fill, span)?;
                    srcs.push(None);
                }
            }
        }
        Ok(Some(srcs))
    }

    /// Push one omitted slot's default value: the ONE lowering of a default fill, shared by a
    /// call's plan and a `json.decode` default thunk (TICKET-198).
    fn compile_default_fill(
        &mut self,
        fc: &mut FnComp,
        fill: &crate::checker::ArgFill,
        span: Span,
    ) -> Result<(), CompileError> {
        match fill {
            crate::checker::ArgFill::Provider(name) => {
                let id = self.provider_id(name);
                fc.emit(Op::MakeFuncIn(id), span);
                fc.emit(Op::Call(0), span);
                Ok(())
            }
            // The declaration's own node, under its declaring module's records, so a literal
            // default resolves in the definer, never in the caller.
            crate::checker::ArgFill::Inline { module, expr } => {
                let saved = std::mem::replace(&mut self.current_module_idx, *module);
                let r = self.compile_expr(fc, expr);
                self.current_module_idx = saved;
                r
            }
            crate::checker::ArgFill::Arg(_) | crate::checker::ArgFill::Pack(_) => {
                Err(CompileError {
                    message: "internal: not a default fill".into(),
                    span,
                })
            }
        }
    }

    /// A zero-arg proto that returns one field default, for `json.decode` to call when its key is
    /// absent; homed in the declaring module for an inline fill.
    fn compile_default_thunk(
        &mut self,
        fill: &crate::checker::ArgFill,
        span: Span,
    ) -> Result<crate::base::json_desc::DefaultThunk, CompileError> {
        let mut fc = FnComp::new("json.decode".to_string(), 0, false);
        self.compile_default_fill(&mut fc, fill, span)?;
        fc.emit(Op::Return, span);
        let proto = self.finish(fc);
        let module = match fill {
            crate::checker::ArgFill::Inline { module, .. } => *module,
            crate::checker::ArgFill::Arg(_)
            | crate::checker::ArgFill::Pack(_)
            | crate::checker::ArgFill::Provider(_) => self.current_module_idx,
        };
        Ok(crate::base::json_desc::DefaultThunk { proto, module })
    }

    /// The descriptor's coercion of the parsed `Result[Json]` on the stack: the one coercion step
    /// of std.json's decode, shared by the call and the value proto.
    fn emit_json_decode(
        &mut self,
        fc: &mut FnComp,
        desc: &crate::base::json_desc::TypeDescriptor<crate::checker::ArgFill>,
        span: Span,
    ) -> Result<(), CompileError> {
        let desc = desc.try_map_defaults(&mut |f| self.compile_default_thunk(f, span))?;
        fc.emit(Op::JsonDecode(desc), span);
        Ok(())
    }

    /// `json.decode[T]` read as a value (TICKET-214): one proto per target shape
    /// ([`SynthFn::Decode`]), homed in std.json by `Op::MakeFuncIn`, so its home is fixed per heap
    /// and two reads in any modules are one `Vm::fn_value` (TICKET-226).
    pub(super) fn compile_decode_fn_value(
        &mut self,
        fc: &mut FnComp,
        obj: &Expr,
        desc: &crate::base::json_desc::TypeDescriptor<crate::checker::ArgFill>,
        span: Span,
    ) -> Result<(), CompileError> {
        let Resolution::Module(json_idx) = *self.resolution(obj)? else {
            return Err(CompileError {
                message: "internal: json.decode's module is not a whole-module import".into(),
                span,
            });
        };
        let pid = self.synth_fn_proto(SynthFn::Decode(desc.shape()), |c| {
            c.build_decode_fn(json_idx, desc)
        })?;
        let id = self.provider_id(&decode_fn_name(pid));
        fc.emit(Op::MakeFuncIn(id), span);
        Ok(())
    }

    /// The body of a `json.decode[T]` value: std.json's own `parse` slot (it reads no reader
    /// global), then the coercion. Registered in the provider table under [`decode_fn_name`].
    fn build_decode_fn(
        &mut self,
        json_idx: usize,
        desc: &crate::base::json_desc::TypeDescriptor<crate::checker::ArgFill>,
    ) -> Result<ProtoId, CompileError> {
        let mut vf = FnComp::new("json.decode".to_string(), 1, false);
        let s = vf.add_local("$s".to_string());
        let Some(parse) = self.program.modules[json_idx]
            .global_slots
            .iter()
            .position(|n| n == crate::resolver::JSON_DECODE_PARSE)
        else {
            return Err(CompileError {
                message: "internal: std.json has no parse slot".into(),
                span: Span::RUNTIME,
            });
        };
        vf.emit(Op::GetGlobalSlot(parse as u32), Span::RUNTIME);
        vf.emit_get_local_raw(s, Span::RUNTIME);
        vf.emit(Op::Call(1), Span::RUNTIME);
        self.emit_json_decode(&mut vf, desc, Span::RUNTIME)?;
        vf.emit(Op::Return, Span::RUNTIME);
        let pid = self.finish(vf);
        self.provider_defs
            .insert(decode_fn_name(pid), (pid, json_idx));
        Ok(pid)
    }

    /// Push a call's arguments -- by its plan when the checker bound it, else as written -- and
    /// return how many values were pushed.
    pub(super) fn compile_call_args(
        &mut self,
        fc: &mut FnComp,
        call_id: crate::ast::NodeId,
        callee: &Expr,
        args: &[Expr],
        named: &[(String, Expr)],
        span: Span,
    ) -> Result<usize, CompileError> {
        match self.compile_plan_args(fc, call_id, callee, args, named, span)? {
            Some(srcs) => Ok(srcs.len()),
            None => {
                self.compile_args(fc, args)?;
                Ok(args.len())
            }
        }
    }

    pub(super) fn compile_closure(
        &mut self,
        fc: &mut FnComp,
        params: &[crate::ast::Param],
        body: &Expr,
        span: Span,
    ) -> Result<(), CompileError> {
        // Capture ONLY the names this body references from the enclosing frame (its free-variable
        // set), not every visible local. `free_names_expr` is a trusted over-approximation (it also
        // drives cell-boxing).
        let params_set: HashSet<String> = params.iter().map(|p| p.name.clone()).collect();
        let mut free = FreeNames::default();
        free_names_expr(body, &params_set, &mut free);
        let snap = fc.snapshot_entries();
        // M24 Task 4: a closure captures only FREE VARIABLES, and the enclosing frame's `$w:T` is
        // never spelled in the source — so it is appended explicitly (M24-2: when this body can
        // reach it), and travels BY VALUE (the point of case 1: a closure that outlives its defining
        // frame still constructs the right type).
        let entries: Vec<CapEntry> = self.with_witness_captures(
            &snap,
            snap.iter()
                .filter(|e| free.names.contains(&e.name))
                .cloned()
                .collect(),
            Some(&free),
        );
        let captured_names: Vec<String> = entries.iter().map(|e| e.name.clone()).collect();

        let mut child = FnComp::new("<closure>".to_string(), params.len(), false);
        // Uniform by-reference capture (Task A): this closure's own boxed-name set (unwired).
        child.boxed_names = captured_names_of_closure(body, params);
        child.captured_names = captured_names;
        for p in params {
            child.add_local(p.name.clone());
        }
        // Uniform by-reference capture: box any param captured by a nested closure.
        self.emit_box_param_prologue(&mut child, params);
        // M-C (TICKET-040): a closure body is its own function body — a nested bare `spawn` (incl.
        // one reached through a `recover:`) binds to the closure's *own* implicit nursery, joined
        // when the closure body returns. Mirrors the `spawn:`-block gate above.
        let implicit = expr_has_bare_spawn(body);
        if implicit {
            child.has_implicit_nursery = true;
            child.emit(Op::EnterNursery, span);
            child.nursery_scopes += 1;
        }
        self.compile_expr(&mut child, body)?;
        if implicit {
            child.nursery_scopes -= 1;
        }
        child.emit(Op::Return, span);
        let pid = self.finish(child);

        fc.emit(Op::MakeClosure(pid, entries), span);
        Ok(())
    }

    /// Emit an interpolated literal ([`ExprKind::Interp`]): literal text and `{expr}` fragments,
    /// concatenated with `BuildStr`. A literal-only chunk list is a single `ConstStr`.
    pub(super) fn compile_interp(
        &mut self,
        fc: &mut FnComp,
        chunks: &[Chunk],
        span: Span,
    ) -> Result<(), CompileError> {
        if let [Chunk::Lit(s)] = chunks {
            fc.emit(Op::ConstStr(s.clone()), span);
            return Ok(());
        }
        if chunks.is_empty() {
            fc.emit(Op::ConstStr(String::new()), span);
            return Ok(());
        }
        let n = chunks.len();
        // A value+keyword call inside a `{…}` fragment is keyed by (string span, fragment ordinal) so
        // the lookup matches the checker's record (each fragment re-lexes from a fresh source, so span
        // alone can't tell two fragments apart). Save/restore for nested interpolations.
        let saved_ctx = self.kw_frag_ctx;
        let saved_ord = self.kw_frag_ord;
        let mut ord = 0usize;
        for chunk in chunks {
            match chunk {
                Chunk::Lit(s) => fc.emit(Op::ConstStr(s.clone()), span),
                Chunk::Expr(e, spec, fields) => {
                    self.kw_frag_ctx = span;
                    self.kw_frag_ord = ord;
                    // The fragment root is compiled with its OWN span, so a RUNTIME fault inside a
                    // fragment keeps the span it had before M24. (The checker re-anchors its
                    // fragment-root DIAGNOSTIC at the string literal — a diagnostic anchor, not a
                    // key: every per-call table keys on a sub-node the rewrite cannot touch, the
                    // CALLEE TOKEN for `WitnessTable::calls`, the call NodeId for
                    // `CallPlanTable`. `49bd9f80` conflated the two; keeping them separate is what
                    // lets both halves be right.)
                    self.compile_expr(fc, e)?;
                    // A nested width/precision field evaluates AFTER the value, width first (CPython
                    // order, and `check_interp_chunks`'s); `ToStrFmt` pops them before the value.
                    for f in fields {
                        self.compile_expr(fc, f)?;
                    }
                    match spec {
                        None => fc.emit(Op::ToStr, span),
                        Some(fs) => fc.emit(Op::ToStrFmt(Box::new(fs.clone())), span),
                    }
                    ord += 1;
                }
            }
        }
        self.kw_frag_ctx = saved_ctx;
        self.kw_frag_ord = saved_ord;
        fc.emit(Op::BuildStr(n), span);
        Ok(())
    }
}
