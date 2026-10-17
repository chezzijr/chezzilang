//! Checker: function values and type paths: generic fn values, `path_fn`, type members.
//! Split out of `src/checker/pattern.rs`. `super::*` is the parent module.

use super::infer_access::json_decode_sig;
use super::resolve::PathPos;
use super::setup::{HeadBinding, TypeHead, TypeHeadKind, TypeRead};
use super::*;

impl Checker {
    /// M24 — PERMANENT WALL, not a v1 limit: a generic fn that takes hidden witness arguments
    /// (`wparams`, from [`FnSig::witness_params`]) may not become a function VALUE. A `Ty::Func`
    /// erases which declaration it came from, so no witness can ever be recovered at the eventual
    /// indirect call — the value would be called one argument short. Rejected at the READ, where the
    /// name is still known. Every path that can hand back a `Ty::Func` for a named fn routes through
    /// here: the bare read (`g := reset`), the turbofish read (`reset[Counter]`, incl. as a HOF
    /// argument), and a cross-module member read (`lib.reset`). Returns `true` if it rejected.
    pub(super) fn reject_witness_fn_value(
        &mut self,
        name: &str,
        wparams: &[String],
        span: Span,
    ) -> bool {
        if wparams.is_empty() {
            return false;
        }
        self.error(
            span,
            format!(
                "'{name}' cannot be used as a function value: its bound on {} requires a static \
                 protocol method, which needs the concrete type — a function value erases it. \
                 Call '{name}' directly, or pass a factory closure instead (e.g. \
                 `fn make[T](mk: fn() -> T) -> T`)",
                wparams.join(", ")
            ),
        );
        true
    }

    /// W7-42r shape (b) — reject a VALUE read of an imported name that sits ABOVE that name's own
    /// `import`. Imports are HOISTED (`check_module` runs `bind_import` for every import before the
    /// `check_stmt` loop), so the name is in `scopes[0]` from line 1 whatever line the `import` is
    /// on, and a read above it silently resolves to the imported binding. Go rejects the late
    /// `import`; the forward READ is the error here.
    ///
    /// Deliberately also rejects programs that are TECHNICALLY SOUND today (`print(COUNT)` above
    /// `import COUNT from lib` works, because of the hoist): it reads as a use-before-definition,
    /// and both owning ancestors refuse it — CPython raises `NameError`, Go will not even parse an
    /// `import` after a declaration. Kept narrow on purpose: VALUE/CALLABLE reads only (a bare type
    /// name resolves through `bare_types`/`resolve_type`, never here), and NOT Go's full "imports
    /// before all code" rule, which would be a grammar change.
    ///
    /// A DEFERRED read — a top-level `fn` body above the `import` — is rejected too, and there the
    /// ancestors SPLIT (measured): CPython accepts it, because the body runs after the import; Go
    /// still refuses, because it will not take a late `import` at all. We follow Go, because the
    /// hoist makes the sound and the unsound case indistinguishable AT THE READ SITE: the same
    /// `COUNT` in the same position is fine until some later `let` refills the slot, and the reader
    /// cannot see which it is. Do not loosen this to "only immediate reads".
    ///
    /// "Still the import's binding" is the exact test the W7-42 rule uses (`sig.rs`): a module-scope
    /// `declare` clears `imported_values` (`setup.rs:1774`), so once a `let` has handed the name
    /// back to this module the read is that let's, not the import's. For a from-imported FN the
    /// caller passes `is_import = true` and the `import_binds` lookup below is the whole gate — a
    /// same-module top-level `fn` is never in `import_binds`, and must not be: it is legitimately
    /// position-independent (`compiler/mod.rs:1404`, `desugar/mod.rs:689`). "Above its import" is
    /// the same directional `import_binds` span comparison, in the opposite direction — total for
    /// SHADOWING is the CALLER's job, and each caller already carries it: the value arm passes
    /// `is_import` only when no scope above 0 holds the name (see `infer_ident`), while both
    /// `functions`-arm callers are reached only after `lookup(name)` came back `None` — a
    /// parameter/local/loop/block binding of that name would have resolved there first, so a
    /// from-imported fn shadowed by an inner binding never reaches this gate at all.
    /// the same two reasons (imports are top-level only; no statement separator, so no positional
    /// tie). Writes need no counterpart: `check_assign` already rejects an assignment to a
    /// from-imported global at ANY position ("cannot assign to 'x' imported from module …"), and a
    /// whole-module bind is a `Ty::Module` that no value is assignable to.
    pub(super) fn reject_read_above_import(&mut self, name: &str, is_import: bool, span: Span) {
        if !is_import {
            return;
        }
        let Some(imp) = self.import_binds.get(name).copied() else {
            return;
        };
        if (span.line, span.col) < (imp.line, imp.col) {
            self.error(
                span,
                format!(
                    "'{name}' is used before its `import` on line {} (imports are hoisted, so this \
                     reads the imported binding — move the `import` above this line)",
                    imp.line
                ),
            );
        }
    }

    /// TICKET-187 — the one pin-or-reject rule for a GENERIC fn read as a value, shared by every
    /// read: same-module and from-imported bare names (Scope A in `infer_ident`) and qualified
    /// `m.f` (the `Ty::Module` field read). `Some(ty)` when the rule decided (pinned, or rejected
    /// as `Ty::Unknown`); `None` to fall through to the rigid `fn(T) -> T`, whose assignability
    /// diagnostic is the accurate one there. A rigid callee `T` must never reach a caller's scope,
    /// because `Ty::Param` compares by name (G1).
    pub(super) fn generic_fn_value_ty(
        &mut self,
        node: crate::ast::NodeId,
        name: &str,
        sig: &FnSig,
        spelling: &str,
        span: Span,
    ) -> Option<Ty> {
        if sig.type_params.is_empty() {
            return None;
        }
        let type_params = sig.type_params.clone();
        let declared = fn_value_ty(sig);
        // A hint of `Unknown` DETERMINES NOTHING and must count as no hint at all, or the rule
        // cancels itself on an INFERRED return type (`fn get(): return id`): the
        // return-inference pass reads the body, takes the `Ty::Unknown` this arm returns as the
        // inferred return, and the real pass re-checks the same `return id` against a
        // `Some(Unknown)` hint — turning a reject into a silent ACCEPT (measured: `g := get();
        // g(1)` printed `1`, check-clean).
        // A protocol hint (`print(ident)`, `oa(ident)` over `fn oa(v: Any)`) fixes nothing either
        // (`hint_fixes`, owner rule 2026-10-17), so the read reports as with no hint.
        let hint = self.expected_hint.clone().filter(hint_fixes);
        let verdict = match &hint {
            Some(h) => {
                pin_generic_fn_value(&type_params, &declared, h, &|n| self.rigid_param(n, &[]))
            }
            // No hint at all: nothing in this position can determine anything.
            None => FnValuePin::Undetermined,
        };
        match verdict {
            FnValuePin::Pinned(map, refined) => {
                self.enforce_bounds(&type_params, &type_params, &map, span);
                // A method value's receiver `where` bound; empty for a fn.
                self.enforce_bounds(&sig.where_bounds, &type_params, &map, span);
                return Some(refined);
            }
            // …the value can never be formed. Go refuses exactly this spelling, at the READ:
            // `cannot use generic function id without instantiation` (and, in argument
            // position, `in call to takeBool, cannot infer T`). Chezzi used to accept it and
            // blame the eventual call ("argument 1 of 'closure': expected T, found int" — a
            // `closure` the user never wrote, naming a `T` there is no way to act on), or
            // accept it silently when the value was never called. The
            // witness wall above wins first — its advice differs (a turbofish does not help).
            // …gated by the hint's own parameter positions (see `fn_slot_params_concrete`):
            // a hint that is not concrete there cannot answer the question, so the rigid
            // arm's assignability diagnostic owns it. No hint at all still reports (`g := id`).
            //
            // TICKET-225 (R5, amends DEC-197): the read is no longer the final word. It takes one
            // type variable per param, and another operand of the same statement binds them
            // (TICKET-238: the frame is one statement); the frame verdict (`close_tyvar_frame`)
            // reports a read still unbound, with this same message.
            FnValuePin::Undetermined
                if hint
                    .as_ref()
                    .is_none_or(|h| fn_slot_params_concrete(h, &|n| self.rigid_param(n, &[]))) =>
            {
                return Some(self.defer_generic_fn_value(node, name, sig, spelling, span, false));
            }
            // Not this rule's business (see [`FnValuePin::Skip`]) — fall through to the rigid
            // `fn(T) -> T` arm and let the existing assignability diagnostic, which is the
            // accurate one there, speak.
            FnValuePin::Undetermined | FnValuePin::Skip => {}
        }
        None
    }

    /// TICKET-187 — is `e` a read of a GENERIC fn-like path as a value, and which one. Returns the
    /// display name, the sig with any head args (written or alias-pinned) substituted, and the
    /// instantiation hint. A view over [`Self::path_fn`] (TICKET-204): `Bx[int].put` and `B.put`
    /// answer with only `put`'s own `U` free, exactly as `Bx.put` is re-pinned.
    pub(super) fn generic_fn_value_sig(&self, e: &Expr) -> Option<(String, FnSig, String)> {
        self.path_fn(e)
            .and_then(|pf| self.pin_path_head(pf))
            .filter(|pf| !pf.sig.type_params.is_empty())
            .map(|pf| (pf.display, pf.sig, pf.spelling))
    }

    /// The path value `T.m` for an in-scope type param `T`: a bound's INSTANCE method `m`, as a fn
    /// taking the receiver first (Rust's `T::m`). `None` for a static requirement (call-only,
    /// through `infer_witness_static_call`) and a miss. Protocol methods take no own type params,
    /// so the value is never generic.
    pub(super) fn param_member_fn(&self, tname: &str, name: &str) -> Option<PathFn> {
        let (_, msig, map) = self.bound_method(tname, name, &|s| !s.is_static)?;
        let mut inst = subst_sig(&msig, &map);
        inst.ret = self.bound_method_ret(&msig, &map);
        let sig = method_value_sig(&inst, &[], Ty::Param(tname.to_string()));
        let arity = sig.params.len();
        Some(PathFn {
            display: format!("{tname}.{name}"),
            head_spelled: tname.to_string(),
            spelling: format!("{tname}.{name}"),
            sig,
            head_decl: Vec::new(),
            head_params: 0,
            head_args: None,
            head_pinned: None,
            res: Some(Resolution::ParamMethodFn {
                method: name.to_string(),
                arity,
            }),
        })
    }

    /// The `m.f` half of [`Self::path_fn`]: `m` is a whole-module import here and `f` is one of its
    /// fns.
    pub(super) fn module_fn(&self, m: &str, name: &str) -> Option<(String, FnSig)> {
        if !matches!(self.head_binding(m), HeadBinding::Module) {
            return None;
        }
        if self.json_decode_member(m, name) {
            return Some((format!("{m}.{name}"), json_decode_sig()));
        }
        let msig = self.module_sigs.get(self.imported_modules.get(m)?)?;
        let sig = msig.certain_fn(name)?;
        Some((format!("{m}.{name}"), sig.clone()))
    }

    /// [`Self::module_fn`] when `f` is GENERIC.
    pub(super) fn generic_module_fn(&self, m: &str, name: &str) -> Option<(String, FnSig)> {
        self.module_fn(m, name)
            .filter(|(_, sig)| !sig.type_params.is_empty())
    }

    /// THE one answer to "is this path a fn-like item read as a value, with which signature"
    /// (TICKET-204, Rust's path-value rule): a same-module or from-imported fn no local shadows,
    /// `m.f` on a whole-module import, or a type member — a static method, an instance method named
    /// through its type (receiver first), or a payload variant. Generic or not; the caller filters.
    pub(super) fn path_fn(&self, e: &Expr) -> Option<PathFn> {
        match &e.kind {
            ExprKind::Ident(name) => {
                if !matches!(
                    self.head_binding(name),
                    HeadBinding::Unbound | HeadBinding::Global
                ) || self.lookup(name).is_some()
                {
                    return None;
                }
                if let Some(sig) = self.functions.get(name) {
                    return Some(PathFn::of_fn(name.clone(), sig.clone()));
                }
                // A bare imported variant constructor, through the head its import stored.
                let iv = self.imported_variants.get(name)?;
                self.type_member_fn(&iv.head, None, &iv.variant)
            }
            ExprKind::Field { obj, name, .. } => {
                // A type parameter shadows a module or type of that name here, as in a call.
                if let ExprKind::Ident(t) = &obj.kind
                    && self.shadowing_type_param(t)
                {
                    return self.param_member_fn(t, name);
                }
                if let ExprKind::Ident(m) = &obj.kind
                    && let Some((display, sig)) = self.module_fn(m, name)
                {
                    return Some(PathFn::of_fn(display, sig));
                }
                let (th, head_args) = self.peel_type_path(obj)?;
                self.type_member_fn(&th, head_args, name)
            }
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
            | ExprKind::ElseGuard { .. } => None,
        }
    }

    /// The type head of a member path's receiver `obj`, with its written type arguments: `Bx`,
    /// `Bx[int]`, `vlib.R2[int, str]`, or an alias `B`. An alias given type arguments (`A[int]`)
    /// is still a type path; its readers reject it through [`Self::written_head_args`].
    pub(super) fn peel_type_path(&self, obj: &Expr) -> Option<(TypeHead, Option<WrittenTypeArgs>)> {
        let (head, args) = match crate::ast::type_application(obj) {
            Some(app) => (app.head, Some((app.args, app.args_span))),
            None => (obj, None),
        };
        let th = self.type_head(head)?;
        Some((th, args))
    }

    /// The fn-like member `name` of type head `th`: a payload variant (a constructor fn) or a
    /// method (an instance method takes its receiver first, keyword `self`). `None` for a nullary
    /// variant, a protocol, a miss, and a native handle, whose methods have no proto.
    pub(super) fn type_member_fn(
        &self,
        th: &TypeHead,
        head_args: Option<WrittenTypeArgs>,
        name: &str,
    ) -> Option<PathFn> {
        if th.native_handle {
            return None;
        }
        let key = &th.key;
        let params_of = |tps: &[TyParam]| {
            tps.iter()
                .map(|tp| Ty::Param(tp.name.clone()))
                .collect::<Vec<_>>()
        };
        let method = |tps: Vec<TyParam>, msig: &FnSig, recv: Ty| {
            let res = Resolution::MethodFn {
                type_key: key.clone(),
                method: name.to_string(),
            };
            (method_value_sig(msig, &tps, recv), tps, res)
        };
        let (mut sig, head_decl, res) = match th.kind {
            TypeHeadKind::Enum => {
                let tps = self.enum_type_params.get(key).cloned().unwrap_or_default();
                let recv = Ty::enum_ty(key.clone(), params_of(&tps));
                if let Some(v) = self.variants.get(&(key.clone(), name.to_string())) {
                    if v.payload.is_empty() {
                        return None;
                    }
                    let res = Resolution::VariantFn {
                        enum_key: key.clone(),
                        variant: name.to_string(),
                    };
                    let mut sig = FnSig::plain(v.payload.clone(), recv);
                    sig.type_params = tps.clone();
                    (sig, tps, res)
                } else {
                    method(tps, self.enum_methods.get(key)?.get(name)?, recv)
                }
            }
            TypeHeadKind::Struct => {
                let info = self.struct_shape(key)?;
                let tps = info.type_params.clone();
                let recv = Ty::Struct(key.clone(), params_of(&tps));
                method(tps, info.methods.get(name)?, recv)
            }
            TypeHeadKind::Protocol => return None,
        };
        // A head param the instantiated sig no longer names (`Box[int].make2`) leaves the sig but
        // stays in `head_decl`, so written head args still arity-check against the declaration.
        let mut occurring = Vec::new();
        for t in sig.params.iter().chain(std::iter::once(&sig.ret)) {
            ty_collect_params(t, None, &mut occurring);
        }
        let own = sig.type_params.split_off(head_decl.len());
        let kept: Vec<TyParam> = head_decl
            .iter()
            .filter(|tp| occurring.contains(&tp.name))
            .cloned()
            .collect();
        let head_params = kept.len();
        sig.type_params = kept.iter().cloned().chain(own.iter().cloned()).collect();
        let head_text = match &head_args {
            Some((args, _)) => format!(
                "{}[{}]",
                th.spelled,
                args.iter()
                    .map(|t| self.resolve_ty_ro(t).to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            None => th.spelled.clone(),
        };
        let display = format!("{head_text}.{name}");
        let hint_head = if head_args.is_none() && th.pinned.is_none() {
            fn_spelling(&th.spelled, &kept)
        } else {
            head_text
        };
        Some(PathFn {
            display,
            head_spelled: th.spelled.clone(),
            sig,
            head_decl,
            head_params,
            head_args,
            head_pinned: th.pinned.clone(),
            res: Some(res),
            spelling: fn_spelling(&format!("{hint_head}.{name}"), &own),
        })
    }

    /// The one decision for a type head given type arguments, in every position: an alias
    /// (`pinned` is `Some`, even `Some([])` for a non-generic alias) given written args reports
    /// that it already fixes its arguments and answers `None`. Else the written args when there are
    /// any, else the alias's pinned args, else none. A new site calls this; it never tests `pinned`.
    pub(super) fn written_head_args(
        &mut self,
        spelled: &str,
        decl_params: usize,
        pinned: Option<Vec<Ty>>,
        written: Vec<Ty>,
        span: Span,
    ) -> Option<Vec<Ty>> {
        if head_args_clash(pinned.as_deref(), decl_params, !written.is_empty()) {
            self.error(
                span,
                format!(
                    "type alias '{spelled}' already fixes its type arguments; write the aliased type to pass your own"
                ),
            );
            return None;
        }
        if written.is_empty() {
            Some(pinned.unwrap_or_default())
        } else {
            Some(written)
        }
    }

    /// How many type params the struct or enum keyed `key` declares.
    pub(super) fn type_param_count(&self, key: &str) -> usize {
        self.type_params_of(key).map_or(0, |tps| tps.len())
    }

    /// The type params the struct or enum keyed `key` declares; `None` for no such type.
    pub(super) fn type_params_of(&self, key: &str) -> Option<Vec<TyParam>> {
        if let Some(info) = self.struct_shape(key) {
            return Some(info.type_params.clone());
        }
        self.enum_type_params.get(key).cloned()
    }

    /// The one place head args become a substitution for a `&self` reader: the alias-pinned args,
    /// else the written ones (resolved with `resolve_ty_ro`), else none (`pf` unchanged). `None` on
    /// an arity mismatch, which [`Self::path_fn_value_ty`] reports at the read.
    fn pin_path_head(&self, mut pf: PathFn) -> Option<PathFn> {
        if head_args_clash(
            pf.head_pinned.as_deref(),
            pf.head_decl.len(),
            pf.head_args.is_some(),
        ) {
            return None;
        }
        let args: Vec<Ty> = if let Some(p) = &pf.head_pinned {
            p.clone()
        } else if let Some((a, _)) = &pf.head_args {
            a.iter().map(|t| self.resolve_ty_ro(t)).collect()
        } else {
            return Some(pf);
        };
        if args.len() != pf.head_decl.len() {
            return None;
        }
        let map: HashMap<String, Ty> = pf
            .head_decl
            .iter()
            .map(|tp| tp.name.clone())
            .zip(args)
            .collect();
        let mut sig = subst_sig(&pf.sig, &map);
        sig.type_params = sig.type_params.split_off(pf.head_params);
        pf.spelling = fn_spelling(&pf.display, &sig.type_params);
        pf.sig = sig;
        pf.head_params = 0;
        pf.head_decl = Vec::new();
        pf.head_args = None;
        pf.head_pinned = None;
        Some(pf)
    }

    /// The value type of fn-like path `pf` read here, with its own written type args `own_args`
    /// (a turbofish), if any: the witness wall, the head and own arity checks, the bounds over the
    /// type's declared params followed by the item's own (DEC-202), then the substituted fn type —
    /// or, with params left free, the pin-or-reject rule of [`Self::generic_fn_value_ty`].
    pub(super) fn path_fn_value_ty(
        &mut self,
        node: crate::ast::NodeId,
        pf: PathFn,
        own_args: Option<WrittenTypeArgs>,
        span: Span,
    ) -> Ty {
        // M24 — the fn-as-value wall: pinning the type params does NOT recover the witness (the pin
        // is checker-only, the runtime value is the same erased function).
        if self.reject_witness_fn_value(&pf.display, &pf.sig.witness_params, span) {
            return Ty::Unknown;
        }
        let own = pf.sig.type_params[pf.head_params..].to_vec();
        let mut map = HashMap::new();
        let mut arity_ok = true;
        let written: Vec<Ty> = match &pf.head_args {
            Some((args, aspan)) => args.iter().map(|t| self.resolve_type(t, *aspan)).collect(),
            None => Vec::new(),
        };
        let Some(head) = self.written_head_args(
            &pf.head_spelled,
            pf.head_decl.len(),
            pf.head_pinned.clone(),
            written,
            span,
        ) else {
            return Ty::Unknown;
        };
        if pf.head_args.is_some() {
            arity_ok &= head.len() == pf.head_decl.len();
            map.extend(self.seed_targs(&pf.head_spelled, &pf.head_decl, &head, span));
        } else {
            for (tp, t) in pf.head_decl.iter().zip(head) {
                map.insert(tp.name.clone(), t);
            }
        }
        if let Some((args, aspan)) = &own_args {
            let resolved: Vec<Ty> = args.iter().map(|t| self.resolve_type(t, *aspan)).collect();
            arity_ok &= resolved.len() == own.len();
            // `seed_targs` emits the clean "'name' expects N type argument(s), found M".
            map.extend(self.seed_targs(&pf.display, &own, &resolved, span));
        }
        if !arity_ok {
            return Ty::Unknown;
        }
        let tps: Vec<TyParam> = pf.head_decl.iter().chain(own.iter()).cloned().collect();
        self.enforce_bounds(&tps, &tps, &map, span);
        // A conditional method's receiver `where` bound (`where T: Add`), as its call form enforces.
        self.enforce_bounds(&pf.sig.where_bounds, &tps, &map, span);
        let mut sig = subst_sig(&pf.sig, &map);
        sig.type_params.retain(|tp| !map.contains_key(&tp.name));
        if sig.type_params.is_empty() {
            return fn_value_ty(&sig);
        }
        let spelling = if map.is_empty() {
            pf.spelling.clone()
        } else {
            fn_spelling(&pf.display, &sig.type_params)
        };
        self.generic_fn_value_ty(node, &pf.display, &sig, &spelling, span)
            .unwrap_or_else(|| fn_value_ty(&sig))
    }

    /// THE one checker resolution of a type-applied fn value `head[T…]`, either carrier
    /// (`ast::type_application`): `pair[str, int]`, `lib.idt[int]`, `Bx[int].put[str]`. `None` when
    /// the head is no fn-like path with type params of its own.
    pub(super) fn infer_type_applied_fn_value(&mut self, e: &Expr) -> Option<Ty> {
        let app = crate::ast::type_application(e)?;
        let head = app.head;
        let written = Some((app.args.clone(), app.args_span));
        match self.resolve_path(head, PathPos::Value) {
            // A fn-like path: its own type params take the written arguments. A fn head resolves to
            // `Resolution::Fn`, the one fact the compiler erases on (DEC-197).
            Some(
                r @ (Resolution::Fn { .. }
                | Resolution::MethodFn { .. }
                | Resolution::ParamMethodFn { .. }),
            ) => {
                let pf = self.path_fn(head)?;
                if pf.sig.type_params.len() == pf.head_params {
                    return None;
                }
                // The compiler loads a module fn's head as a value (`Compiler::resolution` (5)).
                if let Resolution::Fn { .. } = r
                    && let ExprKind::Field { obj: m, .. } = &head.kind
                {
                    self.infer(m);
                }
                Some(self.path_fn_value_ty(e.id, pf, written, head.span))
            }
            // TICKET-214: std.json's decode, which the table leaves unnamed — `T` is written, so
            // the value always records its descriptor (or reports a target that does not decode).
            None => {
                let ExprKind::Field { obj: m, name, .. } = &head.kind else {
                    return None;
                };
                let ExprKind::Ident(mn) = &m.kind else {
                    return None;
                };
                if !self.json_decode_member(mn, name) {
                    return None;
                }
                self.infer(m);
                let pf = self.path_fn(head)?;
                let ty = self.path_fn_value_ty(e.id, pf, written, head.span);
                Some(self.record_decode_value(head.id, ty, head.span))
            }
            _ => None,
        }
    }

    /// The members a type path names but never yields, reported at `name_span`: a protocol
    /// method and a native handle's method. Read by the value read ([`Self::type_member_value`])
    /// and the call read (`infer_static_call`), so both refuse the same paths. `true` when it
    /// reported.
    pub(super) fn type_member_refusal(
        &mut self,
        th: &TypeHead,
        name: &str,
        name_span: Span,
    ) -> bool {
        let spelled = th.spelled.clone();
        if th.kind == TypeHeadKind::Protocol {
            let has = self
                .protocols
                .get(&th.key)
                .is_some_and(|p| p.methods.iter().any(|(m, _)| m == name));
            if !has {
                return false;
            }
            self.error(
                name_span,
                format!(
                    "'{name}' is a method of protocol '{spelled}' -- a protocol method is not a \
                      value: name it through a concrete type (`<Type>.{name}`, which takes the \
                      receiver first) or wrap it (`fn(x): x.{name}()`)"
                ),
            );
            return true;
        }
        if th.native_handle {
            if !self
                .structs
                .get(&th.key)
                .is_some_and(|info| info.methods.contains_key(name))
            {
                return false;
            }
            self.error(
                name_span,
                format!(
                    "'{name}' is a method of the native type '{spelled}' -- \
                     a native method is not a value: call it on a value (`x.{name}(…)`) or wrap \
                     it in a closure (`fn(x): x.{name}()`)"
                ),
            );
            return true;
        }
        false
    }

    /// A type path `Head.name` / `Head[T…].name` read as a value (TICKET-204): a nullary variant, a
    /// fn-like member through [`Self::type_member_fn`], or one of the refusals (a protocol method,
    /// a native method, a missing variant). `None` when `obj` is no type head or `name` is no member
    /// this decides; the caller falls through to the ordinary field path.
    pub(super) fn type_member_value(
        &mut self,
        obj: &Expr,
        name: &str,
        name_span: Span,
        res: Option<&Resolution>,
    ) -> Option<Ty> {
        let (th, head_args) = self.peel_type_path(obj)?;
        let spelled = th.spelled.clone();
        if self.type_member_refusal(&th, name, name_span) {
            return Some(Ty::Unknown);
        }
        let key = th.key.clone();
        match res {
            Some(Resolution::Variant { .. }) => {
                // A nullary variant is a value of the enum: explicit head args resolve and
                // arity-check, an alias head pins its own, a bare head leaves them Unknown.
                let tps = self.enum_type_params.get(&key).cloned().unwrap_or_default();
                let written: Vec<Ty> = match &head_args {
                    Some((targs, _)) => targs
                        .iter()
                        .map(|t| self.resolve_type(t, obj.span))
                        .collect(),
                    None => Vec::new(),
                };
                let Some(head) = self.written_head_args(
                    &spelled,
                    self.type_param_count(&key),
                    th.pinned.clone(),
                    written,
                    obj.span,
                ) else {
                    return Some(Ty::Unknown);
                };
                let args = if head_args.is_some() {
                    self.seed_targs(&spelled, &tps, &head, obj.span);
                    head
                } else if head.len() == tps.len() {
                    head
                } else {
                    vec![Ty::Unknown; tps.len()]
                };
                Some(Ty::enum_ty(key, args))
            }
            Some(Resolution::MethodFn { .. }) => {
                let pf = self.type_member_fn(&th, head_args, name)?;
                Some(self.path_fn_value_ty(obj.id, pf, None, name_span))
            }
            // U10: a payload variant is a constructor, never a value.
            Some(Resolution::VariantFn { .. }) => {
                self.type_not_value(&TypeRead::Variant(format!("{spelled}.{name}")), name_span);
                Some(Ty::Unknown)
            }
            // A miss on an enum: no such variant. A struct's miss reads `obj` as a value below,
            // which reports the type.
            None if th.kind == TypeHeadKind::Enum && !th.native_handle => {
                // A declared enum is named bare, as the call path names it; an alias as written.
                let ename = if th.pinned.is_some() {
                    &spelled
                } else {
                    &th.name
                };
                let names = self.variant_names(&key);
                self.error_help(
                    name_span,
                    format!("enum '{ename}' has no variant '{name}'"),
                    suggest::did_you_mean(name, &names),
                );
                Some(Ty::Unknown)
            }
            _ => None,
        }
    }

    /// The one message for a type name read as a value, bare, imported, qualified or type-applied
    /// (TICKET-204, U10). It decides nothing about the name; it formats from `read`.
    pub(super) fn type_not_value(&mut self, read: &TypeRead, span: Span) {
        let ctor = |s: &str| {
            format!(
                "'{s}' is a type, not a value — constructors are not values: call it \
                  (`{s}(…)`) or wrap it in a closure (`fn(e): {s}(e)`)"
            )
        };
        let msg = match read {
            TypeRead::Head(th) if th.native_handle => {
                format!("'{}' is a type, not a value", th.spelled)
            }
            TypeRead::Head(th) => {
                let spelled = &th.spelled;
                match th.kind {
                    TypeHeadKind::Struct => ctor(spelled),
                    TypeHeadKind::Enum => format!(
                        "'{spelled}' is a type, not a value — use one of its variants \
                          (`{spelled}.<Variant>`)"
                    ),
                    TypeHeadKind::Protocol => {
                        format!(
                            "'{spelled}' is a type, not a value — a protocol has no constructor"
                        )
                    }
                }
            }
            TypeRead::Variant(s) => ctor(s),
            TypeRead::Builtin(n)
                if self.builtin_callee(n).is_some()
                    || n.rsplit_once('.').is_some_and(|(_, m)| {
                        BuiltinType::from_name(m).is_some_and(BuiltinType::has_opcode_ctor)
                    }) =>
            {
                ctor(n)
            }
            TypeRead::Alias(s) | TypeRead::Builtin(s) => {
                format!("'{s}' is a type, not a value")
            }
            TypeRead::SelfType => "'Self' is a type, not a value".to_string(),
        };
        self.error(span, msg);
    }

    /// THE ONE diagnostic for "this read of generic fn `name` cannot become a function value here",
    /// shared by both positions that can reach the verdict: the immediate read (`infer_ident`, whose
    /// expected-type hint either determines the params or does not) and the DEFERRED end-of-call
    /// check on a generic method's argument ([`Checker::close_tyvar_frame`]).
    /// One rule, one sentence — the whole point of the extension is that a binding and an argument
    /// stop giving one function two verdicts.
    pub(super) fn reject_undetermined_generic_fn_value(
        &mut self,
        name: &str,
        decl: &FnSig,
        spelling: &str,
        span: Span,
    ) {
        let type_params = &decl.type_params;
        // Render the wanted shape from the fn's OWN signature with each undetermined parameter shown
        // as a `<T>` placeholder, so the advice fits this declaration instead of a made-up one.
        // `fn_value_ty` so a fn with DEFAULTED params does not advertise a stricter arity than a
        // plain fn read gives (`fn rep[T](x: T, n: int = 2)` — a non-generic `g := rep; g(1)` works,
        // so the suggested type must permit it too).
        let holes: HashMap<String, Ty> = type_params
            .iter()
            .map(|tp| (tp.name.clone(), Ty::Param(format!("<{}>", tp.name))))
            .collect();
        let sig = subst(&fn_value_ty(decl), &holes);
        let names = type_params
            .iter()
            .map(|tp| tp.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let sig = sig.to_string();
        // TICKET-204: a turbofish takes every parameter at once (`pair[<A>, <B>]`), on any fn-like
        // path (`R1[<T>].L`, `Bx[<T>].put[<U>]`); `spelling` is the hint `path_fn` built.
        let turbofish = format!("instantiate it (`{spelling}`), or ");
        // Which parameters actually OCCUR in the signature? Only those can be reached by giving the
        // position a concrete function type. One that appears NOWHERE (`pred[T](n: int) -> bool`,
        // reachable from a HOF slot) renders no `<…>` hole at all, so the "write a real type in place
        // of each `<…>`" tail would point at nothing and the suggested `fn(int) -> bool` is the type
        // the position ALREADY has — advice the user has, by construction, already followed. Testing
        // the RENDERED text is the exact question: does the advice contain a hole for this parameter?
        let absent = type_params
            .iter()
            .map(|tp| tp.name.as_str())
            .filter(|n| !sig.contains(&format!("<{n}>")))
            .collect::<Vec<_>>();
        // WORDING IS LOAD-BEARING. Say what is true of THIS read — the parameters are not determined
        // HERE — never "nothing determines them", and never "annotate the binding". The rule also
        // fires in positions that DO carry a concrete type Chezzi simply does not thread into
        // `expected_hint` (a parameter/field DEFAULT value: `fn run(f: fn(int) -> int = id)`, whose
        // slot pins T=int by inspection), and in positions with no binding to annotate at all
        // (`print(id)`, a `yield`, a list/map element, a HOF argument). Both earlier phrasings were
        // then FACTUALLY FALSE, and the second told a user who had already written
        // `xs: List[fn(int) -> int] = [id]` to do the thing they had done. Naming the POSITION keeps
        // the sentence true everywhere and still points at the fix.
        let advice = if absent.is_empty() {
            format!(
                "{turbofish}give this position a concrete function type (`{sig}`), writing a real \
                 type in place of each `<…>`"
            )
        } else {
            // The genuinely-unused parameter. A type at this position can never reach it, so the only
            // honest remedies are the turbofish and deleting it.
            let plural = if absent.len() == 1 { "s" } else { "" };
            format!(
                "{} appear{plural} nowhere in its signature (`{sig}`), so no type at this position \
                 can reach {}: {turbofish}drop the unused type parameter{}",
                absent.join(", "),
                if absent.len() == 1 { "it" } else { "them" },
                if absent.len() == 1 { "" } else { "s" },
            )
        };
        self.error(
            span,
            format!(
                "'{name}' is generic and {names} {} not determined here, so it cannot become \
                 a function value — {advice}",
                if type_params.len() == 1 { "is" } else { "are" }
            ),
        );
    }
}

/// Written type arguments and the span their diagnostics anchor on.
pub(super) type WrittenTypeArgs = (Vec<Type>, Span);

/// One fn-like path read as a value (TICKET-204): a fn, `m.f`, or a type member (a static method,
/// an instance method through its type, a payload variant). Built by [`Checker::path_fn`].
pub(super) struct PathFn {
    /// The path as written, head args included (`pair`, `lib.pair`, `Bx[int].put`).
    pub(super) display: String,
    /// The type's spelling (`Bx`, `vlib.Bx`) for head-arg diagnostics; empty for a fn.
    pub(super) head_spelled: String,
    /// The value's signature; `type_params` holds the kept head params (the first `head_params`)
    /// then the item's own.
    pub(super) sig: FnSig,
    /// The type's declared params, against which head args are arity-checked and mapped.
    pub(super) head_decl: Vec<TyParam>,
    pub(super) head_params: usize,
    /// The written head args (`Bx[int].make`).
    pub(super) head_args: Option<WrittenTypeArgs>,
    /// An alias head's pinned args (`B.make`); at most one of this and `head_args` is `Some`.
    pub(super) head_pinned: Option<Vec<Ty>>,
    /// `None` for a fn (it records `Resolution::Fn`), else `MethodFn` / `VariantFn`.
    pub(super) res: Option<Resolution>,
    /// The instantiation hint (`pair[<A>, <B>]`, `R1[<T>].L`, `Bx[<T>].put[<U>]`).
    pub(super) spelling: String,
}

impl PathFn {
    fn of_fn(display: String, sig: FnSig) -> PathFn {
        PathFn {
            spelling: fn_spelling(&display, &sig.type_params),
            display,
            head_spelled: String::new(),
            sig,
            head_decl: Vec::new(),
            head_params: 0,
            head_args: None,
            head_pinned: None,
            res: None,
        }
    }
}

/// `display[<P>, …]` over `tps`, or `display` alone when there are none.
pub(super) fn fn_spelling(display: &str, tps: &[TyParam]) -> String {
    if tps.is_empty() {
        return display.to_string();
    }
    let holes = tps
        .iter()
        .map(|tp| format!("<{}>", tp.name))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{display}[{holes}]")
}

/// Does a head with alias-pinned args `pinned` over a type declaring `decl_params` params clash
/// with written type args (DEC-204: an alias that fixes its arguments takes no more)? A pinning
/// alias (`type BI = Bx[int]`) and a non-generic one (`type P2 = P`) clash; an unpinned alias of a
/// generic type (`type BB = Box`, TICKET-180) takes the target's arguments and does not. The one
/// predicate under [`Checker::written_head_args`].
pub(super) fn head_args_clash(pinned: Option<&[Ty]>, decl_params: usize, written: bool) -> bool {
    written && pinned.is_some_and(|p| !p.is_empty() || decl_params == 0)
}

/// A method sig as a value over its type's params `tps` (DEC-197: through `instantiate_method`):
/// the type's params lead `type_params`, and an instance method's receiver slot becomes `recv`,
/// keyword `self`.
fn method_value_sig(msig: &FnSig, tps: &[TyParam], recv: Ty) -> FnSig {
    let recv_map: HashMap<String, Ty> = tps
        .iter()
        .map(|tp| (tp.name.clone(), Ty::Param(tp.name.clone())))
        .collect();
    let mut sig = instantiate_method(msig, &recv_map);
    if !msig.is_static {
        if let Some(p) = sig.params.first_mut() {
            *p = recv;
        }
        if let Some(l) = sig.labels.first_mut() {
            *l = Some(crate::ast::SELF.to_string());
        }
        if let Some(slots) = &mut sig.slots {
            slots.insert(
                0,
                crate::desugar::SlotSpec {
                    name: Some(crate::ast::SELF.to_string()),
                    default: None,
                    is_variadic: false,
                },
            );
        }
    }
    sig.type_params = tps.iter().cloned().chain(sig.type_params).collect();
    sig
}
