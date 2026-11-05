//! Checker: std-module licensing gates, qualified builtins, static-protocol witness params.
//! Split out of `src/checker/sig.rs`. `super::*` is the parent module.

use super::sig::ffi_width_ty;
use super::sig::turbofish_charges;
use super::sig::ty_mentions;
use super::*;

impl Checker {
    /// Resolve an AST `Type` annotation into a checker `Ty`, reporting unknown type names.
    /// The "unknown type T" message, with a module-scoped import hint when `T` is declared by some
    /// (un-imported) module: a type is private to its declaring module and must be imported. Picks
    /// the first declaring module in graph (deps-first) order.
    pub(super) fn unknown_type_msg(&self, n: &str) -> String {
        format!("unknown type '{n}'{}", self.import_hint(n))
    }

    /// The import hint for a bare type or protocol name some module declares (`; import it from
    /// std.regex (`import Match from std.regex`)`), or nothing.
    pub(super) fn import_hint(&self, n: &str) -> String {
        let user = self.types_by_name.get(n).and_then(|mods| mods.first());
        // A protocol first: `std_protocol_owner` reads no more than the protocol table, which the
        // checker has loaded already, where `std_type_owners` parses every std file. A global
        // protocol needs no import, so it gets no hint.
        let std_module = user
            .map(String::as_str)
            .or_else(|| super::graph_pass::std_protocol_owner(n).filter(|_| !is_global_protocol(n)))
            .or_else(|| {
                super::graph_pass::std_type_owners()
                    .get(n)
                    .and_then(|m| m.first())
                    .map(String::as_str)
            });
        match std_module {
            Some(m) => format!("; import it from {m} (`import {n} from {m}`)"),
            None => String::new(),
        }
    }

    /// The import hint for a std protocol `name` the module does not see, else nothing. A
    /// diagnostic that suggests a bound `[T: name]` appends it.
    pub(super) fn bound_hint(&self, name: &str) -> String {
        if self.user_protocol_key(name).is_none() {
            self.import_hint(name)
        } else {
            String::new()
        }
    }

    /// Binds the module-owned builtin types `module` owns under their own names: a module sees
    /// its own declarations (`std/ffi.chz`'s sigs name `ptr`).
    pub(super) fn bind_own_builtins(&mut self, module: &str) {
        for n in super::builtin_type::module_builtins(module) {
            self.builtin_binds.insert(n.clone(), n);
        }
    }

    /// The module-owned builtin type a bare name denotes in this module (the member's own name),
    /// or `None`: an import or the owning module bound it, under that name or an alias.
    pub(super) fn bound_builtin(&self, n: &str) -> Option<&str> {
        self.builtin_binds.get(n).map(String::as_str)
    }

    /// The builtin a bare callee `n` names, by the builtin's own name: a builtin type with an opcode
    /// constructor this module sees (`Channel`, or a bound `Shared`: `S` after
    /// `import Shared as S from std.concurrency`), else a prelude function or constructor. An
    /// import binds before a builtin of its name, as a declaration does.
    pub(super) fn builtin_callee(&self, n: &str) -> Option<Builtin> {
        if let Some(b) = self.builtin_type_named(n)
            && b.has_opcode_ctor()
        {
            return Builtin::from_name(b.name());
        }
        prelude_fn(n).filter(|_| self.bound_builtin(n).is_none())
    }

    /// Whether `module.name` (through a whole-module import) names a builtin type `module` owns.
    pub(super) fn module_owns_builtin(&self, module: &str, name: &str) -> bool {
        !self.head_is_value(module)
            && self
                .imported_modules
                .get(module)
                .and_then(|mid| self.module_sigs.get(mid))
                .is_some_and(|sig| sig.builtins.contains(name))
    }

    /// The builtin type a bare type name denotes here: a module-owned one this module bound (an
    /// import binds before a builtin of its name), else a global builtin.
    pub(super) fn builtin_type_named(&self, n: &str) -> Option<BuiltinType> {
        match self.bound_builtin(n) {
            Some(own) => BuiltinType::from_name(own),
            // A std module sees a builtin type its own protocol declares (`Iterator`) as it sees
            // every std protocol, with no import. Any other module-owned type still needs one.
            None => BuiltinType::global(n).or_else(|| {
                BuiltinType::from_name(n)
                    .filter(|b| self.current_module_is_stdlib && is_std_protocol(b.name()))
            }),
        }
    }

    /// U10: `owned_str` resolves only in an extern signature (a return-only marshalling type);
    /// every other builtin type resolves as it is.
    pub(super) fn builtin_gate(&mut self, b: BuiltinType, t: Ty, span: Span) -> Ty {
        if b != BuiltinType::OwnedStr || self.in_extern_sig {
            return t;
        }
        self.error(
            span,
            "'owned_str' is a return-only extern marshalling type and cannot be used as a \
             general type annotation"
                .to_string(),
        );
        Ty::Unknown
    }

    /// Map an opaque/native builtin TYPE name (one that lives ONLY in its owning std module's
    /// `sig.types` — reserved, so no user module can export it) to its builtin `Ty`, given already-
    /// resolved type args. This is the additive "make a Rust type reachable by qualified path"
    /// lever: `concurrency.Shared[int]` -> `Ty::shared(int)`, `net.Socket` -> `Ty::Socket`,
    /// `ffi.int32` -> `Ty::Int`, etc. Returns `None` for a `sig.types` name that is NOT a builtin
    /// TYPE in type position (`timer`, a function — the caller emits the "function, not a type"
    /// error). Arity is NOT validated here — the mutable `resolve_type` caller emits arity
    /// diagnostics; the read-only `resolve_qualified_ro` caller is permissive (no errors). Shared
    /// by both qualified resolvers so the mapping can never drift between them.
    pub(super) fn qualified_builtin_ty(&self, name: &str, args: &[Ty]) -> Option<Ty> {
        let one = || args.first().cloned().unwrap_or(Ty::Unknown);
        let Some(b) = BuiltinType::from_name(name) else {
            return crate::native::ffi::is_width(name).then(|| ffi_width_ty(name));
        };
        match b {
            BuiltinType::Shared
            | BuiltinType::RwShared
            | BuiltinType::Atomic
            | BuiltinType::Iterator => b.applied_ty(vec![one()]),
            BuiltinType::AtomicInt
            | BuiltinType::Executor
            | BuiltinType::Socket
            | BuiltinType::Listener
            | BuiltinType::Writer
            | BuiltinType::Reader
            | BuiltinType::Ptr
            | BuiltinType::OwnedStr => b.plain_ty(),
            // Global builtins: no std module exports them by qualified path.
            BuiltinType::Int
            | BuiltinType::Float
            | BuiltinType::Bool
            | BuiltinType::Str
            | BuiltinType::Bytes
            | BuiltinType::ByteArray
            | BuiltinType::List
            | BuiltinType::Set
            | BuiltinType::Map
            | BuiltinType::Channel
            | BuiltinType::Range => None,
        }
    }

    /// Does `n` name a protocol with ANY STATIC method requirement (first param NOT `self`), directly OR
    /// through a transitively-embedded protocol? Such a protocol (e.g. `From`'s static-ctor
    /// `from(x: S) -> Self`, or a bundle `protocol MakeInt: From[int]` that EMBEDS it) is
    /// witnessable only by a STATIC method — a VALUE cannot invoke it — so it is usable ONLY as a generic
    /// bound `[T: P]`, never as a value-annotation type. Keys on the STRUCTURAL property (own + flattened
    /// embed requirements), so a future/user static-ctor protocol — and any bundle embedding one — is
    /// gated the same way. Ordinary instance-method protocols (all `is_static == false`) return `false`
    /// and stay usable as value existentials (`c: Container[int]`).
    pub(super) fn protocol_has_static_method(&self, n: &str) -> bool {
        let Some(p) = self.protocol_shape(n) else {
            return false;
        };
        if p.methods.iter().any(|(_, s)| s.is_static) {
            return true;
        }
        // Flatten the transitive embed method set (cycle-capped, read-only) and check it too, so a
        // bundle that pulls in a static-ctor requirement via `embeds` is gated the same as one declaring
        // it directly.
        let mut path = vec![n.to_string()];
        let (required, _cyclic, _conflict) = self.flatten_embed_methods(&p.embeds, &mut path);
        required.values().any(|s| s.is_static)
    }

    /// M24 — the type-param names of `decl` that need a hidden trailing witness argument. TWO
    /// conditions, both necessary:
    /// * the param has at least one bound (declared or `where`) whose protocol carries a STATIC
    ///   requirement, directly or through an embed — only such a bound has anything to witness; AND
    /// * the BODY either mentions the param's name (a `T.static()` call), or names a fn that itself
    ///   takes a witness — slice 2's FORWARDING, where this fn's own `$w:T` becomes the callee's
    ///   argument, so it must be charged one to have anything to pass.
    ///
    /// The second condition is what keeps a generic that never constructs whole: `fn label[T:
    /// Spawnable](x: T) -> str: return x.tag()` needs no witness, so it keeps its value position, its
    /// cross-module call sites and its `spawn`/`defer` target position — all legal before M24. A
    /// witness parameter is arity, so it must be paid for only where it is used.
    ///
    /// The DIRECT half (`T` mentioned anywhere in expression position) stays coarse: over-inclusion
    /// can only cost a fn positions it would lose anyway, while under-inclusion would mean a body
    /// whose `T.static()` cannot lower.
    ///
    /// The FORWARDING half is asked PER CALL SITE ([`Self::call_forwards_a_witness`]), not over the
    /// body's name set, because over-charging there does more than cost a position — a charged param
    /// that no call site can determine makes the fn UNCALLABLE ("type parameter 'T' … is not
    /// determined here"). A name is not a call (naming `lib` while some member is spelled `reset`
    /// never was `lib.reset(x)`), and a call is not necessarily a forward (a call whose every
    /// argument is a concrete constructor takes nothing of THIS fn's `T`). On top of the per-call
    /// answer two whole-fn fences remain:
    /// * the callee name must not be SHADOWED by one of this fn's own params
    ///   (`fn label[T: Spawnable](x: T, reset: int)` calls the param, never the module's `reset`) —
    ///   the free-name walk subtracts the parameter names, so no [`crate::compiler::CallSite`] is
    ///   recorded for it; and
    /// * the param must actually OCCUR in this fn's own signature ([`Self::ty_param_in_sig`]). A
    ///   param that appears in neither a parameter type nor the return type can never be bound to
    ///   anything at an INFERRED call site, so it can never be the thing forwarded there. An EXPLICIT
    ///   TURBOFISH at the caller (`outer[B](4)`) is a separate spelling that determines the param with
    ///   no argument at all, so it bypasses this fence ([`turbofish_charges`]).
    ///
    /// Because forwarding is transitive (`a` forwards into `b` which constructs), this answer depends
    /// on OTHER fns' answers, and a fn is hoisted before its callees. [`Checker::hoist`] therefore
    /// re-runs this to a FIXPOINT over the module's free fns once every signature exists. That loop
    /// is NOT monotone: `fn_sig`'s seed answer was computed mid-hoist against a struct table that did
    /// not yet hold the declarations below the fn, so the first re-run REMOVES charges as well as
    /// adding them. What makes it settle — and what the iteration cap does and does not buy — is
    /// spelled out at the loop itself. A forwarding target in
    /// ANOTHER module needs no fixpoint: modules are checked deps-first, so an imported callee's
    /// `witness_params` is already final when this module hoists (Task 3).
    ///
    /// Declaration order is preserved — it IS the witness-parameter order, so the checker's record
    /// site, the compiler's `$w:T` locals, and the call site's pushed arguments all agree.
    ///
    /// This is the ONE place the "does this fn need witnesses?" question is answered: the result is
    /// stored on [`FnSig::witness_params`] and every consumer reads it from there. The compiler never
    /// asks it (it cannot — protocol identity resolves through imports/aliases/embeds).
    pub(super) fn witness_params_of(&self, decl: &FnDecl) -> Vec<String> {
        let cands = self.static_bounded_type_params(decl);
        if cands.is_empty() {
            return cands; // the overwhelmingly common case — no body walk at all
        }
        // Reuse the whole-body free-name walker (the same one the nested-fn capture record uses): it
        // is exhaustive over statements and expressions and descends into closures, nested fns,
        // `recover:` blocks and string interpolation, so a `T.default()` anywhere in the body is seen.
        // The fn's OWN PARAM NAMES are subtracted: a param shadows a module-level fn of the same
        // name, so `reset` in the body of `fn f(reset: int)` is that param and forwards nothing.
        // (Subtracting them cannot hide a `T` mention — a type param and a value param never share a
        // name, and if they did the param would shadow the type anyway.)
        let params: std::collections::HashSet<String> =
            decl.params.iter().map(|p| p.name.clone()).collect();
        let mut walk = crate::compiler::FreeNames {
            // the MEMBER channel is checker-only; the compiler's capture-site walks skip it
            record_members: true,
            ..Default::default()
        };
        crate::compiler::free_names_block(&decl.body, &params, &mut walk);
        let mentioned = walk.names;
        // Slice 2 — the body names a fn that takes witnesses, so this body can FORWARD into it. Which
        // of this fn's params flows into which callee slot is type work only `record_witness_call`
        // can do (it runs long after this), so every static-bounded param THAT THIS SIGNATURE CAN
        // BIND is charged. A charge that turns out unused is inert arity; a MISSING one would be a
        // call the checker accepts and the compiler cannot lower.
        // The question is asked PER CALL SITE ([`crate::compiler::CallSite`]), never over the body's
        // name set: a name is not a call, and a call is not necessarily a forward.
        // M24-2 — the MEMBER channel. A member witness call (`h.build[T](x)`) shows up in neither
        // `mentioned` (`T` is a type ARGUMENT, which no free-name walk collects) nor `calls` (its head
        // is a RECEIVER, not a module), so without this the body is never charged and its own
        // `h.build[T](x)` is then REJECTED — "no hidden type witness for 'T' is reachable at this
        // call site" — with no way to write the program at all. The question is asked PER CALL SITE
        // and only about THIS declaration ([`Self::member_call_forwards_a_witness`]): does a type
        // argument or an argument carry something of `decl`'s own into the callee, AND can the
        // callee take a witness at all? Either half alone over-fires — the name index alone made one
        // unpinnable `get`/`push` poison that name for every member call in every static-bounded
        // generic, and the call-site half alone charged a BUILTIN `sink.push(x)` that has no witness
        // parameter to receive anything.
        let mut forwards = false;
        let mut turbofished: std::collections::HashSet<String> = std::collections::HashSet::new();
        for c in &walk.calls {
            if self.call_forwards_a_witness(c, decl) {
                forwards = true;
                turbofish_charges(&c.type_args, &cands, &mut turbofished);
            }
        }
        for c in &walk.member_calls {
            if self.member_call_forwards_a_witness(c, decl, &cands) {
                forwards = true;
                turbofish_charges(&c.type_args, &cands, &mut turbofished);
            }
        }
        cands
            .into_iter()
            .filter(|t| {
                mentioned.contains(t)
                    || turbofished.contains(t)
                    || (forwards && Self::ty_param_in_sig(decl, t, true))
            })
            .collect()
    }

    /// M24 — the witness slots of a fn whose witnessed type params are `names` (from
    /// [`Self::witness_params_of`]) and whose resolved params are `tps`: one per (type param, static
    /// requirement of its bounds, own or embedded), in declaration order, requirements by name. Two
    /// bounds with the same requirement (same name and param types) share one slot.
    pub(super) fn witness_reqs(&self, names: &[String], tps: &[TyParam]) -> Vec<WitnessParam> {
        let mut out: Vec<WitnessParam> = Vec::new();
        for tp in tps.iter().filter(|tp| names.contains(&tp.name)) {
            for b in &tp.bounds {
                let map = self.bound_subst(&tp.name, b);
                for m in self.protocol_method_names(&b.name) {
                    let Some(msig) = self.protocol_method_sig(&b.name, &m) else {
                        continue;
                    };
                    if !msig.is_static {
                        continue;
                    }
                    let w = WitnessParam {
                        param: tp.name.clone(),
                        method: m,
                        params: msig.params.iter().map(|t| subst(t, &map)).collect(),
                        ret: subst(&msig.ret, &map),
                    };
                    if !out.iter().any(|o| o.tag() == w.tag()) {
                        out.push(w);
                    }
                }
            }
        }
        out
    }

    /// M24 — `decl`'s type params with at least one bound (declared or `where`) whose protocol
    /// carries a STATIC requirement: the CANDIDATES for a hidden witness argument, before any body
    /// question. Read by [`Self::witness_params_of`].
    pub(super) fn static_bounded_type_params(&self, decl: &FnDecl) -> Vec<String> {
        decl.type_params
            .iter()
            .filter(|tp| {
                // A free fn's `where T: P` is merged into `type_params` by `fn_sig`, but this runs
                // BEFORE/independently of that merge, so consider both spellings.
                let wheres = decl
                    .where_bounds
                    .iter()
                    .filter(|w| w.name == tp.name)
                    .flat_map(|w| w.bounds.iter());
                tp.bounds
                    .iter()
                    .chain(wheres)
                    .any(|b| self.protocol_has_static_method(&b.name))
            })
            .map(|tp| tp.name.clone())
            .collect()
    }

    /// M24 — does this ONE call site inside `decl`'s body forward a witness of `decl`'s own?
    ///
    /// Two independent questions, both of which must answer yes:
    /// * **does the callee take witnesses at all?** A bare `f(…)` resolves through `self.functions`
    ///   (this module's fns and its `from`-imports — one table, so `import reset as again from m` is
    ///   covered); a qualified `m.f(…)` resolves the module bind, then looks up *that exact function
    ///   name* in its `ModuleSig`. The PAIR, never two independent halves: a module exports many fns
    ///   and only some take witnesses, so naming `lib` in a body that also happens to spell some
    ///   member `reset` is not a call to `lib.reset`.
    /// * **could this fn's own type param be what is forwarded?** Only if the call does NOT pin the
    ///   callee's witnesses itself. It pins them when every argument is provably closed
    ///   ([`crate::compiler::CallSite::closed_arg_heads`]) with every constructor head naming a
    ///   NON-GENERIC struct in scope — so the argument types cannot mention a type parameter — AND
    ///   every witness the callee takes occurs in the callee's PARAMETER types, so the arguments
    ///   really do determine them. A witness that occurs only in the callee's RETURN type is
    ///   inferred from the expected type at this call site, which is this fn's own `T`
    ///   (`fn conv[T: Default, U](a: U) -> T` called as `conv(1)`), so that is a forward however
    ///   concrete the arguments look.
    ///
    /// Everything the walk could not positively identify — an unrecognised argument shape, a head
    /// that is not a known non-generic struct, an unknown callee — answers "forwards", because an
    /// under-charge is a forward the checker then REFUSES (`witness_scope` has no `$w:T` to load)
    /// or, worse, one the compiler cannot lower.
    ///
    /// An over-charge is the LESS bad direction, not a free one: the position it costs is the
    /// difference between compiling and not. A charged fn can no longer be read as a function value,
    /// passed to a HOF, or used as a `spawn f(…)` / `defer f(…)` target — so a wrong charge does not
    /// merely make a program slower, it makes an unrelated program stop compiling (`m.get("a")` once
    /// did exactly that to every `uses_get`-shaped generic). Both directions need a reason.
    fn call_forwards_a_witness(&self, c: &crate::compiler::CallSite, decl: &FnDecl) -> bool {
        let sig = match &c.module {
            None => self.functions.get(&c.name),
            Some(m) => self
                .imported_modules
                .get(m)
                .and_then(|id| self.module_sigs.get(id))
                .and_then(|s| s.certain_fn(&c.name)),
        };
        let Some(sig) = sig.filter(|s| !s.witness_params.is_empty()) else {
            return false;
        };
        let args_pin_the_witnesses = c
            .closed_arg_heads
            .as_ref()
            .is_some_and(|heads| heads.iter().all(|h| self.concrete_ctor_head(h, decl)))
            && sig
                .witness_params
                .iter()
                .all(|w| Self::ty_param_in_params(sig, &w.param, &c.supplied));
        !args_pin_the_witnesses
    }

    /// M24-2 — does this ONE MEMBER call site inside `decl`'s body forward a witness of `decl`'s own?
    ///
    /// A member call gives a pre-type walk no callee to resolve — the receiver's type is exactly what
    /// is not known yet — so neither half of the question can be answered alone, and BOTH must say
    /// yes:
    ///
    /// 1. **this call site carries something of `decl`'s own** — a TYPE ARGUMENT names one of
    ///    `decl`'s witness CANDIDATES (`h.make[T](x)`, `h.mk[T]()`), or an ARGUMENT mentions a value
    ///    parameter of `decl` whose annotation mentions one of `decl`'s type params (`h.make(x)` with
    ///    `x: T`, and `h.make(f(xs[0]))` with `xs: List[T]` —
    ///    [`crate::compiler::MemberCall::arg_idents`] is collected at any depth for exactly that);
    ///    **and**
    /// 2. **the method NAME is declared as witness-taking somewhere in the module graph**
    ///    ([`Checker::witness_member_names`]) — otherwise the callee is a builtin or a plain method
    ///    and there is no witness parameter for anything to flow into.
    ///
    /// Each half alone is a measured defect. Half 2 alone made one unpinnable `get` poison every
    /// `m.get("a")` in the program. Half 1 alone charged `sink.push(x)` where `sink: List[T]` — a
    /// BUILTIN `List.push`, which can never take a witness — costing `label` its function-value
    /// position for a forward that does not exist. ANDed they are strictly narrower than either:
    /// `m.get("a")` fails 1 (a literal argument names nothing of `decl`'s), `sink.push(x)` fails 2
    /// unless some user type really declares a witness-taking `push`.
    ///
    /// **Under-charging is the unsafe direction** (the charge and the capture must agree, and the
    /// checker then refuses a forward it did not charge), so within half 1 both clauses stay
    /// generous: `arg_idents` carries every ident in an argument with no bound-name subtraction, and
    /// the parameter clause reads ALL of `decl`'s type params. Type ARGUMENTS are matched against the
    /// static-bounded candidates only, because those are the only params a witness can ever be
    /// charged for — `cands` is what the caller then filters by, so a non-candidate turbofish could
    /// only charge an unrelated `T`. What this still cannot see is a `T` that reaches the argument
    /// through something other than a parameter — a LOCAL (`v := x` then `h.make(v)`), a field, a
    /// call result — which is refused with "no hidden type witness for 'T' is reachable at this call
    /// site"; the turbofish (`h.make[T](v)`) is the spelling that always charges, whether or not
    /// `decl`'s own `T` occurs in `decl`'s signature ([`turbofish_charges`] bypasses that fence).
    fn member_call_forwards_a_witness(
        &self,
        c: &crate::compiler::MemberCall,
        decl: &FnDecl,
        cands: &[String],
    ) -> bool {
        if !self.witness_member_names.contains(&c.name) {
            return false;
        }
        let names_a_type_param =
            |ty: &crate::ast::Type| decl.type_params.iter().any(|tp| ty_mentions(ty, &tp.name));
        c.type_args
            .iter()
            .any(|ty| cands.iter().any(|t| ty_mentions(ty, t)))
            || c.arg_idents.iter().any(|a| {
                decl.params
                    .iter()
                    .any(|p| p.name == *a && p.ty.as_ref().is_some_and(&names_a_type_param))
            })
    }

    /// M24 — does the call-argument constructor head `head` name a NON-GENERIC struct visible here?
    /// Then `head(…)`'s type is fixed by the declaration alone and cannot mention any type parameter.
    /// Anything else is `false` (charge): one of `decl`'s own type params, a plain function whose
    /// return type this syntactic walk cannot see, a generic struct (its args could be `T`), or a
    /// name this module does not know. A variant never reaches here as a struct: a bare variant
    /// head is an imported variant (`Som(2)` after `import Som from Opt1`, or the prelude's
    /// `Some`/`Ok`/`Err`); it is not in `struct_names`, so it answers `false` -- the charge
    /// direction, the safe one. A bare variant that is not imported is a checker error before this
    /// runs.
    fn concrete_ctor_head(&self, head: &str, decl: &FnDecl) -> bool {
        !decl.type_params.iter().any(|tp| tp.name == head)
            && self.struct_names.contains(head)
            && self
                .struct_shape(&self.bare_key(head))
                .is_some_and(|info| info.type_params.is_empty())
    }

    /// M24 — does type param `w` occur in `sig`'s PARAMETER types (the return type does not count)?
    /// Reuses [`subst`] as the occurs-check: replacing `w` changes the type iff `w` was in it.
    ///
    /// OCCURRENCE, not supply. It asks whether SOME parameter mentions `w`, never whether the call
    /// site passed that parameter — so on paper `fn conv[T: Default](a: int, b: T? = None) -> T`
    /// called as `conv(1)` satisfies this clause while supplying nothing that pins `T`. What keeps
    /// that from being an UNDER-charge is not this clause but two properties of defaults, both
    /// measured and both load-bearing:
    /// * an inline default is spliced into the CALL SITE by `desugar` before the checker runs, so the
    ///   recorded [`crate::compiler::CallSite`] is `conv(1, None)` — and the inline defaults a
    ///   `w`-mentioning slot can take are not CLOSED shapes: `compiler::closed_expr` admits only
    ///   scalar literals and ident-headed calls, and `None` / `[]` / `{}` are neither (a scalar
    ///   literal would fit only a slot whose `w` is already pinned to that scalar). So
    ///   `closed_arg_heads` is `None`, and `call_forwards_a_witness` never reaches this clause; and
    /// * the callee-filled alternative — a NON-inline default, which stays out of the call site — no
    ///   longer refuses a short call BY ITSELF (W8-47): `fn conv[T](a: int, b: List[T] = List())`
    ///   called as `conv(1)` is `ok: no type errors`, since `T` carries no witness. What still
    ///   refuses the WITNESS-taking shape is `ast::min_callable_params`, which returns
    ///   `sig.params.len()` (not the shrunk count) whenever the sig carries witness params or is
    ///   variadic: `fn conv[T: Default](a: int, b: List[T] = List())` called as `conv(1)` inside a
    ///   witness-taking caller stays `'conv' expects 2 argument(s), got 1`.
    ///
    /// A change to either — splicing moved after the hoist, or `min_callable_params`'s witness-params
    /// early return deleted — re-opens the hole silently. The defaulted-parameter case in
    /// `witness_forwarding_still_charges_every_unpinned_shape_rejected` is the pin, alongside
    /// `w8_47_a_witness_taking_callee_keeps_its_full_arity`.
    fn ty_param_in_params(sig: &FnSig, w: &str, supplied: &(usize, Vec<String>)) -> bool {
        let probe: HashMap<String, Ty> = [(w.to_string(), Ty::Unknown)].into_iter().collect();
        let (npos, named) = supplied;
        sig.params.iter().enumerate().any(|(i, p)| {
            let filled = i < *npos
                || sig
                    .labels
                    .get(i)
                    .and_then(|l| l.as_deref())
                    .is_some_and(|l| named.iter().any(|n| n == l));
            filled && subst(p, &probe) != *p
        })
    }

    /// M24 — does type param `t` occur in `decl`'s own signature (any parameter's annotation and,
    /// with `with_ret`, the return annotation)? A param that occurs in NEITHER can never be bound by
    /// a call site, so it can never be the type a witness is forwarded for — charging it would only
    /// make the fn uncallable. Syntactic on purpose: this runs at the signature hoist, before
    /// resolution. `with_ret = false` asks the PARAMS only, which is the different question "can this
    /// declaration's witness be pinned by ARGUMENTS?" — the decl-side twin of
    /// [`Self::ty_param_in_params`], used to index a member's pinnability at the hoist.
    pub(super) fn ty_param_in_sig(decl: &FnDecl, t: &str, with_ret: bool) -> bool {
        decl.params
            .iter()
            .filter_map(|p| p.ty.as_ref())
            .chain(decl.ret.as_ref().filter(|_| with_ret))
            .any(|ty| ty_mentions(ty, t))
    }

    /// Find the first STATIC-CTOR protocol embedded ANYWHERE in a (possibly nested) already-resolved
    /// `Ty` — directly (`From[int]`), or nested under a container / `Option` / tuple / `Result` /
    /// `Func` / struct-or-enum type arg. Read-only. This is the value-position gate for a `Ty` that
    /// arrives ALREADY RESOLVED — a cross-module type-alias body, computed by the `&self` read-only
    /// resolver (`resolve_ty_ro_d`) which cannot emit — so the mutable `resolve_type` arm gate never
    /// sees it. `resolve_type` itself recurses through the arm gate for every FRESHLY-resolved position,
    /// so this only needs to run at the pre-resolved-body seams.
    fn first_static_ctor_protocol(&self, ty: &Ty) -> Option<String> {
        match ty {
            Ty::Protocol(n, args) => {
                if self.protocol_has_static_method(n) {
                    Some(n.clone())
                } else {
                    args.iter().find_map(|a| self.first_static_ctor_protocol(a))
                }
            }
            Ty::List(t)
            | Ty::Set(t)
            | Ty::Option(t)
            | Ty::Channel(t)
            | Ty::Shared(t)
            | Ty::Atomic(t)
            | Ty::RwShared(t) => self.first_static_ctor_protocol(t),
            Ty::Map(k, v) | Ty::Result(k, v) => self
                .first_static_ctor_protocol(k)
                .or_else(|| self.first_static_ctor_protocol(v)),
            Ty::Tuple(ts) | Ty::Struct(_, ts) | Ty::Enum(_, ts) => {
                ts.iter().find_map(|t| self.first_static_ctor_protocol(t))
            }
            Ty::Func { params, ret, .. } => params
                .iter()
                .find_map(|p| self.first_static_ctor_protocol(p))
                .or_else(|| self.first_static_ctor_protocol(ret)),
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::Param(_)
            | Ty::AtomicInt
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader
            | Ty::Ptr
            | Ty::Width(_)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => None,
        }
    }

    /// If a resolved value-position `ty` embeds a static-ctor protocol (arriving pre-resolved from a
    /// cross-module alias body, so the `resolve_type` arm gate never fired), reject it with the SAME
    /// bound-only error the arms emit and return `Ty::Unknown`; otherwise return `ty` unchanged.
    pub(super) fn reject_static_protocol_value(&mut self, ty: Ty, span: Span) -> Ty {
        if let Some(n) = self.first_static_ctor_protocol(&ty) {
            self.error(
                span,
                format!(
                    "protocol '{n}' has a static method and can only be used as a bound, not a value type"
                ),
            );
            Ty::Unknown
        } else {
            ty
        }
    }
}
