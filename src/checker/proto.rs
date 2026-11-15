// checker::proto — split out of checker/mod.rs. `super::*` == the `checker` module.
// Protocol hoisting/embedding, satisfies, receiver refinement, hashability.

use super::*;
use std::collections::{HashMap, HashSet};

pub use crate::base::intrinsic::{INTRINSIC_PROTO_METHODS, INTRINSIC_UNPAIRED, RecvKind};

/// Proof that a conformance decision went through one of [`Checker::satisfies_args_d`]'s documented
/// grant paths — the compile-time half of the W6-3 ratchet. The field is private to this module, so a
/// new early-out CANNOT be written as a bare `return Ok(())`: the author must pick
/// [`Checker::grant_intrinsic`] (which registers the grant against [`INTRINSIC_PROTO_METHODS`]) or
/// [`Grant::no_intrinsic_method`], and reading either doc surfaces the pairing obligation. Carries no
/// data — conformance itself is still the `Ok`/`Err` discriminant.
#[must_use]
pub struct Grant(());

impl Grant {
    /// This conformance grants NO intrinsically-callable method, so it needs no runtime arm and no
    /// [`INTRINSIC_PROTO_METHODS`] row. Valid ONLY when the method the caller will go on to invoke is
    /// a real user method or nothing at all: the `Ty::Unknown` don't-cascade guard, an empty
    /// (top-type) protocol, a pure-embed bundle (each embed grants its own methods and is registered
    /// separately), a `Ty::Param` forwarding to its declared bounds, a protocol existential matching
    /// itself, structural satisfaction via a user method table, and — a FOURTH case, TICKET-024,
    /// W8-32 — a built-in (`List`/`Map`/`Set`/`str`/`bytes`/`bytearray`, or a scalar with no method
    /// table) witnessing a USER protocol out of its own harvested `native struct` method table
    /// (`Checker::satisfies_native`). That fourth case is still the right constructor even though the
    /// method IS callable: the method is a real native `fn` the direct-call path already type-checks
    /// and the VM already dispatches by name, so it needs no NEW runtime arm and no
    /// `INTRINSIC_PROTO_METHODS` row — unlike this table's rows, it carries no promise about a method
    /// with no code behind it.
    ///
    /// If the arm you are writing makes a BUILT-IN satisfy a RESERVED protocol with no user method
    /// behind it, this is the WRONG constructor — use [`Checker::grant_intrinsic`] and add the row.
    pub(super) fn no_intrinsic_method() -> Self {
        Grant(())
    }
}

/// Why a method is not the hook ([`Checker::hook_verdict`]).
pub(super) enum HookMiss {
    /// The type declares no method of that name.
    Absent,
    /// The method has no receiver: a static method is never a hook, and never a near miss.
    Static,
    /// A receiver-first method whose shape the requirement rejects; the decider's reason.
    Shape(String),
    /// The method is the hook of its declaration, and this instantiation does not meet it: it
    /// fails the method's own `where` bound, and the text names the bound. The hook table has
    /// one bit per declared type, so the run time calls the method for every instantiation: an
    /// operator on this one is a check error, never the hookless path.
    Bound(String),
}

/// Why one requirement of a protocol is not met ([`Checker::requirement_matches`]).
pub(super) enum ReqMiss {
    /// The method is missing or its signature does not match; the full message.
    Shape(String),
    /// The signature matches and the receiver's type argument fails the method's `where` bound.
    Bound {
        ty: String,
        protocol: String,
        why: String,
    },
}

impl ReqMiss {
    pub(super) fn text(self) -> String {
        match self {
            ReqMiss::Shape(text) => text,
            ReqMiss::Bound { ty, protocol, why } => {
                format!("type {ty} does not satisfy {protocol} ({why})")
            }
        }
    }
}

impl Checker {
    /// The receiver-KIND key [`INTRINSIC_PROTO_METHODS`] is indexed by — the granted `Ty`'s built-in
    /// shape, at the granularity the runtime dispatches on. `None` for anything unclassified, which
    /// matches no row and therefore trips `grant_intrinsic`'s assert (classify it and add its rows
    /// rather than widening this catch-all).
    pub(super) fn intrinsic_recv_kind(ty: &Ty) -> Option<RecvKind> {
        match ty.scalar() {
            Ty::Int => Some(RecvKind::Int),
            Ty::Float => Some(RecvKind::Float),
            Ty::Bool => Some(RecvKind::Bool),
            Ty::Str => Some(RecvKind::Str),
            Ty::Bytes => Some(RecvKind::Bytes),
            Ty::ByteArray => Some(RecvKind::ByteArray),
            Ty::Nil => None,
            Ty::List(_) => Some(RecvKind::List),
            Ty::Map(_, _) => Some(RecvKind::Map),
            Ty::Set(_) => Some(RecvKind::Set),
            Ty::Tuple(_) => Some(RecvKind::Tuple),
            Ty::Struct(n, a) if BuiltinType::is_cursor(n, a) => Some(RecvKind::Cursor),
            Ty::Struct(..) => Some(RecvKind::Struct),
            Ty::Enum(..) => Some(RecvKind::Enum),
            // `Option`/`Result` are their OWN `Ty`s but ONE runtime shape with `enum` (`Obj::Enum`);
            // they are kinds of their own here because the ratchet keys the CHECKER's grant set, and
            // the checker distinguishes them.
            Ty::Option(_) => Some(RecvKind::Option),
            Ty::Result(..) => Some(RecvKind::Result),
            // A builtin fn value (`ord`/`chr`/`panic`/`print`) is a `Ty::Func` too; at run time it
            // compares by `Obj::Builtin` name-equality (`vm/arith.rs`).
            Ty::Func { .. } => Some(RecvKind::Func),
            // A protocol EXISTENTIAL — at runtime this is always its concrete witness object, so
            // `.eq()`/`==`/`[T: Eq]` on it defers to whatever the witness's own `eq` dispatch does
            // (structural derive or a declared `eq`), exactly as Go's interface `==` defers to the
            // dynamic type and panics if that type is uncomparable (W7-52). Classified ONLY so the D1
            // `Eq` arm below can grant it — every OTHER protocol row in `INTRINSIC_PROTO_METHODS`
            // still excludes `"protocol"` (no row registered), so this does not widen anything but Eq.
            Ty::Protocol(..) => Some(RecvKind::Protocol),
            Ty::Channel(_) => Some(RecvKind::Channel),
            Ty::Shared(_) => Some(RecvKind::Shared),
            Ty::RwShared(_) => Some(RecvKind::RwShared),
            Ty::Atomic(_) | Ty::AtomicInt => Some(RecvKind::Atomic),
            Ty::Executor | Ty::Socket | Ty::Listener | Ty::Writer | Ty::Reader | Ty::Ptr => {
                Some(RecvKind::Handle)
            }
            Ty::Param(_) | Ty::Width(_) | Ty::Module(_) | Ty::Unknown | Ty::Var(_) => None,
        }
    }

    /// The `self.structs` key a built-in's harvested `native struct` method table sits under, for
    /// the ONE receiver kinds a USER protocol may witness against (TICKET-024, W8-32) — `None` for
    /// everything else (handles, tuples, `Option`/`Result`, functions), which keeps falling to the
    /// catch-all message. The three scalars have no `self.structs` entry (`std/prelude.chz:29-31`
    /// declares them `native ctor` only), so routing them here anyway reaches an empty table and
    /// IMPROVES the message: "type int does not satisfy Sized (missing method 'len')" instead of
    /// the bare clause.
    pub(super) fn native_witness_key(ty: &Ty) -> Option<&'static str> {
        let b = BuiltinType::of(ty)?;
        match b {
            BuiltinType::List
            | BuiltinType::Map
            | BuiltinType::Set
            | BuiltinType::Str
            | BuiltinType::Bytes
            | BuiltinType::ByteArray
            | BuiltinType::Int
            | BuiltinType::Float
            | BuiltinType::Bool => Some(b.name()),
            BuiltinType::Iterator
            | BuiltinType::Channel
            | BuiltinType::Range
            | BuiltinType::Socket
            | BuiltinType::Listener
            | BuiltinType::Writer
            | BuiltinType::Reader
            | BuiltinType::Ptr
            | BuiltinType::OwnedStr
            | BuiltinType::Shared
            | BuiltinType::RwShared
            | BuiltinType::Atomic
            | BuiltinType::AtomicInt
            | BuiltinType::Executor => None,
        }
    }

    /// The BUILT-IN cursor shape — `Ty::Struct("Iterator", [E])`, what `.iter()` mints and a
    /// generator returns. It is a `Ty::Struct` but NOT a struct at runtime (`Obj::Iterator` /
    /// `Obj::Generator`), and its dispatch arms deliberately expose only `next`/`iter`, so a
    /// protocol grant keyed on `"struct"` would be check-OK-then-`has no method` for it.
    pub(super) fn is_cursor_ty(ty: &Ty) -> bool {
        matches!(ty, Ty::Struct(n, a) if BuiltinType::is_cursor(n, a))
    }

    /// Does `INTRINSIC_PROTO_METHODS` hold the row `(protocol, kind of ty)`?
    pub(super) fn has_intrinsic_row(protocol: &str, ty: &Ty) -> bool {
        let kind = Self::intrinsic_recv_kind(ty);
        INTRINSIC_PROTO_METHODS
            .iter()
            .any(|(p, k)| *p == protocol && Some(*k) == kind)
    }

    /// The method `method` that `ty` has with no declaration behind it (wave 24 F4): the
    /// requirement of a std protocol with no type parameters (`Stringable.str`, `Eq.eq`,
    /// `Hashable.hash`, `Comparable.compare`, `Add.add`, …) that `ty` satisfies with an
    /// [`INTRINSIC_PROTO_METHODS`] row, `Self` read as `ty`. The row's promise is that the method
    /// is callable on `ty` at run time, so a direct call `(5).str()`, a bound and a user protocol
    /// of the same shape all read this one answer. A declared method of the name comes first at
    /// every reader; a protocol value offers only its protocol's methods (object safety), so it
    /// has none here.
    ///
    /// The other half: a `native fn` default of a std protocol `ty` satisfies (`Error.line`), which
    /// the runtime answers for every receiver.
    pub(super) fn intrinsic_method(&self, ty: &Ty, method: &str) -> Option<FnSig> {
        let ty = ty.scalar();
        let kind = Self::intrinsic_recv_kind(ty).filter(|k| *k != RecvKind::Protocol)?;
        let row = INTRINSIC_PROTO_METHODS
            .iter()
            .filter(|(_, k)| *k == kind)
            .find_map(|(protocol, _)| {
                let pinfo = self.protocol_shape(protocol)?;
                let (_, sig) = pinfo.methods.iter().find(|(n, _)| n == method)?;
                self.protocol_sig_for(ty, protocol, pinfo, sig)
            });
        row.or_else(|| {
            self.protocols.iter().find_map(|(protocol, pinfo)| {
                let (_, sig) = pinfo.defaults.iter().find(|(n, _)| n == method)?;
                self.protocol_sig_for(ty, protocol, pinfo, sig)
            })
        })
    }

    /// `sig`, a method of the type-parameter-free `protocol`, as `ty`'s own method, when `ty`
    /// satisfies `protocol`: `Self` and the receiver read as `ty`.
    fn protocol_sig_for(
        &self,
        ty: &Ty,
        protocol: &str,
        pinfo: &ProtocolInfo,
        sig: &FnSig,
    ) -> Option<FnSig> {
        if !pinfo.type_params.is_empty() {
            return None;
        }
        self.satisfies(ty, protocol).ok()?;
        let map = HashMap::from([("Self".to_string(), ty.clone())]);
        let mut sig = subst_sig(sig, &map);
        if let Some(recv) = sig.params.first_mut() {
            *recv = ty.clone();
        }
        Some(sig)
    }

    /// Does `ty`'s declaration name a member `name`: a method of the user struct or enum, a field
    /// (which shadows an intrinsic method of the name, as it would a declared one), or a method of
    /// the builtin's std `native struct` decl? The declared half of the method set; the intrinsic
    /// half ([`Self::intrinsic_method`]) answers only when this is `false`.
    pub(super) fn declares_member(&self, ty: &Ty, name: &str) -> bool {
        let ty = ty.scalar();
        if let Some(methods) = self.nominal_method_table(ty) {
            return methods.contains_key(name) || self.data_member_ty(ty, name).is_some();
        }
        BuiltinType::of(ty)
            .and_then(|b| self.struct_shape(b.name()))
            .is_some_and(|info| info.methods.contains_key(name))
    }

    /// Every method name of `ty`'s method set, declared and intrinsic, sorted: the near-miss
    /// candidates of a method-call miss.
    pub(super) fn method_set_names(&self, ty: &Ty) -> Vec<String> {
        let scalar = ty.scalar();
        let mut names: Vec<String> = match self.nominal_method_table(scalar) {
            Some(methods) => methods.keys().cloned().collect(),
            None => BuiltinType::of(scalar)
                .and_then(|b| self.struct_shape(b.name()))
                .map(|info| info.methods.keys().cloned().collect())
                .unwrap_or_default(),
        };
        names.extend(self.intrinsic_method_names(ty));
        names.sort();
        names.dedup();
        names
    }

    /// The names [`Self::intrinsic_method`] answers for `ty`, for a miss's near-miss help.
    pub(super) fn intrinsic_method_names(&self, ty: &Ty) -> Vec<String> {
        let mut names: Vec<String> = INTRINSIC_PROTO_METHODS
            .iter()
            .filter_map(|(protocol, _)| self.protocol_shape(protocol))
            .flat_map(|p| p.methods.iter().map(|(n, _)| n.clone()))
            .chain(
                self.protocols
                    .values()
                    .flat_map(|p| p.defaults.iter().map(|(n, _)| n.clone())),
            )
            .filter(|n| self.intrinsic_method(ty, n).is_some())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Grant `ty` conformance to `protocol` INTRINSICALLY (no user method) — the single funnel every
    /// intrinsic early-out in [`Checker::satisfies_args_d`] returns through. It exists for the
    /// [`INTRINSIC_PROTO_METHODS`] `debug_assert`: granting a `(protocol, receiver-kind)` pair whose
    /// method is not callable at runtime trips here in the test suite instead of shipping a
    /// check-OK-then-run-fault. Behaviorally it is `Ok(_)` (the assert is debug-only), and it does NOT
    /// decide conformance — every caller already did.
    pub(super) fn grant_intrinsic(&self, protocol: &str, ty: &Ty) -> Result<Grant, String> {
        let kind = Self::intrinsic_recv_kind(ty);
        debug_assert!(
            INTRINSIC_PROTO_METHODS
                .iter()
                .chain(INTRINSIC_UNPAIRED)
                .any(|(p, k)| *p == protocol && Some(*k) == kind),
            "intrinsic conformance granted for ({protocol}, {kind:?}) with no row in \
             INTRINSIC_PROTO_METHODS — add the (protocol, kind) row AND make its methods \
             callable at runtime (`Vm::intrinsic_proto_method`), or register the row in \
             INTRINSIC_UNPAIRED (W6-3)"
        );
        Ok(Grant::no_intrinsic_method())
    }

    /// The methods of a `protocol` declaration, resolved with `Self` and the protocol's own type
    /// params in scope: its requirements, then its `native fn` defaults (the runtime answers them for
    /// every type that satisfies it, so they are not requirements). A `native fn` line is std-only,
    /// like every `native` decl. `hoist_protocol` and `harvest_protocol_stmt` both call it.
    pub(super) fn protocol_method_sigs(
        &mut self,
        methods: &[MethodSig],
        span: Span,
    ) -> ProtocolSigs {
        if !self.current_module_is_stdlib && methods.iter().any(|m| m.native) {
            self.error(
                span,
                "native fn declarations are only allowed in standard-library modules".to_string(),
            );
        }
        let (natives, required): (Vec<&MethodSig>, Vec<&MethodSig>) =
            methods.iter().partition(|m| m.native);
        let defaults = if self.current_module_is_stdlib {
            self.protocol_sigs_of(&natives, span)
        } else {
            Vec::new()
        };
        ProtocolSigs {
            required: self.protocol_sigs_of(&required, span),
            defaults,
        }
    }

    fn protocol_sigs_of(&mut self, methods: &[&MethodSig], span: Span) -> Vec<(String, FnSig)> {
        methods
            .iter()
            .map(|m| {
                let self_param = Ty::Param("Self".to_string());
                let receiver = self.receiver_or_report(&m.params, Some(&self_param), span);
                let params = m
                    .params
                    .iter()
                    .enumerate()
                    .map(|(i, p)| match &p.ty {
                        // The receiver slot stays untyped in a requirement: `method_matches`
                        // compares an implementor's operands, never its receiver.
                        _ if i == 0 && receiver.is_some() => Ty::Unknown,
                        Some(t) => self.resolve_type(t, span),
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
                    .map(|t| self.resolve_ret_type(t, span))
                    .unwrap_or(Ty::Nil);
                // A protocol method whose first param is NOT `self` (or which has no params) is a STATIC
                // (associated) requirement — mirrors `Checker::fn_sig`'s rule for concrete methods. This
                // is load-bearing for `From`-style static-ctor protocols: it drives both the
                // static-slot witnessing check (`method_matches`) and the bound-only value-position gate
                // (`protocol_has_static_method`). Ordinary `fn get(self, …)` requirements are instance
                // methods (`is_static == false`), unchanged.
                let mut sig = FnSig::plain(params, ret);
                sig.is_static = receiver.is_none();
                // TICKET-187: a protocol's parameter names are part of its contract — a call through
                // the protocol binds by them, and an implementor conforms only with the same names.
                sig.labels = protocol_labels(&m.params);
                (m.name.clone(), sig)
            })
            .collect()
    }

    /// Register a `protocol` declaration's method signatures. `Self` resolves to `Ty::Param("Self")`.
    pub(super) fn hoist_protocol(
        &mut self,
        name: &str,
        type_params: &[TypeParam],
        methods: &[MethodSig],
        embeds: &[Bound],
        span: Span,
    ) {
        if is_std_protocol(name) && self.current_module_is_stdlib {
            // A std protocol's SHAPE comes from its `protocol` decl in `std/*.chz`, harvested once by
            // `seed_std_protocols`. In a stdlib module the decl is a no-op here. A USER module may
            // declare a protocol of a builtin's name: only keywords are reserved, and the user
            // protocol shadows the builtin in its module.
            return;
        }
        let key = self.bare_key(name);
        let mut saved = self.type_params.clone();
        std::mem::swap(&mut self.type_params, &mut saved); // start clean, with only Self visible
        self.type_params.insert("Self".to_string(), Vec::new());
        // The protocol's own type params are in scope while resolving its method signatures, so
        // `fn get(self, i: int) -> T` resolves `T` to `Ty::Param("T")`.
        let resolved = self.resolve_bounds(type_params, span);
        for (tp, rtp) in type_params.iter().zip(resolved) {
            if tp.name == "Self" {
                self.error(
                    span,
                    "protocol type parameter cannot be named 'Self'".to_string(),
                );
            }
            self.type_params.insert(tp.name.clone(), rtp.bounds);
        }
        self.check_param_bounds(type_params, span);
        let sigs = self.protocol_method_sigs(methods, span);
        self.type_params = saved; // restore
        self.protocols.insert(
            key,
            ProtocolInfo {
                type_params: type_params.iter().map(|tp| tp.name.clone()).collect(),
                methods: std::sync::Arc::new(sigs.required),
                defaults: std::sync::Arc::new(sigs.defaults),
                // Names only: an embed arg may name a protocol declared LATER, so
                // `resolve_protocol_embeds` fills the args once every protocol is hoisted, before
                // any reader runs (TICKET-202).
                embeds: embeds
                    .iter()
                    .map(|b| TyBound {
                        name: self
                            .user_protocol_key(&b.name)
                            .unwrap_or_else(|| b.name.clone()),
                        args: Vec::new(),
                    })
                    .collect(),
            },
        );
    }

    /// TICKET-202 — resolve a protocol's embed args ONCE, in the owner protocol's scope (its own
    /// params only), after every protocol is hoisted. Silent: `validate_protocol_embeds` reports an
    /// unknown name in its own words. A use site never re-resolves an embed.
    pub(super) fn resolve_protocol_embeds(
        &mut self,
        name: &str,
        type_params: &[TypeParam],
        embeds: &[Bound],
        span: Span,
    ) {
        // The prelude's mirror of a builtin protocol (a std module); a user protocol of that name is
        // its own protocol.
        if is_std_protocol(name) && self.current_module_is_stdlib {
            return;
        }
        let own: HashMap<String, Vec<TyBound>> = type_params
            .iter()
            .map(|tp| (tp.name.clone(), Vec::new()))
            .collect();
        let saved = std::mem::replace(&mut self.type_params, own);
        let m = self.diag_mark();
        let r: Vec<TyBound> = embeds.iter().map(|b| self.resolve_bound(b, span)).collect();
        self.diag_rollback(m);
        self.type_params = saved;
        let key = self.bare_key(name);
        if let Some(p) = self.protocols.get_mut(&key) {
            p.embeds = r;
        }
    }

    /// M22 — validate a protocol's embeds AFTER every protocol is hoisted (so forward/cyclic refs
    /// resolve). Three LOCKED rules: (1) an OWN `fn` whose name collides with any transitively-embedded
    /// required method → error, regardless of order or sig; (2) two embeds pulling the same method name
    /// with DIFFERING signatures → error (identical sigs dedup silently — the legal diamond); (3) a
    /// cyclic embed (A embeds B, B embeds A) → error. Built-in protocols (registered via
    /// `seed_std_protocols`, not here) are trusted and skipped.
    pub(super) fn validate_protocol_embeds(
        &mut self,
        name: &str,
        type_params: &[TypeParam],
        methods: &[MethodSig],
        embeds: &[Bound],
        span: Span,
    ) {
        if is_std_protocol(name) && self.current_module_is_stdlib {
            return; // the prelude's mirror of a builtin protocol
        }
        // Each embed must name a real protocol.
        for emb in embeds {
            if self.user_protocol_shape(&emb.name).is_none() {
                let msg = self.unknown_protocol_msg(&emb.name, &format!("embedded in '{name}'"));
                self.error(span, msg);
            }
        }
        let own: Vec<String> = type_params.iter().map(|tp| tp.name.clone()).collect();
        for emb in embeds {
            for a in &emb.args {
                // An embed arg naming a type that does not exist (`Contains[T]` where the protocol
                // declares no `T`) resolves to `Ty::Unknown`, and an `Unknown` element type accepts
                // EVERY operand — `"oops" in b` type-checked on a `Bag` whose `contains` takes an
                // `int`, then faulted. Unknown-as-permissive must be a hard error, not a silently
                // wide requirement. Every struct/enum/alias/protocol is hoisted before this runs, so
                // an unresolvable name here really is unresolvable.
                if let Some(bad) = first_unresolvable_name(a, &own, &|n| {
                    !self.resolve_ty_ro(&Type::named(n)).is_unknown()
                }) {
                    self.error(
                        span,
                        format!(
                            "unknown type '{bad}' in the type argument of embedded protocol '{}' \
                             in '{name}'",
                            emb.name
                        ),
                    );
                }
            }
        }
        // Flatten the transitive embed method set, detecting cycles + cross-embed signature conflicts.
        // The walk reads the RESOLVED (keyed) embeds, so the path starts at this protocol's key.
        let resolved = self
            .protocol_shape(name)
            .map(|p| p.embeds.clone())
            .unwrap_or_default();
        let mut path = vec![self.bare_key(name)];
        let (required, cyclic, conflict) = self.flatten_embed_methods(&resolved, &mut path);
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
        embeds: &[TyBound],
        path: &mut Vec<String>,
    ) -> (HashMap<String, FnSig>, bool, Option<String>) {
        self.flatten_embed_methods_seen(embeds, path, &mut HashSet::new())
    }

    /// `seen` is what bounds the walk. `path` detects a CYCLE (a name revisited on the current
    /// branch) but does nothing about SHARING: a DAG where each protocol embeds two others re-walks
    /// every shared subtree once per route, which is exponential — a 42-protocol chain of
    /// `Pi: P(i+1), P(i+2)` hung `check` (and the LSP) past 25 s on the DECLARATION alone, before any
    /// use. Skipping an already-walked protocol is result-preserving: its methods are already merged
    /// into `required`, and re-merging identical signatures is what the diamond rule already dedups.
    /// The `path` check stays FIRST so a cycle is still reported rather than silently skipped.
    fn flatten_embed_methods_seen(
        &self,
        embeds: &[TyBound],
        path: &mut Vec<String>,
        seen: &mut HashSet<String>,
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
            let Some(pinfo) = self.protocol_shape(&emb.name).cloned() else {
                continue; // unknown embed already errored in validate_protocol_embeds
            };
            if !seen.insert(emb.name.clone()) {
                continue; // already merged on this walk — a shared subtree, not new work
            }
            for (mn, ms) in pinfo.methods.iter() {
                merge(mn, ms, &mut conflict);
            }
            path.push(emb.name.clone());
            let (sub, cyclic, sub_conf) =
                self.flatten_embed_methods_seen(&pinfo.embeds, path, seen);
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

    /// M22 — resolve `method` on protocol `pname`: OWN methods first, then transitively through the
    /// embeds, substituting each embed's args into the pulled-in signature so it speaks the OUTER
    /// protocol's type-param vocabulary (`protocol Bag[T]: Contains[T]` ⇒ `contains(self, T) -> bool`,
    /// so a `Bag[int]` receiver witnesses `int`). This is what makes an embedded method callable
    /// through an interface value and through a bound — `spec.md`'s "flattened at every use site".
    ///
    /// Deliberately NOT built on [`Self::flatten_embed_methods`]: that one builds the whole map and
    /// does not substitute `Bound.args` (its callers only inspect `is_static`), which would type an
    /// embedded method in the EMBEDDED protocol's vocabulary. Cycle-guarded like `protocol_provides` —
    /// a cyclic embed is rejected at declare time but still reaches here after erroring.
    pub(super) fn protocol_method_sig(&self, pname: &str, method: &str) -> Option<FnSig> {
        self.protocol_method_sig_d(pname, method, &mut HashSet::new())
    }

    /// `seen` is what makes this terminate, not a depth cap: with branching ≥ 2 a depth bound of 64
    /// is 2^64 visits, so a diamond DAG (or a cyclic decl, which is an error but still reaches here)
    /// hangs `check` and the LSP on a MISS — every embed gets explored before `None` is returned.
    /// Visiting each protocol once is also semantically free: the first hit wins either way.
    fn protocol_method_sig_d(
        &self,
        pname: &str,
        method: &str,
        seen: &mut HashSet<String>,
    ) -> Option<FnSig> {
        if !seen.insert(pname.to_string()) {
            return None;
        }
        let pinfo = self.protocol_shape(pname)?;
        if let Some((_, sig)) = pinfo
            .methods
            .iter()
            .chain(pinfo.defaults.iter())
            .find(|(n, _)| n == method)
        {
            return Some(sig.clone());
        }
        for emb in &pinfo.embeds {
            let Some(sig) = self.protocol_method_sig_d(&emb.name, method, seen) else {
                continue;
            };
            // The recovered sig is spelled in `emb.name`'s params; re-spell it in ours.
            let etps = self
                .protocol_shape(&emb.name)
                .map(|p| p.type_params.clone())
                .unwrap_or_default();
            let map: HashMap<String, Ty> = etps.into_iter().zip(emb.args.clone()).collect();
            return Some(subst_sig(&sig, &map));
        }
        None
    }

    /// The method names a protocol requires, own OR through any embed, flattened. Copies
    /// `protocol_method_sig_d`'s `seen` cycle guard so an embed cycle can't recurse forever.
    pub(super) fn protocol_method_names(&self, pname: &str) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        self.protocol_method_names_d(pname, &mut seen, &mut out);
        out.sort();
        out.dedup();
        out
    }

    fn protocol_method_names_d(
        &self,
        pname: &str,
        seen: &mut HashSet<String>,
        out: &mut Vec<String>,
    ) {
        if !seen.insert(pname.to_string()) {
            return;
        }
        let Some(pinfo) = self.protocol_shape(pname) else {
            return;
        };
        out.extend(
            pinfo
                .methods
                .iter()
                .chain(pinfo.defaults.iter())
                .map(|(n, _)| n.clone()),
        );
        for emb in &pinfo.embeds {
            self.protocol_method_names_d(&emb.name, seen, out);
        }
    }

    /// M22 + object safety — the name of a method this protocol requires, own OR through any embed,
    /// whose signature takes `Self`. `Some(name)` ⇒ no existential can be a witness for it.
    ///
    /// The flattened set is the point: `protocol Vecish: Add` has NO own method taking `Self` — the
    /// `add(self, o: Self) -> Self` arrives through the embed — so an own-methods-only check let the
    /// commonest spelling of the hazard straight through. Embed-arg substitution is irrelevant here
    /// (`Self` is not a protocol type param, so no substitution can introduce or remove it), which
    /// is why `flatten_embed_methods` is enough and the re-spelling walk is not needed.
    pub(super) fn protocol_self_param_method(&self, p: &str) -> Option<String> {
        let pinfo = self.protocol_shape(p)?;
        if let Some((n, _)) = pinfo
            .methods
            .iter()
            .find(|(_, s)| self_in_param_position(s))
        {
            return Some(n.clone());
        }
        let mut path = vec![p.to_string()];
        let (required, _cyclic, _conflict) = self.flatten_embed_methods(&pinfo.embeds, &mut path);
        let mut hits: Vec<&String> = required
            .iter()
            .filter(|(_, s)| self_in_param_position(s))
            .map(|(n, _)| n)
            .collect();
        hits.sort(); // `required` is a HashMap — pick deterministically so the diagnostic is stable
        hits.first().map(|n| (*n).clone())
    }
}

/// TICKET-187 — a protocol method's parameter labels, parallel to its `params` (`None` for the
/// `self` receiver). The one builder for user protocols (`hoist_protocol`) and the std ones
/// declared in `std/*.chz` (`harvest_protocol_stmt`).
pub(super) fn protocol_labels(params: &[crate::ast::Param]) -> Vec<Option<String>> {
    params
        .iter()
        .map(|p| (!p.is_self()).then(|| p.name.clone()))
        .collect()
}

/// The names of `tps` with no binding in `map` yet: a call's still-free params, which never stand
/// as a caller's rigid param ([`Checker::rigid_param`]).
pub(super) fn unbound_params(tps: &[TyParam], map: &HashMap<String, Ty>) -> Vec<String> {
    tps.iter()
        .filter(|tp| !map.contains_key(&tp.name))
        .map(|tp| tp.name.clone())
        .collect()
}
