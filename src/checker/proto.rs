// checker::proto — split out of checker/mod.rs. `super::*` == the `checker` module.
// Protocol hoisting/embedding, satisfies, receiver refinement, hashability.

use super::*;

impl Checker {
    /// Register a `protocol` declaration's method signatures. `Self` resolves to `Ty::Param("Self")`.
    pub(super) fn hoist_protocol(
        &mut self,
        name: &str,
        type_params: &[TypeParam],
        methods: &[MethodSig],
        embeds: &[Bound],
        span: Span,
    ) {
        if is_reserved_protocol(name) {
            // Phase 5c-protocols — a reserved protocol's SHAPE is now ALSO declared in `std/prelude.chz`
            // as a plain `protocol` decl (a drift-guarded ADDITIVE mirror; see
            // `assert_native_protocol_shape_matches`). In a stdlib module that decl is VALIDATE-AND-NO-OP:
            // do NOT insert (the live source stays `prebuilt_protocols`, seeded at `Checker::new`) and do
            // NOT error — mirroring the `native struct`/`native enum` stdlib arms. In a USER module the
            // reserved name stays rejected (a user can't redeclare a builtin protocol).
            if !self.current_module_is_stdlib {
                self.error(span, format!("protocol '{name}' is reserved (builtin)"));
            }
            return;
        }
        // A protocol may not shadow a reserved builtin TYPE name either (`protocol List` / `protocol
        // int`). Sibling struct/enum/newtype/type-alias decl guards all reject `is_reserved_type`;
        // protocol was the sole decl path that omitted it. Ordered AFTER the reserved-protocol arm so
        // `Iterator` (both a reserved protocol AND type) is caught once, above, keeping its protocol
        // wording. The stdlib carve-out mirrors the reserved-protocol arm (no native protocol is named
        // after a reserved type, so it never fires there, but preserves symmetry).
        if is_reserved_type(name) {
            if !self.current_module_is_stdlib {
                self.error(span, format!("type '{name}' is reserved (builtin)"));
            }
            return;
        }
        if self.protocols.contains_key(name) {
            self.error(span, format!("protocol '{name}' is already defined"));
        }
        // A protocol's own type param may not be named after a reserved builtin type (`protocol
        // P[int]`) — same rule as struct/enum/newtype/fn params.
        self.reject_reserved_type_params(type_params);
        let mut saved = self.type_params.clone();
        std::mem::swap(&mut self.type_params, &mut saved); // start clean, with only Self visible
        self.type_params.insert("Self".to_string(), Vec::new());
        // The protocol's own type params are in scope while resolving its method signatures, so
        // `fn get(self, i: int) -> T` resolves `T` to `Ty::Param("T")`.
        for tp in type_params {
            if tp.name == "Self" {
                self.error(
                    span,
                    "protocol type parameter cannot be named 'Self'".to_string(),
                );
            }
            self.type_params.insert(tp.name.clone(), tp.bounds.clone());
        }
        for tp in type_params {
            self.check_bounds(&tp.bounds, &tp.name, span);
        }
        let sigs = methods
            .iter()
            .map(|m| {
                let params = m
                    .params
                    .iter()
                    .map(|p| match &p.ty {
                        Some(t) => self.resolve_type(t, span),
                        None if p.name == "self" => Ty::Unknown,
                        None => {
                            self.error(
                                span,
                                format!("protocol method parameter '{}' needs a type", p.name),
                            );
                            Ty::Unknown
                        }
                    })
                    .collect();
                let ret = m
                    .ret
                    .as_ref()
                    .map(|t| self.resolve_type(t, span))
                    .unwrap_or(Ty::Nil);
                // A protocol method whose first param is NOT `self` (or which has no params) is a STATIC
                // (associated) requirement — mirrors `Checker::fn_sig`'s rule for concrete methods. This
                // is load-bearing for `Convert`-style static-ctor protocols: it drives both the
                // static-slot witnessing check (`method_matches`) and the bound-only value-position gate
                // (`protocol_has_static_method`). Ordinary `fn get(self, …)` requirements are instance
                // methods (`is_static == false`), unchanged.
                let mut sig = FnSig::plain(params, ret);
                sig.is_static = m.params.first().is_none_or(|p| p.name != "self");
                (m.name.clone(), sig)
            })
            .collect();
        self.type_params = saved; // restore
        self.protocols.insert(
            name.to_string(),
            ProtocolInfo {
                type_params: type_params.iter().map(|tp| tp.name.clone()).collect(),
                methods: sigs,
                embeds: embeds.to_vec(),
            },
        );
    }

    /// M22 — validate a protocol's embeds AFTER every protocol is hoisted (so forward/cyclic refs
    /// resolve). Three LOCKED rules: (1) an OWN `fn` whose name collides with any transitively-embedded
    /// required method → error, regardless of order or sig; (2) two embeds pulling the same method name
    /// with DIFFERING signatures → error (identical sigs dedup silently — the legal diamond); (3) a
    /// cyclic embed (A embeds B, B embeds A) → error. Built-in protocols (registered via
    /// [`prebuilt_protocols`], not here) are trusted and skipped.
    pub(super) fn validate_protocol_embeds(
        &mut self,
        name: &str,
        methods: &[MethodSig],
        embeds: &[Bound],
        span: Span,
    ) {
        if is_reserved_protocol(name) {
            return; // builtin (or already errored as a redeclaration / reserved name)
        }
        // Each embed must name a real protocol.
        for emb in embeds {
            if !self.protocols.contains_key(&emb.name) {
                self.error(
                    span,
                    format!("unknown protocol '{}' embedded in '{name}'", emb.name),
                );
            }
        }
        // Flatten the transitive embed method set, detecting cycles + cross-embed signature conflicts.
        let mut path = vec![name.to_string()];
        let (required, cyclic, conflict) = self.flatten_embed_methods(embeds, &mut path);
        if cyclic {
            self.error(
                span,
                format!("cyclic protocol embedding involving '{name}'"),
            );
            return;
        }
        if let Some(m) = conflict {
            self.error(
                span,
                format!("conflicting signature for method '{m}' from embedded protocols"),
            );
        }
        // Rule 1: an own `fn` colliding with an embedded-required method name.
        for m in methods {
            if required.contains_key(&m.name) {
                self.error(
                    span,
                    format!(
                        "method '{}' conflicts with embedded protocol requirement",
                        m.name
                    ),
                );
            }
        }
    }

    /// Recursively collect the method signatures required by a list of embeds. Returns
    /// `(required, cyclic, conflict)`: `required` maps method name → signature (identical sigs deduped),
    /// `cyclic` is set if any embed revisits a protocol on the current `path`, and `conflict` names a
    /// method seen with two DIFFERING signatures (rule 2). Read-only over `self.protocols`.
    pub(super) fn flatten_embed_methods(
        &self,
        embeds: &[Bound],
        path: &mut Vec<String>,
    ) -> (HashMap<String, FnSig>, bool, Option<String>) {
        let mut required: HashMap<String, FnSig> = HashMap::new();
        let mut conflict: Option<String> = None;
        let mut merge = |mn: &str, ms: &FnSig, conflict: &mut Option<String>| {
            match required.get(mn) {
                Some(existing) if !fn_sig_eq(existing, ms) => {
                    if conflict.is_none() {
                        *conflict = Some(mn.to_string());
                    }
                }
                Some(_) => {} // identical sig — dedup silently (legal diamond)
                None => {
                    required.insert(mn.to_string(), ms.clone());
                }
            }
        };
        for emb in embeds {
            if path.iter().any(|p| p == &emb.name) {
                return (required, true, conflict);
            }
            let Some(pinfo) = self.protocols.get(&emb.name).cloned() else {
                continue; // unknown embed already errored in validate_protocol_embeds
            };
            for (mn, ms) in &pinfo.methods {
                merge(mn, ms, &mut conflict);
            }
            path.push(emb.name.clone());
            let (sub, cyclic, sub_conf) = self.flatten_embed_methods(&pinfo.embeds, path);
            path.pop();
            if cyclic {
                return (required, true, conflict);
            }
            for (mn, ms) in &sub {
                merge(mn, ms, &mut conflict);
            }
            if conflict.is_none() {
                conflict = sub_conf;
            }
        }
        (required, false, conflict)
    }

    /// Does concrete `ty` structurally satisfy `protocol`? Read-only. Primitives intrinsically
    /// satisfy `Comparable`; structs satisfy any protocol whose methods they all implement.
    /// Valid `map` key / `set` element types: anything that satisfies the `Hashable` protocol —
    /// the scalars `int`/`str`/`bool` intrinsically, or a struct defining `hash(self) -> int`.
    /// `float` is rejected (NaN/equality footgun); `Unknown` is tolerated (no cascade). With this,
    /// user structs can be map keys / set elements, hashed via their `hash()` at runtime.
    pub(super) fn is_hashable_key(&self, t: &Ty) -> bool {
        self.satisfies(t, "Hashable").is_ok()
    }

    /// Refine-on-first-use (empty-slot half of the `Ty::Unknown` soundness family). A bare empty
    /// collection literal (`[]`/`{}`/`set()`), a nullary user-enum variant (`Box.Empty`), or the
    /// native nullary `None` types its element/key/value/type-arg slot as `Ty::Unknown`, which is
    /// permissive in both directions — so junk would flow into a check-blessed program and fault at
    /// runtime, and the float-key/Hashable ban would be bypassed. This hook fires at the top of
    /// `infer_method_call`, when a mutating method (`push`/`add`/`insert`/`extend`) on a
    /// **simple-variable** receiver supplies a CONCRETE type at an `Unknown` slot: it structurally
    /// merges the supplied shape into the binding, re-pins it in its owning scope, and runs the
    /// Hashable check on a newly-concrete set element. A later op supplying an incompatible concrete
    /// type then fails as a normal `check_args` mismatch against the now-pinned element — and the
    /// mismatch diagnostic is enriched (in `check_args`) to hint at annotating for a mixed/protocol
    /// collection.
    ///
    /// RESIDUAL HOLE (documented, not fixed here): refine only fires when the receiver is a simple
    /// `Ident` in scope. `obj.field.push(...)` / `f().push(...)` / `xss[0].push(...)` (non-Ident
    /// receivers) stay unrefined — struct fields are explicitly typed anyway, so the impact is low.
    pub(super) fn refine_receiver(&mut self, obj: &Expr, obj_ty: &Ty, method: &str, args: &[Expr]) {
        // (a) simple-variable receiver only (the documented limitation).
        let ExprKind::Ident(name) = &obj.kind else {
            return;
        };
        // Must be a real in-scope binding (not a function/global-type name).
        if self.lookup(name).is_none() {
            return;
        }
        // PART A: a slot-supplying mutator (`push`/`add`/`insert`/`extend` with an arg) constrains
        // this binding's element type, so clear any pending empty-collection annotation requirement.
        // Done BEFORE the `is_captured` early-return below so a `spawn:`/`Executor.submit` body that
        // supplies the element only via a mutator (`acc := []` outside, `acc.push(1)` captured) still
        // drops the site — the element WAS supplied, so requiring an annotation would be wrong. A
        // no-op when no site exists (`drop_empty_site` only removes a matching `(owner, name)`).
        if matches!(method, "push" | "add" | "insert" | "extend") && !args.is_empty() {
            self.drop_empty_site(name);
        }
        // Skip captured bindings: mirror the airlock reassignment ban — refine is a checker-side
        // narrowing, but skipping it here keeps behavior aligned and avoids a confusing diagnostic.
        if self.is_captured(name) {
            return;
        }
        // (b) the binding must have an Unknown in a SLOT position (not a bare top-level Unknown —
        // that's the cascade-suppression sentinel and must stay permissive).
        if !contains_unknown_in_slot(obj_ty) {
            return;
        }
        // (c) determine the supplied ELEMENT type from a slot-supplying mutator's args.
        // `push(x)`/`add(x)`/`insert(x)` supply the element directly; `extend(xs)` supplies a
        // list/set whose element refines ours.
        let mark = self.errors.len();
        let elem = match method {
            "push" | "add" | "insert" => args.first().map(|a| self.infer_value(a)),
            "extend" => args.first().map(|a| match self.infer_value(a) {
                Ty::List(e) | Ty::Set(e) => *e,
                other => other,
            }),
            _ => return,
        };
        let Some(elem) = elem else { return };
        // Wrap the element into a RECEIVER-SHAPED value so the structural merge lines up the slot:
        // a list receiver merges with `list[elem]`, a set receiver with `set[elem]`. Any other
        // receiver kind isn't a push/add/extend target, so nothing to refine.
        let shape = match obj_ty {
            Ty::List(_) => Ty::list(elem),
            Ty::Set(_) => Ty::set(elem),
            _ => return,
        };
        // (d) cascade invariant: if inferring the arg itself reported an error, don't refine — and
        // roll back the speculative diagnostics so the real dispatch path (check_args) reports them
        // exactly once. Leaving them here double-reports an erroring arg (e.g. `xs.push(undefined)`).
        if self.errors.len() != mark {
            self.errors.truncate(mark);
            return;
        }
        // A shape that is itself Unknown supplies nothing concrete; merge is a no-op, bail early.
        if shape.is_unknown() {
            return;
        }
        let merged = merge_unknown(obj_ty, &shape);
        if merged == *obj_ty {
            return; // nothing newly concrete
        }
        // Run the Hashable / float-key ban at the moment a SET element becomes concrete (the sig
        // tables don't). Map keys are handled in the `m[k]=v` index-assign refine path.
        if let Ty::Set(e) = &merged
            && !e.is_unknown()
            && !self.is_hashable_key(e)
        {
            self.error(
                obj.span,
                format!("set element type must implement Hashable (int, str, bool, or a struct/enum/newtype defining hash(self) -> int), found {e}"),
            );
        }
        self.repin(name, merged);
    }

    /// Refine-on-first-use for an index-assign `m[k]=v` / `xs[i]=v` (the assignment-statement
    /// sibling of [`Self::refine_receiver`]). When the receiver is a simple variable whose type has
    /// an `Unknown` key/value/element slot, merge the supplied (index type, value type) shape into
    /// the binding, re-pin it, and run the Hashable / float-key ban on a newly-concrete MAP key.
    /// `val_ty` is already inferred by the caller; we infer the index type here only when the
    /// receiver is actually refinable (so we don't double-report on the common already-typed path).
    pub(super) fn refine_index_receiver(&mut self, obj: &Expr, index: &Expr, val_ty: &Ty) {
        let ExprKind::Ident(name) = &obj.kind else {
            return;
        };
        let Some(obj_ty) = self.lookup(name) else {
            return;
        };
        if self.is_captured(name) || !contains_unknown_in_slot(&obj_ty) {
            return;
        }
        // PART A: an index-assign (`m[k]=v` / `xs[i]=v`) constrains this binding — clear any pending
        // empty-collection annotation requirement (BEFORE the speculative-index-infer truncate-return,
        // mirroring `refine_receiver`, so an erroring key like `m[undefined_k]=1` still drops the site).
        self.drop_empty_site(name);
        if val_ty.is_unknown() {
            return;
        }
        // The supplied shape mirrors the receiver kind: `Map(idx, val)` for a map, `List(val)` for a
        // list (index type is the int position, irrelevant to the element slot).
        let mark = self.errors.len();
        let shape = match &obj_ty {
            Ty::Map(..) => Ty::map(self.infer(index), val_ty.clone()),
            Ty::List(..) => Ty::list(val_ty.clone()),
            _ => return,
        };
        if self.errors.len() != mark {
            self.errors.truncate(mark); // roll back the speculative index-infer diagnostics; the
            return; // real index-assign path re-infers + reports them once (no double-report)
        }
        let merged = merge_unknown(&obj_ty, &shape);
        if merged == obj_ty {
            return;
        }
        // NOTE: the map-key Hashable / float-key ban is NOT run here — it is the direct
        // insertion-site check in `check_assign`'s Index branch (so it fires even while the key type
        // is still `Unknown`, e.g. `m:={}; m[1.5]=..`), keeping a single owner and no double-report.
        self.repin(name, merged);
    }

    /// Assignability with protocol-existential awareness. Like the free [`compatible`], but a
    /// concrete type is assignable to a `Protocol(P)` slot iff it satisfies `P` — which needs the
    /// protocol/struct registry, so it can't live in the context-free `compatible`. Recurses through
    /// compound types so a nested existential (the `E` in `Result[T, Error]`) is checked structurally.
    /// Strict assignability — NO int→float widening (the reverse `float`→`int` is always rejected).
    pub(super) fn assignable(&self, expected: &Ty, actual: &Ty) -> bool {
        use Ty::*;
        match (expected, actual) {
            (Unknown, _) | (_, Unknown) => true,
            // A protocol existential slot: the actual type must satisfy the protocol WITH the carried
            // args (empty for a bare existential — reproduces the old `satisfies(a, p)`). This single
            // witness is shared by every value write-site (param/return/field/reassign) since they all
            // route assignment through `assignable`.
            (Protocol(p, pargs), a) => self.satisfies_args(a, p, pargs).is_ok(),
            // `Option`/`Result` are IMMUTABLE carriers — covariant element assignment stays sound
            // (no write-through alias), so they keep recursing via `assignable`. `List`/`Set`/`Map`
            // and user generic `Struct`/`Enum` are MUTABLE, by-reference containers: covariant type
            // args are a soundness hole (a `G[Sub]` bound aliased as `G[Super]` can have a Super value
            // written back into it — see `invariance_rejects_*` tests). Their type ARGUMENTS are
            // therefore compared with the context-free structural-equality primitive `compatible`
            // (= strict INVARIANCE), mirroring the M14 `compatible` Protocol/Struct/Enum arms and
            // `bound_args_match`. Docs: spec.md "strictly invariant"; future.md "no covariance holes".
            (Option(e), Option(a)) => self.assignable(e, a),
            (List(e), List(a)) | (Set(e), Set(a)) => compatible(e, a),
            (Result(et, ee), Result(at, ae)) => self.assignable(et, at) && self.assignable(ee, ae),
            (Map(ek, ev), Map(ak, av)) => compatible(ek, ak) && compatible(ev, av),
            (Struct(n, ea), Struct(m, aa)) | (Enum(n, ea), Enum(m, aa)) => {
                n == m && ea.len() == aa.len() && ea.iter().zip(aa).all(|(x, y)| compatible(x, y))
            }
            (Tuple(e), Tuple(a)) => {
                e.len() == a.len() && e.iter().zip(a).all(|(x, y)| self.assignable(x, y))
            }
            // Labels are surface-only: assignability matches on arity + param/ret only (`..`).
            (
                Func {
                    params: p1,
                    ret: r1,
                    ..
                },
                Func {
                    params: p2,
                    ret: r2,
                    ..
                },
            ) => {
                p1.len() == p2.len()
                    && p1.iter().zip(p2).all(|(a, b)| self.assignable(a, b))
                    && self.assignable(r1, r2)
            }
            _ => compatible(expected, actual),
        }
    }

    /// Like [`Checker::assignable`], but accepts **one-way int→float widening** (`(Float, Int)` only)
    /// at a SCALAR value-DEFINITION sink (typed `let`, function/struct/method arg, return,
    /// param/field default).
    ///
    /// `widen` is NOT "this is a float sink" — it is "this expression is an untyped int CONSTANT"
    /// ([`crate::ast::untyped_int_const`]), which is what every caller must pass. Go's rule: an
    /// untyped constant adapts to a float context; a TYPED int value never implicitly converts (the
    /// user writes `float(x)`). That distinction is what the old blanket `widen=true` lacked — it
    /// accepted `i := 1; x: float = i`, which the type-blind compiler happily lowered as an `Int`
    /// sitting in a static `float` slot (int overflow under a float type, an unsorted `List[float]`,
    /// an `f64` load over an int payload once a JIT exists).
    ///
    /// Widening is still NOT propagated into ANY compound position (list/set/option element,
    /// map/result value, struct/tuple/func) — only a scalar `float` sink emits `Op::CoerceFloat`.
    /// Collection floats come instead from mixed-literal element inference (`[1, 2.3]` infers
    /// `list[float]`), whose own widen gate (`Checker::elem_widen_ok`) fires only where the compiler
    /// is guaranteed to coerce. `widen=false` ⇒ identical to [`Checker::assignable`].
    pub(super) fn assignable_w(&self, expected: &Ty, actual: &Ty, widen: bool) -> bool {
        if widen && matches!((expected, actual), (Ty::Float, Ty::Int)) {
            return true;
        }
        self.assignable(expected, actual)
    }

    pub(super) fn satisfies(&self, ty: &Ty, protocol: &str) -> Result<(), String> {
        self.satisfies_args(ty, protocol, &[])
    }

    /// Do a declared bound's type args (AST `Type`s) match the `required` ones (resolved `Ty`s) for a
    /// forwarded parameterized bound? Read-only — used inside `satisfies_args`. Conservative: only a
    /// *fully concrete* mismatch is rejected (so a still-generic arg like a sibling type param keeps
    /// forwarding loosely, as before), which is what closes the `Container[str]`→`Container[int]` hole
    /// without breaking valid `[S: Iterator[T], T]` forwards.
    pub(super) fn bound_args_match(&self, bound_args: &[Type], required: &[Ty]) -> bool {
        if bound_args.len() != required.len() {
            return false;
        }
        bound_args.iter().zip(required).all(|(ba, want)| {
            let bt = self.resolve_ty_ro(ba);
            !ty_fully_concrete(&bt) || !ty_fully_concrete(want) || compatible(&bt, want)
        })
    }

    /// M22 — does a declared bound `bound_name[bound_args]` PROVIDE `protocol[required]`, directly or
    /// transitively through embedded (super-)protocols? This is what makes `a + b` / `a / b` legal
    /// inside a `[T: Arithmetic]` body (the bound `Arithmetic` flattens to Add/Sub/Mul/Div) and lets a
    /// `[T: Arithmetic]` value forward into a `[U: Div]` call. Preserves the `Iterator`→`Iterable`
    /// subsumption. Depth-capped against a (declare-time-rejected, but still-checked) cyclic embed.
    pub(super) fn bound_provides(
        &self,
        bound_name: &str,
        bound_args: &[Type],
        protocol: &str,
        required: &[Ty],
        depth: usize,
    ) -> bool {
        if depth > 64 {
            return false;
        }
        // Direct: the bound names the required protocol with matching args.
        if bound_name == protocol && self.bound_args_match(bound_args, required) {
            return true;
        }
        // Subsumption: every `Iterator[T]` IS `Iterable[T]` (its `iter()` returns self).
        if protocol == "Iterable"
            && bound_name == "Iterator"
            && self.bound_args_match(bound_args, required)
        {
            return true;
        }
        // Transitive: any embed of the bound's protocol provides it.
        if let Some(pinfo) = self.protocols.get(bound_name) {
            return pinfo
                .embeds
                .iter()
                .any(|e| self.bound_provides(&e.name, &e.args, protocol, required, depth + 1));
        }
        false
    }

    /// Read-only type resolution (no error emission), for contexts that only hold `&self`. Returns
    /// `Ty::Unknown` for anything it can't resolve, which callers treat permissively.
    pub(super) fn resolve_ty_ro(&self, t: &Type) -> Ty {
        self.resolve_ty_ro_d(t, 0)
    }

    /// Depth-bounded read-only type resolution. `depth` guards against a recursive alias body
    /// (`type A = B; type B = A`): without `alias_resolving` (this is `&self`), a hard cap of 64
    /// expansions returns `Ty::Unknown` instead of overflowing the stack. 64 is far beyond any real
    /// alias chain.
    pub(super) fn resolve_ty_ro_d(&self, t: &Type, depth: usize) -> Ty {
        if depth > 64 {
            return Ty::Unknown;
        }
        match t {
            Type::Named { name: n, .. } => match n.as_str() {
                "int" => Ty::Int,
                "float" => Ty::Float,
                "bool" => Ty::Bool,
                "str" => Ty::Str,
                "bytes" => Ty::Bytes,
                "bytearray" => Ty::ByteArray,
                "nil" => Ty::Nil,
                "Executor" => Ty::Executor,
                "Socket" => Ty::Socket,
                "Listener" => Ty::Listener,
                "Writer" => Ty::Writer,
                "Reader" => Ty::Reader,
                "ptr" => Ty::Ptr,
                "owned_str" => Ty::Str,
                _ if self.type_params.contains_key(n) => Ty::Param(n.clone()),
                // A fixed-width FFI integer name resolves to plain `int` (the width is a marshalling
                // detail). Needed so an exported alias body `type Len = int32` captures `Ty::Int`.
                _ if crate::native::ffi::TYPE_NAMES.contains(&n.as_str()) => Ty::Int,
                // A bare alias name resolves to its (recursively-resolved) body.
                _ if self.aliases.contains_key(n) => {
                    let body = self.aliases[n].clone();
                    self.resolve_ty_ro_d(&body, depth + 1)
                }
                _ if self.imported_alias_tys.contains_key(n) => self.imported_alias_tys[n].clone(),
                _ if self.struct_names.contains(n) => Ty::strukt(self.bare_key(n)),
                _ if self.enum_names.contains(n) => Ty::Enum(self.bare_key(n), Vec::new()),
                _ if self.newtype_names.contains(n) => Ty::NewType(self.bare_key(n), Vec::new()),
                _ if self.protocols.contains_key(n) => Ty::Protocol(n.clone(), Vec::new()),
                _ => Ty::Unknown,
            },
            Type::Generic(n, args, ..) => match (n.as_str(), args.as_slice()) {
                ("List", [x]) => Ty::list(self.resolve_ty_ro_d(x, depth + 1)),
                ("Set", [x]) => Ty::set(self.resolve_ty_ro_d(x, depth + 1)),
                ("Option", [x]) => Ty::option(self.resolve_ty_ro_d(x, depth + 1)),
                ("Channel", [x]) => Ty::channel(self.resolve_ty_ro_d(x, depth + 1)),
                ("Shared", [x]) => Ty::shared(self.resolve_ty_ro_d(x, depth + 1)),
                ("RwShared", [x]) => Ty::rwshared(self.resolve_ty_ro_d(x, depth + 1)),
                ("Atomic", [x]) => Ty::atomic(self.resolve_ty_ro_d(x, depth + 1)),
                ("Result", [x]) => Ty::result(self.resolve_ty_ro_d(x, depth + 1)),
                ("Result", [x, e]) => Ty::result_e(
                    self.resolve_ty_ro_d(x, depth + 1),
                    self.resolve_ty_ro_d(e, depth + 1),
                ),
                ("Map", [k, v]) => Ty::map(
                    self.resolve_ty_ro_d(k, depth + 1),
                    self.resolve_ty_ro_d(v, depth + 1),
                ),
                // `Iterator[T]` is an existential ITERATOR value, represented as `Ty::Struct("Iterator",
                // [T])` — NOT a protocol. Mirror the mutable `resolve_type` arm (sig.rs) so the two
                // resolvers agree on identical syntax; without this it would fall to the protocol arm
                // below and mint `Protocol("Iterator",[T])`, which downstream Iterator logic (keyed on
                // `Ty::Struct(name=="Iterator")`) would not recognize.
                ("Iterator", [elem]) => Ty::Struct(
                    "Iterator".to_string(),
                    vec![self.resolve_ty_ro_d(elem, depth + 1)],
                ),
                _ if self.struct_names.contains(n) => Ty::Struct(
                    self.bare_key(n),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                _ if self.enum_names.contains(n) => Ty::Enum(
                    self.bare_key(n),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                _ if self.newtype_names.contains(n) => Ty::NewType(
                    self.bare_key(n),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                // A parameterized protocol used as a value type (`Container[int]`). Mint the carried
                // args so the read-only resolver no longer silent-accepts it as `Unknown` (which would
                // erase the witness). Mirrors the mutable `resolve_type` protocol arm.
                _ if self.protocols.contains_key(n) => Ty::Protocol(
                    n.clone(),
                    args.iter()
                        .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                        .collect(),
                ),
                _ => Ty::Unknown,
            },
            Type::Func {
                params,
                ret,
                labels,
            } => Ty::Func {
                params: params
                    .iter()
                    .map(|p| self.resolve_ty_ro_d(p, depth + 1))
                    .collect(),
                ret: Box::new(self.resolve_ty_ro_d(ret, depth + 1)),
                labels: FnLabels(labels.clone()),
            },
            Type::Tuple(ts) => Ty::Tuple(
                ts.iter()
                    .map(|t| self.resolve_ty_ro_d(t, depth + 1))
                    .collect(),
            ),
            Type::Qualified { module, name, args } => {
                let resolved_args: Vec<Ty> = args
                    .iter()
                    .map(|a| self.resolve_ty_ro_d(a, depth + 1))
                    .collect();
                self.resolve_qualified_ro(module, name, &resolved_args)
            }
        }
    }

    /// Read-only resolution of a module-qualified type `module.name[args]` to a `Ty`. Looks the bound
    /// module up in `imported_modules`, finds the type in its `ModuleSig`, and returns the matching
    /// `Ty` (struct / enum / alias body). `Ty::Unknown` if anything is missing (callers permissive).
    pub(super) fn resolve_qualified_ro(&self, module: &str, name: &str, args: &[Ty]) -> Ty {
        let Some(mid) = self.imported_modules.get(module) else {
            return Ty::Unknown;
        };
        let Some(sig) = self.module_sigs.get(mid) else {
            return Ty::Unknown;
        };
        // A RESERVED native type (Shared/RwShared/Atomic/Executor/Socket/Listener) has a harvested
        // `sig.struct_defs` METHOD-table entry but is NOT nominal — skip it here so it resolves to the
        // reserved builtin `Ty` via the `sig.types` branch below (mirrors `resolve_type`'s guard).
        if sig.struct_defs.contains_key(name) && self.qualified_builtin_ty(name, &[]).is_none() {
            Ty::Struct(self.type_key(mid, name), args.to_vec())
        } else if sig.enum_defs.contains_key(name) {
            Ty::Enum(self.type_key(mid, name), args.to_vec())
        } else if sig.newtype_defs.contains_key(name) {
            Ty::NewType(self.type_key(mid, name), args.to_vec())
        } else if let Some(asig) = sig.type_aliases.get(name) {
            asig.body.clone()
        } else if sig.types.contains(name) {
            // Mirror `resolve_type`'s qualified builtin branch on the READ-ONLY export path (so an
            // EXPORTED `type S = concurrency.Shared[int]` / `newtype MyS[T] = concurrency.Shared[T]`
            // resolved via `resolve_ty_ro_d` carries the right builtin `Ty`). Permissive: no errors,
            // and a non-type `sig.types` name (`timer`) returns `Ty::Unknown`.
            self.qualified_builtin_ty(name, args).unwrap_or(Ty::Unknown)
        } else {
            Ty::Unknown
        }
    }

    /// Resolve a surface extern `Type` to its WIDTH-BEARING [`CType`] in the CURRENT module's
    /// import/alias scope — the SINGLE resolver both backends consume (the FFI collision-fix root).
    /// Mirrors `resolve_ty_ro_d`'s alias / `from`-import / `Qualified` walk EXACTLY, but stops at the
    /// width-bearing leaf instead of collapsing every FFI integer to `Ty::Int`. Crucially:
    ///   * a LOCAL alias (`self.aliases`) recurses on its body — resolving each hop in THIS module's
    ///     scope (a colliding same-named alias in another module can never be reached);
    ///   * a `from`-imported alias reads `self.imported_alias_ctypes` (the alias's CType computed in
    ///     its DEFINING module's scope) — closing the named-import-hop hole;
    ///   * a `module.Alias` reads the TARGET module's `AliasSig.ctype` (likewise defining-scope).
    ///
    /// `depth > 64` returns `None` (cycle guard, matching `resolve_ty_ro_d`). `None` means "not
    /// C-marshallable here" — the marshallability gate (`assert_marshallable`) is the actual error,
    /// this is only the width carrier.
    pub(super) fn resolve_ctype(&self, t: &Type) -> Option<CType> {
        self.resolve_ctype_d(t, 0)
    }

    pub(super) fn resolve_ctype_d(&self, t: &Type, depth: usize) -> Option<CType> {
        if depth > 64 {
            return None; // cyclic alias — defended (the marshal gate rejects it cleanly).
        }
        match t {
            Type::Named { name: n, .. } => match n.as_str() {
                "int" => Some(CType::Int),
                "float" => Some(CType::Float),
                "bool" => Some(CType::Bool),
                "str" => Some(CType::Str),
                "ptr" => Some(CType::Ptr),
                "owned_str" => Some(CType::OwnedStr),
                "int8" => Some(CType::Int8),
                "int16" => Some(CType::Int16),
                "int32" => Some(CType::Int32),
                "int64" => Some(CType::Int64),
                "uint8" => Some(CType::UInt8),
                "uint16" => Some(CType::UInt16),
                "uint32" => Some(CType::UInt32),
                "uint64" => Some(CType::UInt64),
                // A LOCAL transparent alias: recurse on its body in THIS module's scope.
                _ if self.aliases.contains_key(n) => {
                    let body = self.aliases[n].clone();
                    self.resolve_ctype_d(&body, depth + 1)
                }
                // A `from`-imported alias: its CType was computed in the DEFINING module's scope.
                _ if self.imported_alias_ctypes.contains_key(n) => {
                    self.imported_alias_ctypes[n].clone()
                }
                // A bare-visible struct (local or `from`-imported): a by-value flat-scalar struct.
                // SAME-MODULE path (current scope == defining scope): a cache hit (already populated)
                // OR a field-walk in THIS scope for a not-yet-populated forward-reference nested struct
                // — both correct here because this is the struct's own defining scope.
                _ if self.struct_names.contains(n) => {
                    let key = self.bare_key(n);
                    self.resolve_struct_ctype(&key)
                        .or_else(|| self.struct_ctype_from_asts(&key, depth))
                }
                _ => None,
            },
            // A sync scalar callback param (callbacks #4): `fn(scalars...) -> scalar` lowers to
            // `CType::Callback`. Every part must lower to a C SCALAR (`is_scalar`) — a non-scalar
            // part (str/struct/nested callback) yields `None`, so the marshal gate rejects it cleanly.
            // (Param-only; a function-typed RETURN is rejected by `assert_marshallable`, never lowered.)
            Type::Func { params, ret, .. } => {
                let mut cparams = Vec::with_capacity(params.len());
                for p in params {
                    let cp = self.resolve_ctype_d(p, depth + 1)?;
                    if !cp.is_scalar() {
                        return None;
                    }
                    cparams.push(cp);
                }
                let cret = self.resolve_ctype_d(ret, depth + 1)?;
                if !cret.is_scalar() {
                    return None;
                }
                Some(CType::Callback {
                    params: cparams,
                    ret: Box::new(cret),
                })
            }
            // RETURN-ONLY nullable `char*` (`str?` / `owned_str?`): the inner type decides
            // borrowed (`str` → OptStr) vs owned (`owned_str` → OptOwnedStr).
            Type::Generic(n, args, ..) if n == "Option" && args.len() == 1 => {
                match self.resolve_ctype_d(&args[0], depth + 1) {
                    Some(CType::Str) => Some(CType::OptStr),
                    Some(CType::OwnedStr) => Some(CType::OptOwnedStr),
                    _ => None,
                }
            }
            // A module-qualified type `mod.Name` — resolved in the TARGET module's scope, so a width
            // alias carries its DEFINING module's width and a struct its identity key (collision-proof
            // by construction: never the bare flat alias map).
            Type::Qualified { module, name, .. } => {
                let mid = self.imported_modules.get(module)?;
                let sig = self.module_sigs.get(mid)?;
                // A qualified FFI marshalling TYPE name (`ffi.int32` / `ffi.ptr`) in an extern
                // signature: map to its WIDTH-bearing CType, exactly like the bare `Type::Named`
                // arm above. The surface name is unchanged (the backends' `ctype_of` reads the
                // resulting CType, never re-derives from a module prefix), so it marshals identically
                // to the bare width. Gated on `sig.types` membership — only `std.ffi`'s sig carries
                // these reserved names — so this fires solely for genuine FFI types.
                if sig.types.contains(name) {
                    let c = match name.as_str() {
                        "ptr" => Some(CType::Ptr),
                        "int8" => Some(CType::Int8),
                        "int16" => Some(CType::Int16),
                        "int32" => Some(CType::Int32),
                        "int64" => Some(CType::Int64),
                        "uint8" => Some(CType::UInt8),
                        "uint16" => Some(CType::UInt16),
                        "uint32" => Some(CType::UInt32),
                        "uint64" => Some(CType::UInt64),
                        _ => None,
                    };
                    if c.is_some() {
                        return c;
                    }
                }
                if sig.struct_defs.contains_key(name) {
                    // CROSS-MODULE: read the qualified struct's CType from the cache VERBATIM (computed
                    // in ITS defining module's scope, deps-first). NEVER field-walk here — the current
                    // scope is the IMPORTER's, where the struct's field aliases are invisible/colliding.
                    self.resolve_struct_ctype(&self.type_key(mid, name))
                } else if let Some(asig) = sig.type_aliases.get(name) {
                    asig.ctype.clone()
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// PURE CACHE READ of the by-value `CType::Struct` for the struct under IDENTITY `key`. The CType
    /// was pre-computed in the struct's OWN DEFINING module's scope (by `populate_struct_ctypes`, run
    /// after `hoist` in each module's `check_module` — deps-first AND before this module's own extern
    /// harvest, so a cross-module OR same-module struct is always cached before an extern needs it).
    /// This is the SINGLE-RESOLVER invariant that kills the FFI drift: `resolve_struct_ctype` NEVER
    /// re-resolves a struct's fields in the (wrong, importing) current scope — it only reads the cache,
    /// so a field typed via the defining module's local alias keeps its true width.
    pub(super) fn resolve_struct_ctype(&self, key: &str) -> Option<CType> {
        self.struct_ctypes.get(key).cloned().flatten()
    }

    /// Cache every struct DECLARED IN THIS MODULE under its identity key, its by-value `CType::Struct`
    /// computed HERE — in this (the DEFINING) module's import/alias scope (extending the
    /// `AliasSig::ctype` precedent to structs). Called once per module from `check_module` after
    /// `hoist` (so all of this module's aliases/`from`-imports are live) and BEFORE the check_stmt loop
    /// (so a same-module extern harvested in the loop reads the cache). Modules are checked deps-first,
    /// so a downstream importer's extern returning `mod.Struct` reads this cached, defining-scope CType
    /// verbatim. Gated to the extern-harvesting pass; a lone `check` never builds these.
    pub(super) fn populate_struct_ctypes(&mut self, stmts: &[Stmt], id: Option<&ModuleId>) {
        let Some(mid) = id else { return };
        let mid = mid.clone();
        for stmt in stmts {
            if let StmtKind::Struct { name, .. } = &stmt.kind {
                let key = self.type_key(&mid, name);
                if self.struct_ctypes.contains_key(&key) {
                    continue; // already cached (a cross-module forward-ref resolved it earlier).
                }
                let c = self.struct_ctype_from_asts(&key, 0);
                self.struct_ctypes.insert(key, c);
            }
        }
    }

    /// Build a by-value `CType::Struct` for the struct under IDENTITY `key` from its raw field ASTs,
    /// mapping each AST field type to its own width-bearing `CType` (so an `int32` field stays 4 bytes
    /// — the layout the C ABI expects) IN THE CURRENT SCOPE. `key` is the identity key (the tag a
    /// returned struct carries, so field lookup hits). `None` if a field isn't a scalar leaf (a
    /// non-marshallable struct — the marshal gate rejects it). The ONLY field-resolving path — only
    /// `populate_struct_ctypes` calls it, in the defining scope, so there is exactly one resolver.
    pub(super) fn struct_ctype_from_asts(&self, key: &str, depth: usize) -> Option<CType> {
        let fields = self.struct_field_asts.get(key)?;
        let mut cfields = Vec::with_capacity(fields.len());
        let mut field_names = Vec::with_capacity(fields.len());
        for (fname, fty) in fields {
            cfields.push(self.resolve_ctype_d(fty, depth + 1)?);
            field_names.push(fname.clone());
        }
        Some(CType::Struct {
            name: key.to_string(),
            field_names,
            fields: cfields,
        })
    }

    /// Does concrete `ty` satisfy `protocol` instantiated with `args` (the bound's type arguments,
    /// e.g. `[int]` for `Container[int]`)? `args` is empty for a bare protocol. For a parameterized
    /// protocol the structural check substitutes the protocol's type params with `args` before
    /// matching method signatures.
    pub(super) fn satisfies_args(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
    ) -> Result<(), String> {
        self.satisfies_args_d(ty, protocol, args, 0)
    }

    /// Depth-bounded core of [`satisfies_args`]. `depth` guards the embed-flattening recursion (M22):
    /// cycles are rejected at declare time, but a malformed cyclic program still runs the rest of the
    /// checker, so a hard cap (mirroring `resolve_ty_ro_d`) breaks the recursion with a plain failure
    /// instead of overflowing the stack.
    pub(super) fn satisfies_args_d(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
        depth: usize,
    ) -> Result<(), String> {
        let Some(pinfo) = self.protocols.get(protocol) else {
            return Err(format!("unknown protocol '{protocol}'"));
        };
        if let Ty::Unknown = ty {
            return Ok(()); // don't cascade
        }
        // An EMPTY structural protocol (zero embeds AND zero methods — e.g. the `Any` top type) is
        // satisfied by EVERY type, scalars included. Without this short-circuit a zero-method/zero-embed
        // protocol would fall past every intrinsic arm to the `_ => Err` at the bottom for Int/Float/
        // Bool/Str/Nil (structs pass via the vacuous `satisfies_methods` over zero methods, but scalars
        // have no structural arm). This makes any empty protocol a genuine top type for every `Ty`.
        if pinfo.embeds.is_empty() && pinfo.methods.is_empty() {
            return Ok(());
        }
        // M22 — embedded (super-)protocols: a type satisfies `protocol` iff it satisfies every embed
        // (transitively) AND has every OWN method below. A PURE bundle (`Arithmetic` = embeds only,
        // no own methods) short-circuits once its embeds pass — this is what lets int/float/struct
        // satisfy `Arithmetic` (each embed recurses into the intrinsic/structural arms). A `Ty::Param`
        // is NOT flattened here — it forwards through its declared bounds in the `Ty::Param` arm below
        // (which knows, via `bound_provides`, that an `Arithmetic`-bound param provides Add/Sub/…).
        if !pinfo.embeds.is_empty() && !matches!(ty, Ty::Param(_)) {
            if depth <= 64 {
                for emb in &pinfo.embeds {
                    let eargs: Vec<Ty> = emb.args.iter().map(|a| self.resolve_ty_ro(a)).collect();
                    self.satisfies_args_d(ty, &emb.name, &eargs, depth + 1)?;
                }
            }
            if pinfo.methods.is_empty() {
                return Ok(()); // pure bundle — all embeds satisfied, no own methods to check
            }
        }
        if protocol == "Comparable" && matches!(ty, Ty::Int | Ty::Float | Ty::Str) {
            return Ok(());
        }
        // `Stringable` (sole method `str(self) -> str`) is satisfied intrinsically by every scalar —
        // all four stringify (int/float/bool/str), so a `[T: Stringable]` generic accepts them (the
        // erased body's `v.str()` is dispatched by the scalar `str` branch in `Vm::do_method_call`).
        // Note the membership is all FOUR scalars — unlike Comparable (no Bool) / Hashable (no Float).
        // Structs/enums/newtypes still fall through to the structural `satisfies_methods` below (a type
        // WITHOUT a `str(self) -> str` method stays correctly rejected; newtypes stay opt-in).
        if protocol == "Stringable" && matches!(ty, Ty::Int | Ty::Float | Ty::Bool | Ty::Str) {
            return Ok(());
        }
        // `Hashable` is satisfied intrinsically by the scalar key types (mirrors the map/set key
        // restriction; float is excluded — its equality is a hazard). Struct conformance falls
        // through to the structural check (needs a `hash(self) -> int` method).
        if protocol == "Hashable" && matches!(ty, Ty::Int | Ty::Str | Ty::Bytes | Ty::Bool) {
            return Ok(());
        }
        // A ZERO-FIELD struct WITHOUT an explicit `hash(self)` method is intrinsically `Hashable`: it
        // has no state to hash, so both engines return a constant hash for it (with `==`'s type-tag
        // guard keeping distinct empty-struct types unequal despite the hash collision). This lets
        // `struct S: pass` be used as a Set element / Map key without an explicit `hash(self)` method.
        // The `!methods.contains_key("hash")` clause MUST mirror the runtime `struct_hash` guard
        // (src/vm/mod.rs, src/interp/mod.rs): the runtime only substitutes the constant-0 hash when the
        // struct has NO `hash` method — a zero-field struct that DOES define `hash` gets its method
        // dispatched. So a zero-field struct WITH a `hash` method must fall through to the structural
        // check (which validates `hash(self) -> int`), or a mis-typed `hash` (wrong return / arity)
        // would pass the checker and fault at runtime (check-ok/run-diverge).
        if protocol == "Hashable"
            && let Ty::Struct(name, _) = ty
            && self
                .structs
                .get(name)
                .is_some_and(|d| d.fields.is_empty() && !d.methods.contains_key("hash"))
        {
            return Ok(());
        }
        // `str` conforms to `Error` intrinsically (Go-style: its message is itself).
        if protocol == "Error" && matches!(ty, Ty::Str) {
            return Ok(());
        }
        // `Iterator` conformance is exactly "can be iterated" — built-in collections intrinsically,
        // a user struct via its structural `next(self) -> Option[E]`. Reusing `iter_elem` keeps this
        // in lockstep with what `for` accepts (single source of truth, no drift). A `Ty::Param` falls
        // through to the declared-bounds check below (so a `[S: Iterator[T]]` value forwards into
        // another iterator-generic call), since `iter_elem` can't see through a bare param.
        if protocol == "Iterator" && !matches!(ty, Ty::Param(_)) {
            return if self.iter_elem(ty).is_some() {
                Ok(())
            } else {
                Err(format!("type {ty} does not satisfy Iterator"))
            };
        }
        // `Iterable` conformance is "can produce a fresh cursor". Built-in collections satisfy it
        // intrinsically; ANY `Iterator[T]`-satisfying type satisfies it too (every Iterator IS
        // Iterable — `iter()` returns self), so `iter_elem` (which already covers both) is reused as
        // the predicate. A user struct with a structural `iter(self) -> Iterator[E]` (but no `next`)
        // is caught by the `iterable_elem` helper. The bound's `[T]` arg, if supplied and concrete,
        // must match the element type (mirrors the parameterized-`Index` arg check). A `Ty::Param`
        // falls through to the declared-bounds check below (so `[S: Iterable[T]]` forwards).
        if protocol == "Iterable" && !matches!(ty, Ty::Param(_)) {
            let Some(elem) = self.iterable_elem(ty) else {
                return Err(format!("type {ty} does not satisfy Iterable"));
            };
            if let Some(want) = args.first()
                && !want.is_unknown()
                && !elem.is_unknown()
                && !compatible(want, &elem)
            {
                return Err(format!("type {ty} does not satisfy Iterable"));
            }
            return Ok(());
        }
        // `Index`/`IndexSet`/`Slice` — built-in `list`/`map`/`str` conform intrinsically (a struct
        // conforms structurally, falling through to the matcher below; a `Ty::Param` forwards to its
        // declared bounds). `str` is immutable, so it satisfies `Index`/`Slice` but NOT `IndexSet`.
        if matches!(protocol, "Index" | "IndexSet" | "Slice")
            && !matches!(ty, Ty::Param(_) | Ty::Struct(..))
        {
            let provided: Vec<Ty> = match protocol {
                "Slice" => match self.slice_result(ty) {
                    Some(r) => vec![r],
                    None => return Err(format!("type {ty} does not satisfy Slice")),
                },
                _ => {
                    if protocol == "IndexSet"
                        && !matches!(ty, Ty::List(_) | Ty::Map(_, _) | Ty::ByteArray)
                    {
                        return Err(format!("type {ty} does not satisfy IndexSet"));
                    }
                    match self.index_kv(ty) {
                        Some((k, v)) => vec![k, v],
                        None => return Err(format!("type {ty} does not satisfy {protocol}")),
                    }
                }
            };
            // Any args the bound supplied must match what the built-in actually provides.
            for (want, got) in args.iter().zip(&provided) {
                if !want.is_unknown() && !got.is_unknown() && !compatible(want, got) {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                }
            }
            return Ok(());
        }
        // A protocol existential value satisfies a protocol iff it IS that protocol AND its carried
        // args match the required ones (arity + arg-wise `compatible`). This enforces strict
        // invariance when a Protocol VALUE is the subject: a bare `Container` value does NOT satisfy
        // `Container[int]` (0 args vs 1) and vice-versa, and `Container[str]` ≠ `Container[int]`.
        if let Ty::Protocol(p, pargs) = ty {
            let args_match =
                pargs.len() == args.len() && pargs.iter().zip(args).all(|(x, y)| compatible(x, y));
            return if p == protocol && args_match {
                Ok(())
            } else {
                Err(format!("type {ty} does not satisfy {protocol}"))
            };
        }
        // The numeric operator protocols are satisfied intrinsically by int/float (their `+ - * / %`
        // and unary `-` are the primitive ops), so a `[T: Add + Mul]` / `[T: Div]` / `[T: Neg]`
        // generic works over numbers as well as structs.
        if matches!(protocol, "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Neg")
            && matches!(ty, Ty::Int | Ty::Float)
        {
            return Ok(());
        }
        // A bound type parameter satisfies a protocol if that protocol is among its declared bounds —
        // this is what lets a generic forward its `T: P` value into another `[U: P]` call. For a
        // parameterized protocol the bound's type args must also match the required ones, so a
        // `Container[str]` value is NOT accepted where `Container[int]` is required (forwarding hole).
        if let Ty::Param(name) = ty {
            let matched = self.type_params.get(name).is_some_and(|bs| {
                bs.iter()
                    .any(|b| self.bound_provides(&b.name, &b.args, protocol, args, 0))
            });
            return if matched {
                Ok(())
            } else {
                Err(format!("type {ty} does not satisfy {protocol}"))
            };
        }
        match ty {
            Ty::Struct(sname, _) => {
                // MISS-ONLY identity-key fallback (gap #4): a named-fn-imported factory result carries
                // its owning module's `Ty::Struct` key but injects nothing into the local `self.structs`
                // table, so resolve the shape from the owning `ModuleSig` on a local miss — otherwise a
                // structurally-conforming value is spuriously rejected at a protocol bound (the same
                // three-import-forms inconsistency the member-access fix already closed).
                let Some(info) = self.struct_shape(sname) else {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                };
                self.satisfies_methods(ty, protocol, args, pinfo, &info.methods)
            }
            // Enum conformance is structural exactly like a struct's: the enum satisfies `protocol`
            // iff its `methods` map carries every protocol method with a matching signature. This
            // unlocks Stringable/Hashable/Add/Sub/Mul/Comparable for enums and protocol-bound generics.
            Ty::Enum(ename, _) => {
                // MISS-ONLY identity-key fallback (gap #4): resolve a named-fn-imported enum value's
                // method table from the owning `ModuleSig` on a local-table miss (see the struct arm).
                let Some(methods) = self.enum_methods_of(ename) else {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                };
                self.satisfies_methods(ty, protocol, args, pinfo, methods)
            }
            // A newtype satisfies a protocol structurally via its OWN methods (like struct/enum).
            // PLUS, when its underlying is numeric, it intrinsically satisfies the operator protocols
            // (`Add`/`Sub`/`Mul`/`Div`/`Mod`/`Comparable`) — its same-type `+`/`<` use the native op
            // (unwrap→op→rewrap), so a `newtype Meters = float` flows into a `[T: Add]` generic with
            // no user `add` method. Hashable/Stringable stay strictly opt-in (the user's own method).
            Ty::NewType(ntkey, _) => {
                // The intrinsic numeric-operator satisfaction is for SCALAR newtypes only. A generic
                // newtype is methods-only — even `newtype Box[T] = T` gets no native Add/Sub/Mul/
                // Comparable; operators come strictly from its own methods (checked below).
                let numeric = !self.newtype_is_generic(ntkey)
                    && self
                        .newtype_underlying(ntkey)
                        .is_some_and(|u| u.is_numeric());
                if numeric
                    && matches!(
                        protocol,
                        "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Comparable"
                    )
                {
                    return Ok(());
                }
                // SOUNDNESS: a newtype operator overload defined as a METHOD is NEVER dispatched at
                // runtime — the same-newtype operator arm (vm `newtype_arith` / `compare_op`, interp
                // `eval_binop`) always auto-flows to the UNDERLYING's native op, and unary `-` has no
                // newtype path at all. So an operator protocol on a newtype is satisfiable ONLY via the
                // numeric auto-flow above; admitting it structurally here would type-check a call that
                // diverges on every engine (`check` ok / `run` silently using the native op, or
                // "cannot apply <Op> to <underlying>"). A non-numeric (or generic) newtype therefore
                // does NOT satisfy these — its own `add`/`sub`/`mul`/`div`/`mod`/`neg`/`compare` method
                // is intentionally unreachable as an operator. `Comparable` is in this list for the
                // same reason: same-newtype `<`/`<=`/`>`/`>=` always uses the underlying's NATIVE
                // ordering (`compare_op`'s `same_newtype_keys` fast path), never the user `compare`, so
                // a generic newtype (the only non-numeric case that reaches here after the numeric
                // short-circuit) must NOT claim `Comparable` via a method — that would be check-ok /
                // run-divergent. (Hashable/Stringable/Iterable/etc. still resolve structurally below.)
                if matches!(
                    protocol,
                    "Add" | "Sub" | "Mul" | "Div" | "Mod" | "Neg" | "Comparable"
                ) {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                }
                // MISS-ONLY identity-key fallback (gap #4): resolve a named-fn-imported newtype value's
                // method table from the owning `ModuleSig` on a local-table miss (see the struct arm).
                let Some(methods) = self.newtype_methods_of(ntkey) else {
                    return Err(format!("type {ty} does not satisfy {protocol}"));
                };
                self.satisfies_methods(ty, protocol, args, pinfo, methods)
            }
            _ => Err(format!("type {ty} does not satisfy {protocol}")),
        }
    }

    /// Structural conformance check shared by the struct and enum arms of [`satisfies_args`]: a type
    /// satisfies `protocol` iff `methods` carries every protocol method with a matching signature
    /// (the protocol's own type params substituted from the bound's `args`; `Self` handled inside
    /// `method_matches`).
    pub(super) fn satisfies_methods(
        &self,
        ty: &Ty,
        protocol: &str,
        args: &[Ty],
        pinfo: &ProtocolInfo,
        methods: &HashMap<String, FnSig>,
    ) -> Result<(), String> {
        let pmap: HashMap<String, Ty> = pinfo
            .type_params
            .iter()
            .cloned()
            .zip(args.iter().cloned())
            .collect();
        // The RECEIVING type's own param→arg substitution (e.g. `T→int` from `Box[int]`). The user's
        // stored methods carry the struct/enum/newtype's type params UNsubstituted (a method on
        // `Box[T]` is stored as `add(self, o: Box[T]) -> Box[T]`), so for a generic instantiation we
        // must bind those params to the instantiation's args before comparing against the (Self-bound)
        // protocol signature — otherwise `compatible(Box[int], Box[T])` fails and a perfectly good
        // operator method is rejected as "wrong signature". Non-generic types yield an empty map (no
        // params), and checking inside the generic's own def (`ty = Box[T]`) yields an identity map
        // `{T→T}` — both are no-op substitutions, so this only ever binds concrete args.
        // Each arm resolves the receiving type's params through the miss-only identity-key helpers
        // (gap #4), so a generic named-fn-imported instantiation (`Box[int]` from a factory whose TYPE
        // name is not imported) binds its params here identically to a whole-module import.
        let tymap: HashMap<String, Ty> = match ty {
            Ty::Struct(name, targs) => self
                .struct_shape(name)
                .map(|info| struct_param_map(info, targs))
                .unwrap_or_default(),
            Ty::Enum(name, targs) => self.enum_param_map(name, targs),
            Ty::NewType(key, targs) => self
                .newtype_type_params_of(key)
                .map(|tps| {
                    tps.iter()
                        .map(|tp| tp.name.clone())
                        .zip(targs.iter().cloned())
                        .collect()
                })
                .unwrap_or_default(),
            _ => HashMap::new(),
        };
        for (mname, msig) in &pinfo.methods {
            let subst_params: Vec<Ty> = msig.params.iter().map(|t| subst(t, &pmap)).collect();
            let min_params = subst_params.len();
            let want = FnSig {
                labels: Vec::new(),
                params: subst_params,
                ret: subst(&msig.ret, &pmap),
                type_params: Vec::new(),
                where_bounds: Vec::new(),
                min_params,
                // Carry the protocol requirement's static-ness (e.g. `Convert`'s `convert(x: S)` is
                // STATIC) so `method_matches` can reject an instance/self-slot witness of a static slot.
                // Instance-method requirements keep `is_static == false`, so this is inert for them.
                is_static: msig.is_static,
                doc: None,
                variadic: None,
            };
            let msig = &want;
            // Pre-substitute the receiving type's params into the ACTUAL (user) method signature so
            // its `Box[T]` becomes `Box[int]` before the comparison. Only the actual side is bound —
            // a genuinely wrong sig (`add(self, o: int) -> int`) stays a mismatch, no laundering.
            let actual_owned = methods.get(mname).map(|actual| {
                if tymap.is_empty() {
                    actual.clone()
                } else {
                    FnSig {
                        params: actual.params.iter().map(|t| subst(t, &tymap)).collect(),
                        ret: subst(&actual.ret, &tymap),
                        ..actual.clone()
                    }
                }
            });
            match actual_owned.as_ref() {
                Some(actual) if method_matches(msig, actual, ty) => {
                    // Conditional conformance: a method whose `where` bounds the RECEIVER's own type
                    // param (e.g. `compare(self, o: Box[T]) -> int where T: Comparable` on `Box[T]`)
                    // only makes the type satisfy `protocol` when that bound HOLDS for this concrete
                    // instantiation. Enforce it structurally here — so EVERY satisfies-based consumer
                    // (operator dispatch, generic bounds, protocol-typed params, `for`) is sound, not
                    // just explicit `.method()` calls (which enforce at the call site). `tymap` maps the
                    // receiver param (`T`) to the concrete arg; `Ty::Unknown` args defer (satisfies_args
                    // returns `Ok` on Unknown), matching `List.sort`'s late-inference behaviour.
                    // Recursion terminates by structural descent (each level checks a smaller type arg).
                    // Pre-conditional-methods code has no `where_bounds` on any method, so this loop is a
                    // no-op there — zero effect on existing structural conformance.
                    for wb in &actual.where_bounds {
                        let Some(concrete) = tymap.get(&wb.name) else {
                            continue;
                        };
                        for bound in &wb.bounds {
                            let bargs: Vec<Ty> =
                                bound.args.iter().map(|a| self.resolve_ty_ro(a)).collect();
                            if self.satisfies_args(concrete, &bound.name, &bargs).is_err() {
                                return Err(format!(
                                    "type {ty} does not satisfy {protocol} (method '{mname}' requires {}: {})",
                                    concrete, bound.name
                                ));
                            }
                        }
                    }
                }
                Some(_) => {
                    return Err(format!(
                        "type {ty} does not satisfy {protocol} (method '{mname}' has the wrong signature)"
                    ));
                }
                None => {
                    return Err(format!(
                        "type {ty} does not satisfy {protocol} (missing method '{mname}')"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Result type of an overloaded arithmetic operator (`+`/`-`/`*`) on two operands of the *same*
    /// struct or type-parameter that satisfies `protocol` (`Add`/`Sub`/`Mul`). The runtime dispatches
    /// to the `add`/`sub`/`mul` method; the result type is that same type. `None` ⇒ not overloadable.
    pub(super) fn op_overload_result(&self, l: &Ty, r: &Ty, protocol: &str) -> Option<Ty> {
        // A SAME newtype with a NUMERIC underlying auto-applies the underlying's NATIVE arithmetic op
        // (unwrap→op→rewrap, NOT a user `add`) and returns the newtype. `Meters + float` /
        // `Meters + Seconds` don't match (different/non-newtype operands) → the caller's "cannot
        // apply" error. (A user-defined `add` method also works via the satisfies() path below, but
        // the native numeric op is the no-method common case.)
        if let (Ty::NewType(a, _), Ty::NewType(b, _)) = (l, r)
            && a == b
            && !self.newtype_is_generic(a)
            && self.newtype_underlying(a).is_some_and(|u| u.is_numeric())
        {
            return Some(l.clone());
        }
        // SAME generic type REQUIRES matching type ARGS, not just the same name. The user's operator
        // method `add(self, o: Box[T]) -> Box[T]` on a `Box[int]` receiver needs `o: Box[int]`, so a
        // heterogeneous pair like `Box[int] + Box[str]` must NOT overload — admitting it would infer
        // the result `Box[int]` for a value carrying a `Box[str]` (the returned type the checker can't
        // honor → runtime type confusion). `compatible` checks name + pairwise-compatible targs; an
        // `Unknown` targ on a partially-inferred side still unifies (no false rejection there).
        let same = match (l, r) {
            (Ty::Struct(..), Ty::Struct(..))
            | (Ty::Enum(..), Ty::Enum(..))
            | (Ty::NewType(..), Ty::NewType(..)) => compatible(l, r),
            (Ty::Param(a), Ty::Param(b)) => a == b,
            _ => false,
        };
        if same && self.satisfies(l, protocol).is_ok() {
            Some(l.clone())
        } else {
            None
        }
    }

    /// The resolved underlying `Ty` of a newtype (by runtime key), if known. Falls back to the owning
    /// module's `ModuleSig` on a local-table miss (gap #4), so a named-fn-imported newtype value's
    /// numeric auto-flow / operator satisfaction is decided identically to a whole-module import.
    pub(super) fn newtype_underlying(&self, key: &str) -> Option<Ty> {
        self.newtype_defs
            .get(key)
            .map(|(u, _)| u.clone())
            .or_else(|| self.owning_newtype_def(key).map(|nt| nt.underlying.clone()))
    }

    /// Is the newtype (by runtime key) type-parameterized? A generic newtype is METHODS-ONLY — it
    /// gets no native operator auto-flow (even over a numeric/`T` underlying); operators come strictly
    /// from its own methods + protocol satisfaction. Gates the scalar-newtype auto-flow paths.
    pub(super) fn newtype_is_generic(&self, key: &str) -> bool {
        // MISS-ONLY identity-key fallback (gap #4): a named-fn-imported newtype value injects nothing
        // into the local `newtype_type_params` table, so consult the owning `ModuleSig` on a miss.
        self.newtype_type_params
            .get(key)
            .map(|t| !t.is_empty())
            .or_else(|| {
                self.owning_newtype_def(key)
                    .map(|nt| !nt.type_params.is_empty())
            })
            .unwrap_or(false)
    }

    /// Are `l < r` etc. allowed? True for same-named comparable type params, or same-named structs
    /// that satisfy `Comparable` (operator overloading dispatches to their `compare` at runtime).
    pub(super) fn ordering_allowed(&self, l: &Ty, r: &Ty) -> bool {
        match (l, r) {
            (Ty::Param(a), Ty::Param(b)) if a == b => self.type_params.get(a).is_some_and(|bs| {
                bs.iter()
                    .any(|proto| self.protocol_has_method(&proto.name, "compare"))
            }),
            // Same generic struct/enum REQUIRES matching type ARGS (`compatible` = name + targs), not
            // just the same name — `Box[int] < Box[str]` must not overload `compare` (same
            // heterogeneous laundering as `+`; see `op_overload_result`).
            (Ty::Struct(..), Ty::Struct(..)) if compatible(l, r) => {
                self.satisfies(l, "Comparable").is_ok()
            }
            (Ty::Enum(..), Ty::Enum(..)) if compatible(l, r) => {
                self.satisfies(l, "Comparable").is_ok()
            }
            // Same SCALAR newtype with a numeric underlying: `Meters < Meters` uses the underlying's
            // native ordering (returns bool). A user `compare` method also enables it via satisfies()
            // (the only path for a generic newtype — methods-only, no native ordering auto-flow).
            (Ty::NewType(a, _), Ty::NewType(b, _)) if a == b => {
                (!self.newtype_is_generic(a)
                    && self.newtype_underlying(a).is_some_and(|u| u.is_numeric()))
                    || self.satisfies(l, "Comparable").is_ok()
            }
            _ => false,
        }
    }

    pub(super) fn protocol_has_method(&self, protocol: &str, method: &str) -> bool {
        self.protocols
            .get(protocol)
            .is_some_and(|p| p.methods.iter().any(|(n, _)| n == method))
    }

    /// Whether `name` is a *generic* user fn / struct / enum-variant constructor (i.e. one that can
    /// accept explicit call-site type arguments). Non-generic decls and builtins return `false`.
    /// Can a value of this type cross a task boundary (`spawn` capture / argument, `Channel.send`)?
    /// Scalars, strings, and containers of sendable elements can; `Channel` (and `Shared`, C3)
    /// handles can. Closures/functions (bound to a heap), modules, and protocol existentials (which
    /// may wrap a closure) cannot. A struct/enum is sendable iff *all its field/payload types* are —
    /// inspected via the registry so a closure smuggled inside a struct field is caught. A generic
    /// type parameter (`Param`) is treated as sendable (the opaque-body case; concrete call sites
    /// resolve to a real type that is checked).
    pub(super) fn sendable(&self, ty: &Ty) -> bool {
        self.sendable_rec(ty, &mut Vec::new())
    }

    /// `sendable` with a cycle guard (`stack` holds the struct/enum names currently being walked,
    /// so a recursive type like `Node { next: Option[Node] }` terminates).
    pub(super) fn sendable_rec(&self, ty: &Ty, stack: &mut Vec<String>) -> bool {
        match ty {
            // `bytearray` crosses by deep copy (a fresh independent buffer on the other side, like
            // `list`) — always sendable (its elements are always `int`).
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::Unknown
            | Ty::Param(_) => true,
            // A `Shared[T]` handle always crosses — that's its whole point (one box, many tasks);
            // its element type is *not* a constraint (the value never crosses, only the handle).
            Ty::Shared(_) => true,
            // A `RwShared[T]` handle crosses for the same reason as `Shared` — one box, many tasks;
            // the element type is not a constraint (only the handle crosses).
            Ty::RwShared(_) => true,
            // An `Atomic[T]` handle crosses for the same reason as `Shared` — one box, many tasks;
            // the element type is not a constraint (only the handle crosses).
            Ty::Atomic(_) => true,
            // An `Executor` handle crosses the airlock like a `Channel`/`Shared` handle (the queue
            // lives outside every heap; tasks reach the one work queue).
            Ty::Executor => true,
            // D6 — a `Socket`/`Listener` handle crosses the airlock like the other core handles (the
            // fd lives in an `Arc`'d core outside every heap), so a `parallel:` accept-loop can
            // `spawn handle(conn)` onto a fiber.
            Ty::Socket | Ty::Listener => true,
            // R2 — a `Writer` handle crosses the airlock like the socket handles (the fd/buffer lives
            // in an `Arc`'d core outside every heap), so a `spawn`ed fiber can write to it.
            // R2b — a `Reader` handle likewise (its `BufReader<File>` lives in an `Arc`'d core).
            Ty::Writer | Ty::Reader => true,
            // An opaque `ptr` is a plain raw address (a `usize`) — it crosses the airlock by value,
            // so it is always sendable (its referent, if any, is the foreign library's concern).
            Ty::Ptr => true,
            Ty::List(t) | Ty::Set(t) | Ty::Option(t) | Ty::Channel(t) => {
                self.sendable_rec(t, stack)
            }
            Ty::Map(k, v) => self.sendable_rec(k, stack) && self.sendable_rec(v, stack),
            Ty::Result(t, e) => self.sendable_rec(t, stack) && self.sendable_rec(e, stack),
            Ty::Tuple(elems) => elems.iter().all(|t| self.sendable_rec(t, stack)),
            // B3.3 (Task 2a) — a user `Func` (closure / nested fn / bare fn) crosses the airlock BY
            // VALUE now (the runtime `to_wire`/`to_snap` lowering carries its proto + wired captures),
            // so the bare `fn` type is sendable. The bare type cannot carry its captures, so the
            // per-closure capture-sendability check is done at the airlock SITES (the `spawn:` block
            // read gate + the spawn callee/arg gate in `sig.rs`), NOT here. `Ty::Module`/`Ty::Protocol`
            // stay non-sendable (a module namespace / protocol witness never crosses).
            Ty::Func { .. } => true,
            Ty::Module(_) | Ty::Protocol(_, _) => false,
            // A first-class builtin fn value is pure code (no captured environment) — always
            // sendable, so a `f := ord` captured into a spawned task crosses the airlock (the
            // `Obj::Builtin`/`SnapValue::Builtin` runtime path), unlike a conservatively-non-sendable
            // user `Func`. All four builtins (`print`/`ord`/`chr`/`panic`) are covered uniformly.
            Ty::BuiltinFn { .. } => true,
            Ty::Struct(name, args) => {
                if !args.iter().all(|a| self.sendable_rec(a, stack)) {
                    return false;
                }
                if stack.contains(name) {
                    return true; // already being walked — the cycle adds no new type
                }
                match self.structs.get(name) {
                    Some(info) => {
                        let fields = info.fields.clone();
                        stack.push(name.clone());
                        let ok = fields.iter().all(|(_, fty)| self.sendable_rec(fty, stack));
                        stack.pop();
                        ok
                    }
                    None => true, // unknown struct: be permissive (any error is reported elsewhere)
                }
            }
            Ty::Enum(name, args) => {
                if !args.iter().all(|a| self.sendable_rec(a, stack)) {
                    return false;
                }
                if stack.contains(name) {
                    return true;
                }
                // Built-in Result/Option are erased here (their payloads are the type args, already
                // checked above); a user enum's variant payloads come from the registry.
                let payloads: Vec<Ty> = self
                    .variants
                    .values()
                    .filter(|v| &v.enum_name == name)
                    .flat_map(|v| v.payload.clone())
                    .collect();
                stack.push(name.clone());
                let ok = payloads.iter().all(|pty| self.sendable_rec(pty, stack));
                stack.pop();
                ok
            }
            // A newtype is sendable iff its underlying type is (it crosses by deep-copy of the inner
            // value, like a 1-field struct). Cycle-guarded by the newtype key.
            Ty::NewType(name, _) => {
                if stack.contains(name) {
                    return true;
                }
                // Substitute the newtype's instantiated type args into its underlying, so a generic
                // `Stack[int]` checks `list[int]` (not bare `list[T]`) for sendability.
                match self.newtype_unwrap_target(ty) {
                    Some(under) => {
                        stack.push(name.clone());
                        let ok = self.sendable_rec(&under, stack);
                        stack.pop();
                        ok
                    }
                    None => true,
                }
            }
        }
    }

    /// A targeted hint for the most common non-sendable channel element: the built-in `Error`
    /// existential — what a bare `T!` / `Result[T, Error]` erases to (rendered `Result[int]`). It
    /// isn't sendable because a value satisfying `Error` may carry a non-sendable field; point the
    /// user at a concrete error type. Empty string when the type doesn't mention `Error`.
    pub(super) fn sendable_error_hint(&self, ty: &Ty) -> String {
        if ty_mentions_error_existential(ty) {
            " — the built-in `Error` type can't cross a task boundary (a value satisfying `Error` \
             may hold data that can't be sent between tasks); name a concrete error type, e.g. \
             `Channel[int!str]` or `Channel[int!MyErr]`"
                .to_string()
        } else {
            String::new()
        }
    }

    /// Resolve the element type of a value-first concurrency box (`Shared`/`RwShared`/`Atomic`) from
    /// an OPTIONAL turbofish, mirroring the container-ctor turbofish pattern (the `List` arm above).
    /// With no turbofish the value's `inferred` type wins; with one type arg that arg pins the element
    /// type and is checked against `inferred` (a mismatch like `Shared[str](0)` errors); arity > 1 is
    /// rejected and falls back to `inferred`.
    pub(super) fn concurrency_turbofish_elem(
        &mut self,
        name: &str,
        targs: &[Ty],
        inferred: Ty,
        span: Span,
    ) -> Ty {
        match targs {
            [] => inferred,
            [t] => {
                let t = t.clone();
                if !t.is_unknown() && !inferred.is_unknown() && !self.assignable(&t, &inferred) {
                    self.error(
                        span,
                        format!("{name}[{t}]() expected element type {t}, found {inferred}"),
                    );
                }
                t
            }
            _ => {
                self.error(span, format!("{name}[T]() takes exactly one type argument"));
                inferred
            }
        }
    }

    pub(super) fn name_is_generic(&self, name: &str) -> bool {
        // The built-in `Channel[T]()` constructor takes its element type as an explicit type arg.
        // The container constructors `List[T]()` / `Set[T]()` / `Map[K, V]()` likewise accept a
        // turbofish that pins the (otherwise un-inferable, for an empty container) element type.
        if matches!(name, "Channel" | "List" | "Set" | "Map") {
            return true;
        }
        // The value-first concurrency boxes accept an OPTIONAL turbofish that pins the element type
        // (`Shared[T](v)`); when present it is checked against the value's inferred type. Unlike
        // `Channel[T]()` the type arg is optional (the value is required and inference still works).
        if matches!(name, "Shared" | "RwShared" | "Atomic") {
            return true;
        }
        // Look the struct up by its module-scoped runtime key (`bare_key`), NOT the bare name: under
        // the real `check_graph` path `structs` is keyed `<module>::Name`, so a bare-name lookup
        // always misses and wrongly reports user generic structs as non-generic — which made the
        // `infer_named_call` gate reject explicit call-site type args (`Pair[int, str](…)`) that the
        // struct-ctor branch fully supports. Mirrors the newtype arm below + the ctor branch itself.
        if let Some(i) = self.structs.get(&self.bare_key(name)) {
            return !i.type_params.is_empty();
        }
        // A generic newtype constructor takes turbofish type args (`Stack[int]([])`) — report it so
        // the args aren't pre-rejected by `infer_named_call`'s non-generic gate.
        if self.newtype_names.contains(name) && self.newtype_is_generic(&self.bare_key(name)) {
            return true;
        }
        // Bare-name query (the qualified path resolves genericity directly). A variant name may now
        // belong to several enums; treat it as generic if any owner enum is generic. `variant_owners`
        // stores BARE enum names but `enum_type_params` is module-keyed, so go through `bare_key`
        // (same keying fix as the struct/newtype arms above — else a bare generic-enum variant like
        // `Full[int](5)` reports the misleading "takes no type arguments" instead of the
        // "write it qualified as 'Box.Full'" hint under the real `check_graph` path).
        if let Some(owners) = self.variant_owners.get(name) {
            return owners.iter().any(|en| {
                self.enum_type_params
                    .get(&self.bare_key(en))
                    .is_some_and(|t| !t.is_empty())
            });
        }
        if let Some(s) = self.functions.get(name) {
            return !s.type_params.is_empty();
        }
        false
    }

    /// Seed a substitution map from explicit call-site type arguments, validating their count
    /// against the declared type parameters. Empty `targs` (the inference-only case) yields an
    /// empty map. A count mismatch is reported but the overlapping prefix is still seeded so
    /// inference can recover.
    pub(super) fn seed_targs(
        &mut self,
        name: &str,
        tps: &[TypeParam],
        targs: &[Ty],
        span: Span,
    ) -> HashMap<String, Ty> {
        let mut sub = HashMap::new();
        if !targs.is_empty() {
            if targs.len() != tps.len() {
                self.error(
                    span,
                    format!(
                        "'{name}' expects {} type argument(s), found {}",
                        tps.len(),
                        targs.len()
                    ),
                );
            }
            for (tp, ta) in tps.iter().zip(targs) {
                sub.insert(tp.name.clone(), ta.clone());
            }
        }
        sub
    }

    /// Recover element types from parameterized `Iterator[T]` bounds: for each type param already
    /// bound to a concrete iterand in `sub`, bind the bound's element arg `T` to the iterand's element
    /// type. Mutates `sub` (collects first to avoid borrowing it while iterating). Shared by every
    /// generic-call site (free fn, struct constructor, enum variant).
    pub(super) fn recover_iter_elems(
        &mut self,
        tps: &[TypeParam],
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) {
        let mut binds: Vec<(Ty, Ty)> = Vec::new();
        for tp in tps {
            if let Some(concrete) = sub.get(&tp.name).cloned() {
                for b in &tp.bounds {
                    if b.name == "Iterator"
                        && let Some(arg) = b.args.first()
                        && let Some(elem) = self.iter_elem(&concrete)
                    {
                        binds.push((self.resolve_bound_arg(arg, tps, span), elem));
                    }
                }
            }
        }
        for (arg_ty, elem) in &binds {
            // Bind the element param if it's still free; otherwise it was already pinned (an explicit
            // type arg, another argument position, or a concrete `Iterator[int]` bound) and the
            // recovered element MUST agree — `unify` is a silent no-op there, so check it ourselves.
            match arg_ty {
                Ty::Param(n) if !sub.contains_key(n) => {
                    if !elem.is_unknown() {
                        sub.insert(n.clone(), elem.clone());
                    }
                }
                _ => {
                    let pinned = match arg_ty {
                        Ty::Param(n) => sub.get(n).cloned().unwrap_or(Ty::Unknown),
                        other => other.clone(),
                    };
                    if !pinned.is_unknown() && !elem.is_unknown() && !self.assignable(&pinned, elem)
                    {
                        self.error(
                            span,
                            format!("iterator element type {elem} does not match the declared element type {pinned}"),
                        );
                    }
                }
            }
        }
    }

    /// Recover the `K`/`V` (`Index`/`IndexSet`) and `R` (`Slice`) type args of parameterized bounds
    /// from each type parameter's inferred binding — the indexing analogue of `recover_iter_elems`,
    /// so `fn first[C: Index[int, V], V](c: C) -> V` recovers `V` from the argument.
    pub(super) fn recover_index_args(
        &mut self,
        tps: &[TypeParam],
        sub: &mut HashMap<String, Ty>,
        span: Span,
    ) {
        let mut binds: Vec<(Ty, Ty)> = Vec::new();
        for tp in tps {
            let Some(concrete) = sub.get(&tp.name).cloned() else {
                continue;
            };
            for b in &tp.bounds {
                match b.name.as_str() {
                    "Index" | "IndexSet" => {
                        if let Some((k, v)) = self.index_kv(&concrete) {
                            if let Some(a) = b.args.first() {
                                binds.push((self.resolve_bound_arg(a, tps, span), k));
                            }
                            if let Some(a) = b.args.get(1) {
                                binds.push((self.resolve_bound_arg(a, tps, span), v));
                            }
                        }
                    }
                    "Slice" => {
                        if let Some(r) = self.slice_result(&concrete)
                            && let Some(a) = b.args.first()
                        {
                            binds.push((self.resolve_bound_arg(a, tps, span), r));
                        }
                    }
                    _ => {}
                }
            }
        }
        for (arg_ty, recovered) in &binds {
            // Bind the arg param if still free; otherwise it was already pinned and must agree.
            match arg_ty {
                Ty::Param(n) if !sub.contains_key(n) => {
                    if !recovered.is_unknown() {
                        sub.insert(n.clone(), recovered.clone());
                    }
                }
                _ => {
                    let pinned = match arg_ty {
                        Ty::Param(n) => sub.get(n).cloned().unwrap_or(Ty::Unknown),
                        other => other.clone(),
                    };
                    if !pinned.is_unknown()
                        && !recovered.is_unknown()
                        && !self.assignable(&pinned, recovered)
                    {
                        self.error(
                            span,
                            format!(
                                "index type {recovered} does not match the declared type {pinned}"
                            ),
                        );
                    }
                }
            }
        }
    }

    /// Enforce each type parameter's declared protocol bounds against its inferred binding. A
    /// parameterized bound (`Container[int]`) supplies type args, resolved here (sibling params in
    /// scope) and checked structurally with the protocol's params substituted.
    pub(super) fn enforce_bounds(
        &mut self,
        tps: &[TypeParam],
        sub: &HashMap<String, Ty>,
        span: Span,
    ) {
        for tp in tps {
            if let Some(concrete) = sub.get(&tp.name) {
                for bound in &tp.bounds {
                    // Resolve the bound's args, then substitute any params recovered into `sub` (e.g.
                    // `Index[int, V]` with `V` recovered to `int`) so the structural/intrinsic check
                    // sees concrete args, not a still-free `Ty::Param`.
                    let bargs: Vec<Ty> = bound
                        .args
                        .iter()
                        .map(|a| subst(&self.resolve_bound_arg(a, tps, span), sub))
                        .collect();
                    if let Err(msg) = self.satisfies_args(concrete, &bound.name, &bargs) {
                        self.error(span, msg);
                    }
                }
            }
        }
    }

    /// Type-check a call to a generic function: infer each type parameter from the arguments,
    /// enforce the declared bounds, and substitute into the return type.
    pub(super) fn infer_generic_call(
        &mut self,
        name: &str,
        sig: &FnSig,
        args: &[Expr],
        targs: &[Ty],
        span: Span,
        hint: Option<&Ty>,
    ) -> Ty {
        if args.len() != sig.params.len() {
            self.check_arity(name, sig.params.len(), args, span);
        }
        let arg_tys = self.infer_generic_arg_tys(args);
        // Explicit call-site type arguments (`max[int](…)`) seed the substitution; remaining (or
        // all, when none given) parameters are inferred from positional arguments. `unify` only
        // binds a parameter that isn't already in the map, so explicit args take precedence and a
        // conflicting argument is caught by the per-argument check below.
        let mut subst_map: HashMap<String, Ty> =
            self.seed_targs(name, &sig.type_params, targs, span);
        for (i, (decl, actual)) in sig.params.iter().zip(&arg_tys).enumerate() {
            // Bug D (free-fn analog): for an unannotated CLOSURE arg, unify against a RETURN-MASKED
            // copy so only its PARAMETER positions can bind a function type param in pass 1. Its
            // prepass return may be a leaked `Ty::Param` (an unannotated body that is a nested free
            // generic call, `fn(x): ident(x)`); letting it bind here would prematurely pin the fn's
            // return-position `[U]` to that leaked param, and the loop-back in
            // `recover_return_only_params` (which only fills params still FREE) could not correct it.
            // Non-closure args unify unchanged. Mirrors `infer_generic_method`.
            if matches!(
                args.get(i).map(|a| &a.kind),
                Some(ExprKind::Closure { ret: None, .. })
            ) {
                unify(decl, &mask_closure_ret(actual), &mut subst_map);
            } else {
                unify(decl, actual, &mut subst_map);
            }
        }
        // Bug 1 recovery (free-fn path only): after the masked pass-1 above has let every VALUE and
        // closure-PARAMETER arg pin its params, bind the HOF's return-only `[U]` from a bare closure
        // whose prepass return is ALREADY CONCRETE (`fn(): 5` → `int`, no leaked `Ty::Param`). This runs
        // BEFORE `report_uninferable_closure_params` so a `[U]` recoverable from such a closure RETURN is
        // not mis-reported as an un-inferable deadlock when a SIBLING closure uses the same `[U]` in
        // PARAMETER position (`pair(fn(): 5, fn(x): x + 1)` — adversarial-review bug 1). `unify` only
        // binds a param still FREE, so a sibling VALUE arg that already pinned `[U]` STILL WINS (no
        // closure-vs-value binding race, e.g. `apply(fn(x): str(x), 5, 99)` keeps `B` pinned to the
        // `sink: B` = 99 → `int` and the mismatching closure is rejected). A LEAKED-param prepass return
        // (`fn(x): ident(x)`) stays masked, deferred to the loop-back in `recover_return_only_params`.
        for (i, (decl, actual)) in sig.params.iter().zip(&arg_tys).enumerate() {
            // Gate on FULLY CONCRETE (no `Ty::Param` AND no `Ty::Unknown`, nested too), not merely
            // "contains no param": a param-dependent body prepass-types to an Unknown-CORED container
            // (`fn(x): [x]` → `List[Unknown]`), which has no `Ty::Param` — so a bare no-param gate would
            // `unify(U, List[Unknown])`, binding `U = List[Unknown]` (unify only skips a TOP-LEVEL
            // Unknown, not a nested one) and laundering `List[str]` onto it. A fully-concrete prepass
            // return (`fn(): 5` → `int`) still pre-binds here (needed so the `pair(fn(): 5, fn(x): x+1)`
            // ordering resolves); an Unknown/param-cored one defers to the loop-back's refined
            // checking-mode re-inference, which recovers the concrete type.
            if matches!(
                args.get(i).map(|a| &a.kind),
                Some(ExprKind::Closure { ret: None, .. })
            ) && matches!(actual, Ty::Func { ret, .. } if ty_fully_concrete(ret))
            {
                unify(decl, actual, &mut subst_map);
            }
        }
        // Recover element types from parameterized `Iterator[T]` bounds (bind `T` to the iterand's
        // element), then enforce every declared bound against its inferred binding.
        self.recover_iter_elems(&sig.type_params, &mut subst_map, span);
        self.recover_index_args(&sig.type_params, &mut subst_map, span);
        // Expected-type checking-mode: a `let`/return/param annotation seeds any type param the args
        // left FREE by unifying the declared RETURN type (already `Ty::Param`-bearing) against the
        // hint — so `xs: List[int] = empty()` pins a return-only `T`, and the deadlock probe below
        // sees it bound. After arg-unification ⇒ turbofish/args win.
        seed_from_hint(hint, &sig.ret, &mut subst_map);
        // Same un-inferable closure-param deadlock guard as the struct-ctor path: report the cause
        // (and bind the params to Unknown) before the per-arg closure body is checked.
        self.report_uninferable_closure_params(
            name,
            &sig.type_params,
            &sig.params,
            args,
            &mut subst_map,
            span,
        );
        self.enforce_bounds(&sig.type_params, &subst_map, span);
        // Each argument must match its parameter's substituted type (catches a type param used in
        // two positions with conflicting types, e.g. `max(1, "x")`), AND recover a return-only type
        // param from an inferable closure/fn body (Bug D's loop-back, now shared with the method
        // path): the free-fn path formerly discarded the refined closure type, leaking `Ty::Param`
        // into the return so a downstream `+1`/`.upper()` was spuriously rejected.
        self.recover_return_only_params(
            name,
            &sig.params,
            &arg_tys,
            args,
            &sig.params,
            &sig.type_params,
            &mut subst_map,
            span,
            false,
        );
        subst(&sig.ret, &subst_map)
    }

    /// Infer a generic *method*'s own type parameters from the call arguments. `params`/`ret` are the
    /// method signature already substituted with the receiver struct's type arguments, so only the
    /// method's own `[U]` params remain free; `params[0]` is the receiver (bound from `obj`, not an
    /// explicit arg). `targs` are the EXPLICIT member-level turbofish (`obj.method[A, B](...)`): they
    /// seed the `[U]` params first; the rest are inferred positionally. Mirrors `infer_generic_call`.
    #[allow(clippy::too_many_arguments)] // the method's resolved signature pieces + receiver + targs + call
    pub(super) fn infer_generic_method(
        &mut self,
        method: &str,
        params: &[Ty],
        ret: &Ty,
        mtps: &[TypeParam],
        recv_ty: &Ty,
        targs: &[Ty],
        args: &[Expr],
        span: Span,
    ) -> Ty {
        // The first parameter is the receiver (bound from `obj`). A method with NO params has no
        // receiver slot — reject, mirroring the non-generic path.
        let Some((receiver, expected)) = params.split_first() else {
            self.error(
                span,
                format!("method '{method}' has no receiver parameter (its first parameter must be the receiver, e.g. `self`)"),
            );
            self.infer_all(args);
            return Ty::Unknown;
        };
        if args.len() != expected.len() {
            self.error(
                span,
                format!(
                    "'{method}' expects {} argument(s), got {}",
                    expected.len(),
                    args.len()
                ),
            );
        }
        let mut arg_tys = self.infer_generic_arg_tys(args);
        // Explicit member-level turbofish seeds the `[U]` params (arity-checked); `unify` only binds
        // a param not already in the map, so an explicit targ wins and a conflicting arg is caught by
        // the per-argument check below.
        let mut mmap: HashMap<String, Ty> = self.seed_targs(method, mtps, targs, span);
        // A method type param may appear in the receiver position (`fn f[U](u: U)`); bind it from the
        // actual receiver type so it isn't left unresolved.
        unify(receiver, recv_ty, &mut mmap);
        // Clamp to the shorter of the two lengths — `arg_tys.len() == args.len()` can be < `expected.len()`
        // when the method is called with too few arguments. The arity error is already reported above
        // (it does not early-return), so this loop must not index `args[i]`/`arg_tys[i]` out of bounds;
        // the base `expected.iter().zip(&arg_tys)` clamped implicitly, this preserves that.
        for i in 0..expected.len().min(arg_tys.len()) {
            let decl = &expected[i];
            // Scope-A-through-a-method-slot: a bare same-module GENERIC fn passed as a non-closure arg
            // is prepass-typed rigid (`fn(T) -> str`) because `infer_generic_arg_tys` has no expected
            // hint. Re-pin its OWN `[T]` from the slot with everything bound SO FAR (`fn(int) -> U` for
            // `.map`; `fn(int, int) -> int` for `.fold`'s arg1 once `init` bound `U`), replacing the
            // rigid prepass type with the concrete one so pass-1 unify + the loop-back compose. This is
            // interleaved (uses the LIVE `mmap`) so `.fold`'s accumulator ordering holds. The helper only
            // fires on a bare-ident generic fn that pins FULLY concrete; otherwise `arg_tys[i]` is
            // unchanged and behavior is byte-identical.
            let want = subst(decl, &mmap);
            if let Some(refined) = self.try_pin_generic_fn_value_arg(&args[i], &want, span) {
                arg_tys[i] = refined;
            }
            let actual = &arg_tys[i];
            // Bug D: for a CLOSURE arg, unify against a RETURN-MASKED copy so only its PARAMETER
            // positions can bind a method type param in pass 1. Its prepass return may be a leaked
            // `Ty::Param` (an unannotated body that is a nested free generic call, `fn(x): ident(x)`);
            // letting it bind here would prematurely pin the method's return-position `[U]` to that
            // leaked param, and the loop-back below (which only fills params still FREE) could not
            // correct it. Masking defers `U` to the loop-back's checking-mode re-inference, which
            // recovers it as the CONCRETE return type. Non-closure args unify unchanged.
            if matches!(
                args.get(i).map(|a| &a.kind),
                Some(ExprKind::Closure { ret: None, .. })
            ) {
                unify(decl, &mask_closure_ret(actual), &mut mmap);
            } else {
                unify(decl, actual, &mut mmap);
            }
        }
        // Recover element types from `Iterator[T]` bounds, then enforce every declared bound.
        self.recover_iter_elems(mtps, &mut mmap, span);
        self.recover_index_args(mtps, &mut mmap, span);
        self.enforce_bounds(mtps, &mmap, span);
        // The receiver must still match its declared type AFTER substitution. Without this, a method
        // type param in receiver position (`fn m[U](self: U)`) turbofished to a contradicting type
        // (`b.m[str]()` on a `Box[int]`) is unchecked — `unify` silently drops the conflict once `[str]`
        // is seeded — and a wrong static type escapes onto the value (a soundness hole).
        let want_recv = subst(receiver, &mmap);
        if !self.assignable(&want_recv, recv_ty) {
            self.error(
                span,
                format!("receiver of '{method}' has type {recv_ty}, expected {want_recv}"),
            );
        }
        // Bug D closure-return recovery (shared with the free-fn path via `recover_return_only_params`):
        // re-infer each closure arg in checking-mode, reject a wrong CONCRETE return, loop-back-unify to
        // fill a still-free return-only `[U]`, re-enforce newly-recovered bounds, and degrade a still-
        // unbound param-position param to `Unknown`. `expected` = arg slots (sans receiver); `params` =
        // the full list incl receiver for the param-position degrade.
        self.recover_return_only_params(
            method, expected, &arg_tys, args, params, mtps, &mut mmap, span, true,
        );
        subst(ret, &mmap)
    }

    /// Scope A, delivered through a generic METHOD's concrete parameter slot. `arg` is a call argument
    /// to a builtin/generic container method (`.map`/`.fold`/…); `want` is that argument's declared slot
    /// type substituted with everything the method has pinned SO FAR (for `.map` this is `fn(int) -> U`,
    /// for `.fold`'s arg1 `fn(int, int) -> int`). When `arg` is a BARE reference to a same-module generic
    /// fn (`[1,2,3].map(conv)`), its `[T]` params are pinned from `want`'s concrete positions and the
    /// FULLY-substituted concrete fn type returned — the direct analog of `infer_ident`'s Scope A (a HOF
    /// PARAMETER-slot hint), which fires for a user HOF but not for these own-`[U]`-carrying builtin
    /// methods (their arg goes through `infer_generic_arg_tys` with no `expected_hint`, so it stays
    /// rigid). Returns `None` (leaving the rigid prepass type untouched) unless EVERY arg-fn param binds
    /// AND the result is fully concrete — so a still-free `want` slot (`.fold`'s arg1 before `init` binds
    /// `U`) or a genuinely un-inferable return-only arg-fn param defers to the existing path unchanged,
    /// preserving the Category-1 leak guard and every clean reject. A FRESH substitution map per call
    /// means two distinct pins never launder.
    fn try_pin_generic_fn_value_arg(&mut self, arg: &Expr, want: &Ty, span: Span) -> Option<Ty> {
        // Only a bare identifier that is NOT shadowed by an in-scope binding (`lookup` None) and IS a
        // same-module generic fn — mirrors Scope A's gate exactly.
        let ExprKind::Ident(name) = &arg.kind else {
            return None;
        };
        if self.lookup(name).is_some() || !self.local_fn_names.contains(name) {
            return None;
        }
        let sig = self.functions.get(name)?;
        if sig.type_params.is_empty() {
            return None;
        }
        // The slot must be a concrete-arity `fn(..) -> ..` (everything pinned so far); its param slots
        // drive the pin (`unify` binds the arg fn's params from them).
        let Ty::Func { params: wp, .. } = want else {
            return None;
        };
        if wp.len() != sig.params.len() {
            return None;
        }
        // Clone sig fields before the &mut-self `enforce_bounds` call (mirrors infer_ident Scope A).
        let type_params = sig.type_params.clone();
        let declared = Ty::Func {
            params: sig.params.clone(),
            ret: Box::new(sig.ret.clone()),
            labels: FnLabels(sig.labels.clone()),
        };
        let mut m: HashMap<String, Ty> = HashMap::new();
        unify(&declared, want, &mut m);
        // Accept ONLY when every arg-fn param bound AND the refined type is fully concrete. A slot
        // position that is still a free method param (`.fold` arg1 before `init` binds `U`) or a
        // return-only arg-fn param never pinned leaves a `Ty::Param` in `refined` → bail, unchanged.
        if !type_params.iter().all(|tp| m.contains_key(&tp.name)) {
            return None;
        }
        let refined = subst(&declared, &m);
        if !ty_fully_concrete(&refined) {
            return None;
        }
        // Enforce the arg fn's declared bounds against the bindings, exactly as Scope A does.
        self.enforce_bounds(&type_params, &m, span);
        Some(refined)
    }

    /// Bug D's closure-return recovery, shared by the generic-METHOD (`infer_generic_method`) and
    /// generic FREE-FN/module-qualified-fn (`infer_generic_call`) HOF paths. After pass-1 unification
    /// has seeded `map` with everything the argument SHAPES pin, this:
    ///  1. re-infers each closure arg in checking-mode against its substituted slot type (binding its
    ///     unannotated params, re-reporting its body errors) and captures the REFINED type;
    ///  2. SOUNDNESS: when the closure's expected return is ALREADY concrete (a return-only `[U]` pinned
    ///     by a sibling value arg or an explicit slot), rejects a genuinely wrong body against it — the
    ///     free-fn analog of `fold`-init laundering; a mask alone would launder a mismatching return;
    ///  3. LOOP-BACK: feeds the refined types into a SECOND `unify`, filling a return-only param still
    ///     free after pass 1 (recovered from an inferable closure/fn body) so it no longer leaks a
    ///     `Ty::Param` into the return;
    ///  4. re-enforces bounds on params NEWLY bound by the loop-back only (each enforced exactly once);
    ///  5. (METHOD PATH ONLY, `degrade_unbound_param_pos`) degrades a still-unbound PARAMETER-position
    ///     param to the refinable `Unknown` (the empty-collection case, `[].map(fn(x): x*2)` → `List[?]`,
    ///     matching the retired `infer_list_hof`), while leaving a genuinely un-inferable RETURN-ONLY
    ///     param as a leaked `Ty::Param` so `assignable` still rejects a concrete assignment.
    ///
    /// `arg_decls` are the per-argument declared slot types (method: params sans receiver; free-fn: all
    /// params) — parallel to `arg_tys`/`args`. `all_params` is the full param list used only for the
    /// param-position degrade scan (method: params INCL receiver; free-fn: all params).
    ///
    /// `degrade_unbound_param_pos` gates step 5. It is `true` ONLY on the generic-METHOD path, whose
    /// receiver-collection HOFs (`[].map(...)`) intentionally degrade an empty-collection element param
    /// to `List[?]`. It is `false` on the generic FREE-FN path: `infer_generic_call` never degraded, so
    /// a still-unbound param-position free-fn type param (`first([])` — `U` from an empty `List[U]` arg
    /// that flows to the return) must stay a leaked `Ty::Param` that downstream concrete use REJECTS,
    /// and the deliberate Category-2 "un-inferred type parameter; bind at the construction site"
    /// diagnostic must survive. Degrading it there silently laundered a compile error into a runtime
    /// panic (adversarial-review bugs 1 & 2). Free-fn CLOSURE-param type params left un-inferable by an
    /// empty arg are already bound to `Unknown` by the caller's `report_uninferable_closure_params`, so
    /// omitting the degrade there is behavior-preserving.
    ///
    /// The caller must complete pass-1 state (turbofish/arg-unify/iter+index recovery, and for the
    /// free-fn path its `report_uninferable_closure_params` + pass-1 `enforce_bounds`) BEFORE this call,
    /// so `bound_after_pass1` is correct and pass-1 bounds are enforced exactly once.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn recover_return_only_params(
        &mut self,
        name: &str,
        arg_decls: &[Ty],
        arg_tys: &[Ty],
        args: &[Expr],
        all_params: &[Ty],
        tps: &[TypeParam],
        map: &mut HashMap<String, Ty>,
        span: Span,
        degrade_unbound_param_pos: bool,
    ) {
        // Snapshot the params bound after pass 1, so the loop-back below only re-enforces bounds on
        // params NEWLY bound from a refined arg (pass-1 bounds are enforced by the caller).
        let bound_after_pass1: std::collections::HashSet<String> = map.keys().cloned().collect();
        for (decl, (actual, arg)) in arg_decls.iter().zip(arg_tys.iter().zip(args)) {
            let want = subst(decl, map);
            // For a closure whose UNANNOTATED body is a nested free generic call, the prepass return
            // leaks the callee's own `Ty::Param` (`fn(?) -> T`) — not the lenient `Unknown` a direct
            // body yields — so an unmasked fallback check would spuriously mismatch `want`'s return
            // whether that return is a still-free `[U]` OR already concrete. Mask an unannotated
            // closure's fallback return: `check_generic_arg`'s internal check stays on params + arity,
            // and the REAL return contract is enforced below against the REFINED type. Non-closure and
            // annotated-closure args use their prepass type unchanged.
            let is_bare_closure = matches!(arg.kind, ExprKind::Closure { ret: None, .. });
            let fallback = if is_bare_closure {
                mask_closure_ret(actual)
            } else {
                actual.clone()
            };
            let refined = self.check_generic_arg(name, &want, &fallback, arg);
            // SOUNDNESS: when the closure's expected return is ALREADY concrete (a return-only `[U]`
            // pinned by a sibling value arg or an explicit slot), enforce it explicitly here against the
            // REFINED return — rejecting a genuinely wrong body while ACCEPTING a nested-generic-call
            // body that merely leaked a rigid `Ty::Param` in the prepass. When the expected return is a
            // still-free `[U]` it is non-concrete here and deferred to the loop-back (no contract yet).
            if is_bare_closure
                && let (Ty::Func { ret: want_ret, .. }, Ty::Func { ret: got_ret, .. }) =
                    (&want, &refined)
                && ty_fully_concrete(want_ret)
                && !self.assignable(want_ret, got_ret)
            {
                self.error(
                    arg.span,
                    format!("closure argument to '{name}' returns {got_ret}, expected {want_ret}"),
                );
            }
            // LOOP-BACK (INTERLEAVED per-arg): a closure re-inferred WITH its expected param types has a
            // concrete return (`fn(int) -> int`); feed it straight back into a SECOND `unify` NOW, before
            // the NEXT arg's `want` is substituted. `unify` only binds a param NOT already in the map and
            // IGNORES an `Unknown` actual, so pass-1-resolved params are a strict no-op — it ONLY fills a
            // return-position param still free after pass 1 (recovered from the closure body). Interleaving
            // (vs a separate post-loop pass) is load-bearing for SOUNDNESS: once one closure binds a
            // return-only `[U]`, a SIBLING closure binding the SAME `[U]` to a CONFLICTING type sees `want`
            // now CONCRETE and is REJECTED by the soundness check above, instead of being silently dropped
            // by only-bind-unbound `unify` (adversarial-review bug 2: `two(fn(x): x*2, fn(x): str(x))`).
            unify(decl, &refined, map);
        }
        // Enforce bounds for params NEWLY bound by the loop-back only (pass-1-bound params were already
        // enforced by the caller — each enforced exactly once avoids a double-report). So a
        // `[U: Add]`/`[U: Comparable]` recovered from a closure body still has its bound checked.
        let newly_bound: Vec<TypeParam> = tps
            .iter()
            .filter(|tp| !bound_after_pass1.contains(&tp.name) && map.contains_key(&tp.name))
            .cloned()
            .collect();
        self.enforce_bounds(&newly_bound, map, span);
        // Degrade a STILL-unbound type param to `Unknown` ONLY when it appears in a PARAMETER position —
        // and ONLY on the method path (`degrade_unbound_param_pos`). It was in principle recoverable
        // from an argument, but that argument's relevant type was itself `Unknown` (the empty-collection
        // case, `[].map(fn(x): x*2)`), so degrading yields `List[?]` rather than a leaked `List[U]`. A
        // param appearing ONLY in the RETURN position and in NO parameter is genuinely un-inferable
        // (`fn make[U]() -> U`); it must stay a leaked `Ty::Param` so `assignable` rejects a concrete
        // assignment. On the FREE-FN path the whole degrade is skipped: `infer_generic_call` never
        // degraded, so a param-position free-fn type param left unbound by an empty-collection arg
        // (`first([])`) must stay a leaked `Ty::Param` too — degrading it laundered a clean compile
        // error into a runtime panic and silently suppressed the Category-2 construction-site diagnostic.
        if !degrade_unbound_param_pos {
            return;
        }
        let wanted: std::collections::HashSet<String> =
            tps.iter().map(|tp| tp.name.clone()).collect();
        let mut in_param_pos: Vec<String> = Vec::new();
        for p in all_params {
            ty_collect_params(p, &wanted, &mut in_param_pos);
        }
        for tp in tps {
            if in_param_pos.contains(&tp.name) {
                map.entry(tp.name.clone()).or_insert(Ty::Unknown);
            }
        }
    }
}

/// Does `ty` mention the built-in `Error` protocol existential anywhere (the E side of a bare `T!`,
/// or nested inside a container/struct)? Drives the concrete-error-type hint on a non-sendable
/// channel element. Matches `Error` by name — it is a reserved protocol (`prebuilt_protocols`), so a
/// user protocol can never shadow it.
fn ty_mentions_error_existential(ty: &Ty) -> bool {
    match ty {
        Ty::Protocol(n, _) => n == "Error",
        Ty::List(t)
        | Ty::Set(t)
        | Ty::Option(t)
        | Ty::Channel(t)
        | Ty::Shared(t)
        | Ty::Atomic(t)
        | Ty::RwShared(t) => ty_mentions_error_existential(t),
        Ty::Map(a, b) | Ty::Result(a, b) => {
            ty_mentions_error_existential(a) || ty_mentions_error_existential(b)
        }
        Ty::Tuple(xs) | Ty::Struct(_, xs) | Ty::Enum(_, xs) | Ty::NewType(_, xs) => {
            xs.iter().any(ty_mentions_error_existential)
        }
        _ => false,
    }
}
