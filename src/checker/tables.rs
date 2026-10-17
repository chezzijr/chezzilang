//! Checker: obligations, the operator `HOOKS` table, checker→compiler table types, witness and
//! carrier keys, entry-module bindings.
//! Split out of `src/checker/mod.rs`. `super::*` is the parent module.

use super::*;

/// TICKET-250 (D4) — a conformance question a type spelling asks. `hoist` resolves a type's
/// field and method types before every type of the module is inserted, so inside `hoist` the
/// question is deferred (`Checker::conformance_obligation`) and discharged once they all are.
pub(super) enum Obligation {
    /// `Map[K, _]` / `Set[E]`: the key type must be `Hashable`. `what` opens the message.
    Hashable {
        ty: Ty,
        what: &'static str,
        span: Span,
    },
    /// `Name[args]`: arity, and each argument against its parameter's bounds.
    Bound {
        name: String,
        key: String,
        is_struct: bool,
        args: Vec<Ty>,
        span: Span,
    },
    /// `Atomic[T]`: the payload must compare structurally.
    AtomicPayload { elem: Ty, span: Span },
    /// An extern parameter or return type must be C-marshallable.
    ExternMarshal {
        ty: Ty,
        name: String,
        span: Span,
        allow_void: bool,
    },
}

/// A deferred [`Obligation`] with the type-parameter scope it was asked in.
pub(super) type DeferredObligation = (Obligation, HashMap<String, Vec<TyBound>>);

/// Per declared type, keyed `(graph module index, type key)` like the witness table's members:
/// which methods are conforming hooks. The only channel that carries hook-ness to the runtime.
pub type HookTable = HashMap<(usize, String), HookSet>;

/// Which method-call sites are typed through a reserved protocol's requirement (`s.iter()` in a
/// `[S: Iterable[T]]` body or on an `Iterable[T]` slot), keyed by [`carrier_key`] on the
/// method-name token. The compiler lowers each to `Op::CallHook`.
pub type HookCallTable = HashMap<CarrierKey, Hook>;

/// TICKET-262 — the C width of each field of every struct that has one, keyed by the struct's runtime
/// identity key (`Checker::type_key`), one entry per field in declaration order (`None` = no width).
/// Built from the resolved field `Ty` (`Ty::width`), so the compiler never re-reads a field's spelling;
/// the VM range-checks each store into such a field with `CType::check_fits`. A struct with no width
/// field has no entry.
pub type FieldWidthTable = HashMap<String, Vec<Option<CType>>>;

/// Every checker-to-compiler side table [`resolve_call_tables`] returns, in this order.
pub type CallTables = (
    CallPlanTable,
    WitnessTable,
    CarrierTable,
    SumSeedTable,
    RetCoerceTable,
    TableConflicts,
    CrossingTable,
    ResolutionTable,
    FallOffTable,
    GenCrossings,
    WrapTable,
    HookTable,
    HookCallTable,
    FieldWidthTable,
);

/// W7-49 — side-table keys asked to hold two different decisions, as `(span, message)`. Empty for
/// every well-formed program; a non-empty one is a hard compile error, never a warning.
pub type TableConflicts = Vec<(Span, String)>;

/// Resolve value-keyword calls for a SINGLE-FILE (standalone, source-string) program, through the
/// EXACT SAME [`resolve_call_tables`] pass as the multi-file CLI (one resolver, one consumer,
/// module index `0`). Wraps `stmts` in a synthetic one-module graph, mirroring
/// [`resolve_extern_signatures_standalone`]. Test-only (the standalone compile/run paths are
/// `#[cfg(test)]`; production always goes through `build_graph`).
#[cfg(any(test, feature = "test-api"))]
pub fn resolve_call_tables_standalone(stmts: &[Stmt]) -> CallTables {
    resolve_call_tables_with(&standalone_graph(stmts), true)
}

/// [`resolve_call_tables_standalone`] with the nested-fn return memo off — the twin
/// `the_nested_fn_ret_memo_is_invisible_to_diagnostics_and_tables` compares against.
#[cfg(any(test, feature = "test-api"))]
pub fn resolve_call_tables_standalone_no_memo(stmts: &[Stmt]) -> CallTables {
    resolve_call_tables_with(&standalone_graph(stmts), false)
}

/// The synthetic one-module graph both standalone entry points wrap `stmts` in.
#[cfg(any(test, feature = "test-api"))]
pub fn standalone_graph(stmts: &[Stmt]) -> ModuleGraph {
    let id = crate::base::module_id::ModuleId(std::path::PathBuf::from("<main>"));
    ModuleGraph {
        entry: id.clone(),
        range_used: true,
        skipped_fns: std::collections::HashMap::new(),
        modules: vec![crate::resolver::LoadedModule {
            id,
            dotted: Vec::new(),
            ast: crate::ast::Module {
                stmts: stmts.to_vec(),
            },
            file: 0,
            imports: Vec::new(),
            native: None,
        }],
    }
}

/// W7-49 — record ONE entry into a checker→compiler side table, refusing to overwrite a key that is
/// already bound to a DIFFERENT value.
///
/// This is a BACKSTOP under an already-injective key ([`Span::file`] is what makes the key injective
/// across modules), not a substitute for one. It exists for the surviving residual: the same default
/// expression spliced twice into the SAME module keeps one set of spans, so both splices share one
/// key — and a default is cloned into the CALLER's scope, so a caller-side local can shadow the
/// definer's global and the two splices can genuinely resolve differently. Overwriting there applies
/// the second site's decision to the first: a silent wrong value under a green `chezzi check`.
/// Better loud than silent.
///
/// It compares VALUES, never mere presence: a same-key/same-value re-insert is NORMAL and benign
/// (two call sites omitting the same default, or one module re-checked), and a presence-checking
/// guard would reject every program with a same-module default.
pub(crate) fn record_call_table_entry<K, V>(
    map: &mut HashMap<K, V>,
    conflicts: &mut Vec<(Span, String)>,
    key: K,
    value: V,
    what: &str,
    span: Span,
) where
    K: std::hash::Hash + Eq,
    V: PartialEq,
{
    match map.get(&key) {
        // Same decision recorded twice — the benign, common case.
        Some(prev) if *prev == value => {}
        Some(_) => conflicts.push((
            span,
            format!(
                "internal: two different {what} decisions were recorded for one source position, so \
                 the backend cannot tell the two sites apart. This is a compiler bug — please report \
                 it with the source (docs/contributing/gaps.md W7-49). One trigger has a workaround: if a call here \
                 omits an argument that has a DEFAULT, the default is cloned into each caller's \
                 scope, and a caller-side local that shadows the definer's global makes two splices of \
                 it resolve differently — renaming that local avoids it"
            ),
        )),
        None => {
            map.insert(key, value);
        }
    }
}

/// M24 — build the [`WitnessKey`] for a call site that needs static-witness arguments: `(module,
/// fragment-context span, fragment ordinal, CALLEE-TOKEN span)`. The checker's record site and the
/// compiler's lookup site call this one helper so they can never disagree on the key. `key_span` is
/// always a [`witness_key_span`] result — the callee's own token, never the call node's span; see
/// [`WitnessKey`]. `frag_ctx`/`frag_ord` are the interpolation discriminators.
pub fn witness_key(
    module_idx: usize,
    frag_ctx: Span,
    frag_ord: usize,
    key_span: Span,
) -> crate::checker::WitnessKey {
    (module_idx, frag_ctx, frag_ord, key_span)
}

/// W7-43 — build the [`CarrierKey`] for a `?.` carrier: `(module, fragment-context span, fragment
/// ordinal, the OptChain's NAME-TOKEN span)`. The checker's record site and the compiler's lookup
/// site call this one helper so they can never disagree on the key. `name_span` is always the
/// carrier's own `name_span`, never its node span — see [`CarrierKey`] for why (a mixed
/// `Result`/`Option` chain shares one node span across links with DIFFERENT modes).
/// `frag_ctx`/`frag_ord` are the interpolation discriminators.
pub fn carrier_key(
    module_idx: usize,
    frag_ctx: Span,
    frag_ord: usize,
    name_span: Span,
) -> crate::checker::CarrierKey {
    (module_idx, frag_ctx, frag_ord, name_span)
}

/// W8-21 — build the [`RetCoerceTable`] key for one return-sink success-coercion decision: a plain
/// [`carrier_key`] on the returned value's own span. The checker's record site and the compiler's
/// lookup site call this one helper so they can never disagree on the key.
pub fn ret_coerce_key(
    module_idx: usize,
    frag_ctx: Span,
    frag_ord: usize,
    span: Span,
) -> crate::checker::CarrierKey {
    carrier_key(module_idx, frag_ctx, frag_ord, span)
}

/// M24 Task 5 — the span component of a call site's [`WitnessKey`], from the CALLEE and the call
/// node's span. ONE derivation, shared by the checker's record sites and the compiler's lookups.
///
/// The key is the CALLEE's own token, never the call node's span. A call node's span is shared by
/// every link of a postfix chain (`parse_postfix` gives each link the primary expression's span) AND
/// by every link of a pipe chain (`a |> f() |> g()` desugars at parse time to nested `Call`s that all
/// inherit the whole infix expression's span) — so keying on it aliases two distinct witness calls
/// onto one slot: the later insert wins and the earlier call silently constructs the WRONG type
/// under a green `chezzi check`. The callee token is a distinct source node per link in both shapes:
/// a bare `Ident` (`reset(c)`) keys on the identifier's span, a member callee — instance method
/// (`h.make(c)`), static method (`Holder.build(c)`), module member (`lib.reset(c)`) — on the
/// member-name TOKEN (`name_span`). It is a KEY ONLY: diagnostics still anchor on the call span (the
/// trap `49bd9f80` closed).
pub fn witness_key_span(callee: &Expr, call_span: Span) -> Span {
    match &callee.kind {
        ExprKind::Field { name_span, .. } => *name_span,
        ExprKind::Ident(_) => callee.span,
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
        | ExprKind::ElseGuard { .. } => call_span,
    }
}

/// Whether the entry module binds `name` as a module global at run time: one of the names
/// `resolver::module_global_binds` lists, the list the compiler builds the slot table from.
/// `Vm::invoke_entrypoint` looks the manifest entrypoint up in exactly that slot table, so a name
/// absent here can never be invoked (TICKET-150, W14-35c).
pub(super) fn entry_module_binds(stmts: &[Stmt], imports: &[ResolvedImport], name: &str) -> bool {
    crate::resolver::module_global_binds(imports, stmts, None)
        .iter()
        .any(|(bound, _)| bound == name)
}
