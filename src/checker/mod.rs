//! M4 — the type checker. A static pass between parse and run that catches type errors *before*
//! any code executes, collecting **all** errors (Go-style) rather than stopping at the first.
//!
//! Design: pragmatic local inference (see `ty.rs`). Explicit function signatures give us call
//! types for free; locals are inferred from their initializers. [`Ty::Unknown`] suppresses
//! cascades. Two passes: pass 1 hoists every top-level declaration (so forward references work,
//! matching how the compiler hoists top-level declarations); pass 2 walks bodies and accumulates errors.

mod ty;

use crate::ast::{
    AssignOp, BinaryOp, Block, Bound, CompClause, CompKind, DeferTarget, Expr, ExprKind, FnDecl,
    Import, LitPattern, MethodSig, NativeDecl, Param, Pattern, Span, SpawnTarget, Stmt, StmtKind,
    Type, TypeParam, UnaryOp, WaitArm, WaitArmKind,
};
use crate::base::hooks::{HOOKS, Hook, HookRow, HookSet};
use crate::base::module_id::{ModuleId, ResolvedImport};
use crate::native::cffi::CType;
use crate::resolver::ModuleGraph;
use std::collections::{HashMap, HashSet};
use std::fmt;

pub(crate) use crate::base::builtin::{Builtin, Lowering};
pub(crate) use builtin_type::BuiltinType;
pub use graph_pass::std_protocol_owner;
pub use ty::Ty;
pub use ty::{
    ArgFill, CallPlanTable, CarrierKey, CarrierMode, CarrierTable, FallOff, FallOffTable, FnLabels,
    Resolution, ResolutionTable, RetCoerce, RetCoerceTable, SumSeed, SumSeedTable, WitnessCallee,
    WitnessKey, WitnessSrc, WitnessTable, Wrap, WrapTable,
};
use ty::{compatible, param_invariant};

/// The fully-resolved C signature of one `extern` fn, computed by the checker in the defining
/// module's import/alias scope (the single source of truth for every alias spelling). Each param /
/// the return is a width-bearing [`CType`] (NOT a `Ty`, which collapses `int8`..`int64` to `Ty::Int`
/// — the width is exactly what FFI marshalling needs). `None` for a param means an annotation the
/// checker could not lower (only reachable on an ill-typed program the marshallability gate already
/// rejected); `ret` is `None` for a `void` return (no annotation or one resolving to `nil`).
#[derive(Clone, Default)]
pub struct ExternCSig {
    pub params: Vec<Option<CType>>,
    pub ret: Option<CType>,
    /// Declared with a trailing bare `...`: each call's surplus args are C varargs.
    pub c_variadic: bool,
}

/// Resolved C signatures for every `extern` fn in a module graph, keyed by `(graph module index,
/// fn name)` — the SAME index both backends derive (`compile_graph`'s enumerate / interp's
/// `module_idx_of`). The backends consume these instead of re-resolving alias names themselves, so
/// every alias spelling (local chain, named-import hop, qualified hop, mixed) resolves in its own
/// module's scope, collision-proof by construction. Produced by [`resolve_extern_signatures`].
pub type ExternTable = HashMap<(usize, String), ExternCSig>;

/// One `native enum` of the prelude (`Option[T]`, `Result[T, E]`) as harvested by
/// [`Checker::harvest_native_enums`]: the data [`Checker::seed_carrier_enums`] registers in the
/// ordinary enum tables of every module.
pub(super) struct NativeEnumSeed {
    name: String,
    type_params: Vec<TyParam>,
    /// `(variant, payload)` in declaration order.
    variants: Vec<(String, Vec<Ty>)>,
}

/// What the prelude gives every module (`Checker::harvest_native_enums`): its carrier enums and
/// the variant imports it declares for them (`import Some, None from Option`).
#[derive(Default)]
pub(super) struct CarrierSeeds {
    enums: Vec<NativeEnumSeed>,
    /// `(bound name, enum name, variant)` per prelude variant import.
    imports: Vec<(String, String, String)>,
}

/// A bare name an `import V from Enum` binds to an enum variant (TICKET-229, R3b).
pub(super) struct ImportedVariant {
    /// The enum's head, as `bare_type_head` answered it at the import (DEC-204).
    head: setup::TypeHead,
    variant: String,
    /// Bound by the prelude (`Some` / `None` / `Ok` / `Err`): a declaration or an import in a user
    /// module shadows it, as a builtin.
    prelude: bool,
}

/// What a `match` scrutinee is being matched against, threaded through the match-checking helpers.
enum MatchKind {
    /// Enum/Result/Option scrutinee — arms are variant patterns.
    Variants {
        label: String,
        variants: HashMap<String, Vec<Ty>>,
        /// The scrutinee's own type (generic args included), for a bare whole-value catch-all
        /// binding (TICKET-107).
        scrut: Ty,
    },
    /// int/str/bool scrutinee — arms are literal patterns (+ a required `_` wildcard).
    Literal(Ty),
    /// Tuple scrutinee — arms are tuple patterns (gap #15). Carries the element types.
    Tuple(Vec<Ty>),
    /// Struct scrutinee (L2) — arms are positional field patterns (`Point(x, y)`). A struct has
    /// exactly ONE constructor, so a single all-binding `label(..)` arm is irrefutable ⇒ exhaustive.
    /// `fields` are the INSTANTIATED positional field types (generic params substituted). `targs`
    /// carries the scrutinee's type arguments (`Box[int]` → `[int]`) so a whole-value catch-all
    /// binding reconstructs the full `Ty::Struct(label, targs)` instead of stripping generics.
    Struct {
        label: String,
        fields: Vec<Ty>,
        targs: Vec<Ty>,
    },
    /// Un-inferable (`Ty::Unknown`) scrutinee with only binding/`_` arms — skip exhaustiveness, bind
    /// permissively. A STRUCTURAL arm over an un-inferable scrutinee is rejected upstream (§4.1, in
    /// `reconstruct_unknown_kind` for the top-level arm and `bind_subpattern` for nested positions);
    /// a literal/range arm pins `MatchKind::Literal`. So `Skip` only carries the genuinely-permissive
    /// residue.
    Skip,
}

impl MatchKind {
    /// The scrutinee's type, as the one pattern binder reads it.
    fn scrut_ty(&self) -> Ty {
        match self {
            MatchKind::Variants { scrut, .. } => scrut.clone(),
            MatchKind::Literal(ty) => ty.clone(),
            MatchKind::Tuple(tys) => Ty::Tuple(tys.clone()),
            MatchKind::Struct { label, targs, .. } => Ty::Struct(label.clone(), targs.clone()),
            // Reported upstream (`reconstruct_unknown_kind`).
            MatchKind::Skip => Ty::Unknown,
        }
    }
}

/// Severity of a checker diagnostic. `Error` fails the build; `Warning` is reported and the build
/// continues (exit code unchanged) — Rust's `unused_must_use` model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

/// A type error, with the source span it occurred at. Mirrors `ParseError` / `RuntimeError`.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckError {
    pub message: String,
    pub span: Span,
    pub severity: Severity,
    /// A near-miss suggestion ("did you mean 'len'?"). NEVER module-attributed: `Checker::attribute`
    /// prefixes `in module 'X': ` onto `message` only, and a suggestion names a method or field, not
    /// a module, so prefixing it there would read wrong.
    pub help: Option<String>,
}

impl CheckError {
    pub fn error(message: String, span: Span) -> Self {
        CheckError {
            message,
            span,
            severity: Severity::Error,
            help: None,
        }
    }

    pub fn warning(message: String, span: Span) -> Self {
        CheckError {
            message,
            span,
            severity: Severity::Warning,
            help: None,
        }
    }
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.severity {
            Severity::Error => write!(f, "type error ({}): {}", self.span, self.message),
            Severity::Warning => write!(f, "warning ({}): {}", self.span, self.message),
        }
    }
}

impl CheckError {
    /// Render for a human, attributed to `path` when one resolves — the SAME [`crate::lexer::render_span`]
    /// single source of truth the runtime trace (`vm::format_trace`) and `--errors=json`'s `file` key
    /// use, so all three agree on the `path:line:col` form. `Display` (above) is left as the bare
    /// `line N, col M` form FOREVER: many checker unit tests compare against it directly, and unit
    /// tests construct a `CheckError` with no module graph in reach to name a path from.
    ///
    /// A `help` suggestion, when present, is appended on its own `help: <h>` line. `Display` stays
    /// bare with no `help` line for the same forever-pinned reason above.
    pub fn render(&self, path: Option<&std::path::Path>) -> String {
        let pos = crate::lexer::render_span(self.span, path);
        let head = match self.severity {
            Severity::Error => format!("type error ({pos}): {}", self.message),
            Severity::Warning => format!("warning ({pos}): {}", self.message),
        };
        match &self.help {
            Some(h) => format!("{head}\nhelp: {h}"),
            None => head,
        }
    }
}

/// The dotted path an import targets, for error messages (`core.db`).
fn module_label(import: &Import) -> String {
    let path = match import {
        Import::Module { path, .. } | Import::From { path, .. } => path,
        Import::Variants { enum_name, .. } => return enum_name.clone(),
    };
    path.join(".")
}

/// D3 (TICKET-138): an int NEVER widens into a `float` slot, at any sink. The fix a diagnostic names.
pub(crate) const FLOAT_FIX_NOTE: &str = " — write 1.0 (or x.to_float())";

/// Does `actual` differ from `expected` by an int sitting where a `float` is expected — at the top or
/// nested under the same type constructor? It only picks whether a failed slot check carries
/// [`FLOAT_FIX_NOTE`]; it never decides what is accepted.
fn int_where_float(expected: &Ty, actual: &Ty) -> bool {
    fn any_pair(e: &[Ty], a: &[Ty]) -> bool {
        e.len() == a.len() && e.iter().zip(a).any(|(e, a)| int_where_float(e, a))
    }
    match (expected, actual) {
        (Ty::Float, Ty::Int) => true,
        (Ty::List(e), Ty::List(a))
        | (Ty::Set(e), Ty::Set(a))
        | (Ty::Option(e), Ty::Option(a))
        | (Ty::Channel(e), Ty::Channel(a))
        | (Ty::Shared(e), Ty::Shared(a))
        | (Ty::Atomic(e), Ty::Atomic(a))
        | (Ty::RwShared(e), Ty::RwShared(a)) => int_where_float(e, a),
        (Ty::Map(_, e), Ty::Map(_, a)) => int_where_float(e, a),
        (Ty::Result(e1, e2), Ty::Result(a1, a2)) => {
            int_where_float(e1, a1) || int_where_float(e2, a2)
        }
        (Ty::Tuple(e), Ty::Tuple(a)) => any_pair(e, a),
        (Ty::Struct(n, e), Ty::Struct(m, a))
        | (Ty::Enum(n, e), Ty::Enum(m, a))
        | (Ty::Protocol(n, e), Ty::Protocol(m, a))
            if n == m =>
        {
            any_pair(e, a)
        }
        (
            Ty::Func {
                params: p1,
                ret: r1,
                ..
            },
            Ty::Func {
                params: p2,
                ret: r2,
                ..
            },
        ) => any_pair(p1, p2) || int_where_float(r1, r2),
        // The success-coercion sink (W8-21): a bare `1` at `float?` / `float!`.
        (Ty::Option(e), a) | (Ty::Result(e, _), a)
            if !matches!(a, Ty::Option(_) | Ty::Result(..)) =>
        {
            int_where_float(e, a)
        }
        _ => false,
    }
}

/// U8 (TICKET-260): the note for a `!` value (a `recover:` block, or a fn that returns `!`) read where
/// a `T!` is expected. The block has no tail value; the fix is to assign inside it.
pub(crate) const NO_VALUE_NOTE: &str = " — this value has type `!`: a `recover:` block has no tail value; assign inside it (`v := 0`, then `v = <value>` in the block)";

/// Is `actual` a `!` (`Result[nil, _]`) where `expected` is a `Result` that carries a value? It only
/// picks whether a failed slot check carries [`NO_VALUE_NOTE`]; it never decides what is accepted.
fn no_value_where_value(expected: &Ty, actual: &Ty) -> bool {
    matches!((expected, actual), (Ty::Result(q, _), Ty::Result(p, _)) if **p == Ty::Nil && **q != Ty::Nil)
}

/// The note a DIRECTED slot mismatch (an `expected` slot, an `actual` value) appends when the cause is
/// an int where a `float` is expected, or a `!` where a `T!` is expected. Empty for every other
/// mismatch. The note is chosen from the two types only.
pub(crate) fn slot_note(expected: &Ty, actual: &Ty) -> &'static str {
    if int_where_float(expected, actual) {
        FLOAT_FIX_NOTE
    } else if no_value_where_value(expected, actual) {
        NO_VALUE_NOTE
    } else {
        ""
    }
}

/// [`slot_note`] for a SYMMETRIC mismatch (two branches, two elements, two operands): the int
/// may sit on either side.
pub(crate) fn float_fix_note_join(a: &Ty, b: &Ty) -> &'static str {
    if int_where_float(a, b) || int_where_float(b, a) {
        FLOAT_FIX_NOTE
    } else {
        ""
    }
}

/// TICKET-239 (owner, 2026-10-10) — the note for a failed join of a carrier `k` (`T?` or `T!E`,
/// read through [`Ty::carrier_parts`]) beside exactly its plain payload, in either order: a plain
/// value wraps only into a type that is written, so the message names the annotation. Empty for
/// every other pair. It never decides what is accepted.
pub(crate) fn carrier_join_note(a: &Ty, b: &Ty) -> String {
    let pair = |k: &Ty, p: &Ty| k.carrier_parts().is_some_and(|c| c.1 == p);
    let (k, p) = if pair(a, b) {
        (a, b)
    } else if pair(b, a) {
        (b, a)
    } else {
        return String::new();
    };
    format!(
        " — a plain {p} joins {k} only under a written type: annotate the binding \
         (x: {k} = ..., xs: List[{k}] = [...])"
    )
}

/// A short, surface-faithful label for a return-only extern `Type` in a marshallability error
/// (`owned_str`, `str?`, `owned_str?`). Only ever called on the forms `is_return_only_extern_type`
/// already matched, so non-matching shapes fall back to a generic label.
fn describe_extern_type(t: &Type) -> String {
    match t {
        Type::Named { name: n, .. } => n.clone(),
        Type::Generic(n, args, ..) if crate::ast::unprivate(n) == "Option" => match args.first() {
            Some(Type::Named { name: inner, .. }) => format!("{inner}?"),
            _ => "str?".to_string(),
        },
        Type::Qualified { .. }
        | Type::Generic(..)
        | Type::Func { .. }
        | Type::Tuple(_)
        | Type::Nil(_) => "owned_str".to_string(),
    }
}

/// Every builtin callable name, from [`Builtin::ALL`] (test helper).
#[cfg(test)]
fn builtin_callables() -> Vec<&'static str> {
    Builtin::ALL.into_iter().map(Builtin::name).collect()
}

/// A prelude builtin: a fn or a scalar / container ctor (every [`Builtin`] but the ones with an
/// allocation opcode, which `builtin_callee` reaches through their `BuiltinType`). The SIGNATURES
/// of the first-class fns and scalar ctors are the `native fn` / `native ctor` decls of
/// `std/prelude.chz` (`Checker::builtin_sig`); a container ctor's generic type identity stays in
/// `resolve_type` / `infer_builtin_call`.
pub(crate) fn prelude_fn(name: &str) -> Option<Builtin> {
    Builtin::from_name(name).filter(|b| b.lowering() != Lowering::Opcode)
}

/// True if `name` is a first-class universe builtin fn.
pub(crate) fn is_firstclass_builtin_fn(name: &str) -> bool {
    prelude_fn(name).is_some_and(Builtin::first_class)
}

/// True when a std file declares a protocol called `name`. The `protocol` declarations in `std/*.chz`
/// (`graph_pass::std_protocol_decls`) are the one source of which protocols exist; a user module may
/// still declare a protocol of that name, which shadows the std one in its own module. Every caller
/// is a per-DECLARATION hoist/setup check or a name resolution, off any hot path.
fn is_std_protocol(name: &str) -> bool {
    graph_pass::std_protocol_owner(name).is_some()
}

/// True when the std protocol `name` is global: declared in the prelude (`Any`, `Error`), so every
/// module sees it with no import.
fn is_global_protocol(name: &str) -> bool {
    graph_pass::std_protocol_owner(name) == Some("std.prelude")
}

/// True if `name` is one of the four recognized suite lifecycle hooks.
fn is_lifecycle_hook(name: &str) -> bool {
    crate::base::op::LIFECYCLE_HOOKS.contains(&name)
}

/// The call [`Checker::infer_call`] is dispatching (see [`Checker::call_ctx`]).
#[derive(Clone)]
struct CallCtx {
    id: crate::ast::NodeId,
    named: Vec<(String, Expr)>,
    /// The span a synthesized variadic pack is keyed by (`witness_key_span` of the callee).
    pack_origin: Span,
    /// A binder or a refusal answered the call's named arguments; otherwise `infer_call` reports
    /// the generic refusal.
    consumed: bool,
    /// The call span, where a refusal is reported.
    span: Span,
    /// The expression reading of a one-arg call bracket (`ExprKind::Call::bracket`).
    bracket: Option<Expr>,
}

/// A protocol bound with its type args RESOLVED (`Conv[S]` -> name `Conv`, args `[Ty::Param("S")]`).
/// The args are resolved once, in the DECLARING scope, by [`Checker::resolve_bound`] (TICKET-202):
/// a use site substitutes them, never re-resolves them in the caller's scope.
#[derive(Clone, Debug, PartialEq)]
struct TyBound {
    name: String,
    args: Vec<Ty>,
}

/// A declared type parameter with its bounds resolved once in the declaring scope (TICKET-202) by
/// [`Checker::resolve_bounds`]. The resolved twin of the AST [`TypeParam`].
#[derive(Clone, Debug, PartialEq)]
struct TyParam {
    name: String,
    name_span: Span,
    bounds: Vec<TyBound>,
}

/// [`subst`] over every bound arg of `tps` (names unchanged).
fn subst_ty_params(tps: &[TyParam], map: &HashMap<String, Ty>) -> Vec<TyParam> {
    tps.iter()
        .map(|tp| TyParam {
            name: tp.name.clone(),
            name_span: tp.name_span,
            bounds: tp
                .bounds
                .iter()
                .map(|b| TyBound {
                    name: b.name.clone(),
                    args: b.args.iter().map(|a| subst(a, map)).collect(),
                })
                .collect(),
        })
        .collect()
}

/// A function (or method) signature: parameter types and return type. `type_params` is non-empty
/// only for generic functions (`fn max[T: Comparable]`), where `params`/`ret` contain `Ty::Param`s.
#[derive(Clone)]
struct FnSig {
    params: Vec<Ty>,
    /// Swift-style parameter labels parallel to `params` (the declaration's param names; `None` for
    /// `self` or an unnamed slot). Surface-only — used ONLY to build a labelled `Ty::Func` value type
    /// in `infer_ident` so `g := f; g(name=…)` resolves. Excluded from `fn_sig_eq` (labels are not
    /// type identity). Empty for synthetic/native sigs built via `plain`.
    labels: Vec<Option<String>>,
    ret: Ty,
    type_params: Vec<TyParam>,
    /// `where T: Bound` clauses (empty for the common case). For a NATIVE method sig they name the
    /// enclosing native struct's type param and are enforced at each call site by the container
    /// method-dispatch arm (`enforce_bounds`); for a USER fn they are already MERGED into
    /// `type_params` by `fn_sig`, so this stays empty there. Excluded from `fn_sig_eq` (bound
    /// documentation, not type identity) — see the note there.
    where_bounds: Vec<TyParam>,
    /// D6c — the minimum number of arguments this signature accepts; `params.len()` for an ordinary
    /// fixed-arity signature (set by [`FnSig::plain`]), but smaller when trailing params are optional
    /// (the net socket ops' optional `timeout_ms`). [`Checker::check_args`] accepts any arg count in
    /// `min_params..=params.len()`.
    min_params: usize,
    /// A struct/enum method whose first parameter is NOT `self` (or which has no params) is a STATIC
    /// (associated) method — called `Type.method(args)`, not `value.method(args)`. `false` for every
    /// instance method, free function, closure, and native sig (the default via `plain`);
    /// set only by [`Checker::fn_sig`] from the declaration's first param name.
    is_static: bool,
    /// Doc-comment from the declaration (set by [`Checker::fn_sig`] from `FnDecl::doc`; `None` for
    /// native/synthetic sigs). Purely informational — surfaced on LSP hover, never affects checking
    /// (excluded from `fn_sig_eq`). Covers free fns AND methods/static methods (all reuse `FnSig`).
    doc: Option<String>,
    /// M24 — the type params this fn takes a hidden trailing `$w:T` witness argument for, in
    /// declaration order (empty for everything else, which is nearly every signature). Computed by
    /// [`Checker::witness_params_of`] at the signature hoist — where the declaration BODY is still
    /// available — then re-derived to a fixpoint over the module's free fns (and re-passed once over
    /// its members) in [`Checker::hoist`], since forwarding makes the answer depend on the callees'.
    /// [`FnSig::min_params`] is DERIVED from this and is rewritten wherever this is, so the two can
    /// never disagree. Read from here by every consumer (the body's `witness_scope`, the
    /// [`WitnessTable::fns`] entry the compiler lowers, the fn-as-value wall, the `spawn`/`defer`
    /// target rejection, and the per-call-site record). Crosses the module boundary inside
    /// [`ModuleSig`]. Excluded from `fn_sig_eq` (a lowering detail, not type identity). On a METHOD's
    /// sig it is inert — slice A threads witnesses for module-level FREE fns only, and every consumer
    /// reaches this field through a free-fn table.
    witness_params: Vec<String>,
    /// `Some(i)` iff parameter `i` is variadic (`fn f(...xs: T)`) — its slot type is the collapsed
    /// `List[T]`, and everything after index `i` is keyword-only. Surface-only: the desugar pass
    /// collapses surplus positionals into a `List` literal before checking, so this drives arity /
    /// keyword-only enforcement in desugar. Excluded from `fn_sig_eq` (not type identity). `None` for
    /// every ordinary signature.
    variadic: Option<usize>,
    /// The declaration's call-binding slots (receiver dropped), which `Checker::bind_call` binds
    /// named, omitted and variadic arguments against. `None` for a native, builtin or `extern` sig
    /// (built by [`FnSig::plain`]): it has no Chezzi parameter list, takes
    /// no named arguments, and its `= 0` defaults are arity markers the runtime reads from the
    /// supplied argument count. Excluded from `fn_sig_eq`.
    slots: Option<Vec<crate::desugar::SlotSpec>>,
    /// An `extern` fn declared with a trailing bare `...` (C varargs): a call takes at least
    /// `params.len()` args, and each surplus arg is a C variadic arg (`check_c_variadic_args`).
    /// Distinct from `variadic` (Chezzi's `...xs: T`, collapsed to a `List`). `false` everywhere
    /// else. Excluded from `fn_sig_eq`.
    c_variadic: bool,
}

impl FnSig {
    /// This signature as a value read off a receiver: parameter 0 (`self`) dropped with its label and
    /// slot, the variadic index shifted, `min_params` lowered. The one builder of a bound-method
    /// hover type: `fn_value_ty(&sig.receiverless())`.
    fn receiverless(&self) -> FnSig {
        let mut sig = self.clone();
        sig.params.remove(0);
        if !sig.labels.is_empty() {
            sig.labels.remove(0);
        }
        if let Some(slots) = &mut sig.slots
            && !slots.is_empty()
        {
            slots.remove(0);
        }
        sig.variadic = sig.variadic.and_then(|v| v.checked_sub(1));
        sig.min_params = sig.min_params.saturating_sub(1);
        sig
    }

    /// A non-generic signature (the common case): every param is required (`min_params == params.len()`).
    fn plain(params: Vec<Ty>, ret: Ty) -> FnSig {
        let min_params = params.len();
        FnSig {
            labels: Vec::new(),
            params,
            ret,
            type_params: Vec::new(),
            where_bounds: Vec::new(),
            min_params,
            is_static: false,
            doc: None,
            witness_params: Vec::new(),
            variadic: None,
            slots: None,
            c_variadic: false,
        }
    }
}

/// Where a struct was declared: a stdlib module (`std.*`) vs a user/entry module. Lets a reserved-name
/// / import-collision rule key on a *builtin*-origin std struct (`Match`/`Response`/`ProcResult`/…)
/// without snaring a user struct that merely happens to share the name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum StructOrigin {
    Builtin,
    User,
}

/// A struct's shape: its generic type parameters (empty for a non-generic struct), ordered
/// `(field, type)` pairs, and its methods by name. Field/method types may contain `Ty::Param`s
/// naming the struct's type parameters; they're substituted at each use site.
#[derive(Clone)]
struct StructInfo {
    type_params: Vec<TyParam>,
    fields: Vec<(String, Ty)>,
    methods: HashMap<String, FnSig>,
    origin: StructOrigin,
    /// The struct's own decl docstring (the `#` block above `struct X:`), captured in `capture_sig`
    /// from `name_docs` so it crosses the module boundary to an importer's hover. Editor-only; `None`
    /// for a builtin/native struct and until `capture_sig` populates it. Never read by checking/codegen.
    doc: Option<String>,
    /// Names of the fields carrying a declared default, in declaration order. Recorded because the
    /// ctor arity check cannot otherwise tell an omitted DEFAULTED field from a missing required
    /// argument (W8-47).
    defaulted_fields: Vec<String>,
    /// The ctor's call-binding slots, one per field (see [`FnSig::slots`]). `None` for a native or
    /// builtin struct.
    field_slots: Option<Vec<crate::desugar::SlotSpec>>,
}

/// A protocol's required method signatures, in declaration order. `Self` appears as `Ty::Param("Self")`
/// inside these sigs; conformance substitutes it with the candidate type. `type_params` are the
/// protocol's own parameters (`protocol Container[T]` ⇒ `["T"]`); a bound supplies concrete args for
/// them (`[X: Container[int]]`), substituted into the method sigs before structural matching. Empty
/// for a bare protocol; the built-in `Iterator` carries one (its element type).
#[derive(Clone)]
struct ProtocolInfo {
    type_params: Vec<String>,
    methods: Vec<(String, FnSig)>,
    /// Embedded (super-)protocols (M22): `protocol Vector: Add + Sub` carries `[Add, Sub]`. A type
    /// satisfies this protocol iff it satisfies every embed (transitively) AND has every OWN method
    /// in `methods`. Empty for an ordinary protocol. Reuses [`Bound`] — an embed ref is identical to
    /// a type-param bound (name + optional `[args]`). The builtin `Arithmetic` bundle is built with
    /// this same field (`embeds: [Add, Sub, Mul, Div]`, no own methods) — uniform machinery.
    embeds: Vec<TyBound>,
}

/// A user enum variant: which enum it belongs to and its payload field types.
#[derive(Clone)]
struct VariantInfo {
    enum_name: String,
    payload: Vec<Ty>,
}

/// A module's public surface, computed when it's checked, consumed by its importers: one record
/// per top-level slot (`members`), and the type names it declares.
#[derive(Clone, Default)]
struct ModuleSig {
    /// TICKET-196: ONE export record per module slot (fn, extern, native, let, const let), built
    /// from the defining module's `GlobalBinding`. Every importer path reads this and nothing else.
    members: HashMap<String, MemberSig>,
    types: std::collections::HashSet<String>,
    /// The builtin types this module owns (`builtin_type::module_builtins`: `std.ffi`'s `ptr` and
    /// widths, `std.concurrency`'s `Shared`, …). A qualified `m.X` names a builtin type only when `X`
    /// is here; another module's own `struct ptr` is that module's struct.
    builtins: std::collections::HashSet<String>,
    /// Resolved struct definitions this module declares (module-scoped types — D? feature). An
    /// importer injects these into its own per-module `structs`/`struct_names` so a `from`-imported
    /// or qualified-access struct resolves with the right field layout.
    struct_defs: HashMap<String, StructInfo>,
    /// Resolved enum definitions this module declares: variant names (in order), generic type
    /// params, and each variant's payload `VariantInfo`.
    enum_defs: HashMap<String, EnumSigInfo>,
    /// Transparent type aliases this module declares, with their body RESOLVED in the DEFINING
    /// module's scope (so a cross-module `type Len = int32` carries its FFI-width license). The bool
    /// records whether the alias was licensed (`ffi_alias_ok`) in the defining module. The
    /// `Option<String>` is the name of an embedded FFI width that the defining module did NOT import
    /// (so the alias is unlicensed): an importer must reject it with "unknown type" — the width can't
    /// be laundered through an unlicensed alias.
    type_aliases: HashMap<String, AliasSig>,
    /// Protocol definitions this module declares. A protocol's whole shape already lives in one
    /// `ProtocolInfo` record (unlike a struct/enum, spread over several maps), so this wraps it
    /// rather than re-listing its fields.
    protocol_defs: HashMap<String, ProtocolSigInfo>,
}

/// What an importer may know about one exported module slot (TICKET-196). A new fact about an
/// exported name is a new field here, never a new per-kind table on `ModuleSig`.
#[derive(Clone)]
struct MemberSig {
    /// The slot's final type, once its module has finished initialising.
    ty: Ty,
    /// The one declaration the slot holds: set only when the module declares the name once as
    /// `fn` / extern / native, which a fn slot keeps because it can never be assigned. Labels,
    /// defaults, variadic, type params and witnesses are read from here and nowhere else.
    certain_fn: Option<FnSig>,
    /// A `const` top-level let, or a native constant.
    is_const: bool,
}

impl MemberSig {
    /// The slot holds a function value.
    fn holds_fn(&self) -> bool {
        matches!(self.ty, Ty::Func { .. })
    }
}

/// The one spelling rule for a name internal to std (TICKET-243): a `_` name a std module
/// declares. `Checker::std_internal_hidden` reads it for native methods and
/// `ModuleSig::seal_std_internals` for a std module's export record.
pub(crate) fn is_std_internal_name(name: &str) -> bool {
    name.starts_with('_')
}

impl ModuleSig {
    /// Remove every std-internal name from this (std) module's export record: a member, a type,
    /// and a method of an exported struct or enum. Runs at publication, after the module's own
    /// bodies are checked, so an importer never sees the name and no reader site needs a check.
    fn seal_std_internals(&mut self) {
        let k = |n: &String| !is_std_internal_name(n);
        self.members.retain(|n, _| k(n));
        self.types.retain(k);
        self.struct_defs.retain(|n, _| k(n));
        self.enum_defs.retain(|n, _| k(n));
        self.type_aliases.retain(|n, _| k(n));
        self.protocol_defs.retain(|n, _| k(n));
        for d in self.struct_defs.values_mut() {
            d.methods.retain(|n, _| k(n));
        }
        for d in self.enum_defs.values_mut() {
            d.methods.retain(|n, _| k(n));
        }
    }

    fn member(&self, n: &str) -> Option<&MemberSig> {
        self.members.get(n)
    }

    fn certain_fn(&self, n: &str) -> Option<&FnSig> {
        self.members.get(n).and_then(|m| m.certain_fn.as_ref())
    }

    fn fns(&self) -> impl Iterator<Item = (&String, &FnSig)> {
        self.members
            .iter()
            .filter_map(|(n, m)| m.certain_fn.as_ref().map(|f| (n, f)))
    }

    fn value_ty(&self, n: &str) -> Option<&Ty> {
        self.members.get(n).map(|m| &m.ty)
    }

    fn insert_fn(&mut self, n: String, f: FnSig) {
        let ty = fn_value_ty(&f);
        self.members.insert(
            n,
            MemberSig {
                ty,
                certain_fn: Some(f),
                is_const: false,
            },
        );
    }
}

/// The value type of a named fn: the FULL parameter list plus the optional arity and the
/// declaration's call slots, so `f := request.get; f(url)`, `f(url, 5)` and a keyword or variadic
/// call through a certain `f` all work. Every fn value type is built here (TICKET-197).
fn fn_value_ty(f: &FnSig) -> Ty {
    // `params` ends at the variadic pack slot; the keyword-only rest rides in `labels.kw`.
    let split = f.variadic.map_or(f.params.len(), |v| v + 1);
    let mut labels = FnLabels::new(f.labels.clone())
        .with_min(f.min_params)
        .with_slots(f.slots.clone());
    labels.kw = f.params[split..].to_vec();
    let variadic = match (f.variadic, f.c_variadic) {
        (Some(_), _) => crate::ast::Variadic::Pack,
        (None, true) => crate::ast::Variadic::C,
        (None, false) => crate::ast::Variadic::No,
    };
    Ty::Func {
        params: f.params[..split].to_vec(),
        ret: Box::new(f.ret.clone()),
        labels,
        variadic,
    }
}

/// A protocol's exported shape inside a `ModuleSig`: the checker's own `ProtocolInfo`, plus the
/// decl docstring (ferried across the module boundary for an importer's hover, like
/// `StructInfo::doc`).
#[derive(Clone)]
struct ProtocolSigInfo {
    info: ProtocolInfo,
    doc: Option<String>,
}

/// An exported type alias inside a `ModuleSig`: its body RESOLVED in the defining module's scope,
/// whether it carries an FFI-width license, and — if NOT licensed yet embeds an FFI width the
/// defining module never imported — that un-imported width's name (so an importer rejects it).
#[derive(Clone)]
struct AliasSig {
    body: Ty,
    licensed: bool,
    unlicensed_width: Option<String>,
    /// The alias body resolved to a width-bearing [`CType`] in the DEFINING module's scope (so a
    /// cross-module `type Len = int32` exports `int32`, not `Ty::Int`). `None` if the body is not
    /// C-marshallable (its FFI use is rejected by the checker). This is what carries the real C
    /// width across a `from`-import or a `module.Alias` hop — the `body: Ty` cannot, since `Ty`
    /// collapses every FFI width to `Ty::Int`.
    ctype: Option<CType>,
}

/// An enum's exported shape inside a `ModuleSig`: variant names in declaration order, the enum's
/// generic type params, and each variant's resolved `VariantInfo` (payload types).
#[derive(Clone)]
struct EnumSigInfo {
    variant_names: Vec<String>,
    type_params: Vec<TyParam>,
    variants: Vec<VariantInfo>,
    /// The enum's methods (`fn area(self) …`), name-keyed like `StructInfo.methods`. Ferried across
    /// the module boundary so an imported enum's methods resolve in the importer.
    methods: HashMap<String, FnSig>,
    /// The enum's own decl docstring, carried across the module boundary for an importer's hover
    /// (see [`StructInfo::doc`]). Editor-only; never read by checking/codegen.
    doc: Option<String>,
}

/// TICKET-238 -- the state of `errors` and `poison_reads` when a statement began, and its span.
#[derive(Clone, Copy, Default)]
pub(super) struct StmtMark {
    pub(super) errors: usize,
    pub(super) poison: usize,
    pub(super) span: Span,
}

struct Checker {
    errors: Vec<CheckError>,
    /// Non-fatal diagnostics (`Severity::Warning`), collected separately so they can never reach the
    /// `Err` arm of a check entry point and turn a warning into a build failure.
    warnings: Vec<CheckError>,
    /// The bare `key_ty_reject` reason a TYPE-position site (`Map[..]` / `Set[..]`) already reported
    /// for the statement being checked. A value-position literal with the identical reason skips its
    /// own report. Cleared at the top of `check_stmt`, so it never outlives one statement.
    pending_key_reject: Option<String>,
    scopes: Vec<HashMap<String, Ty>>,
    /// Per-scope set of names bound as `for`-loop variables. Mirrors `scopes` index-for-index (a
    /// loop var is immutable — rebound fresh each iteration — so assigning to it is rejected; this
    /// sidesteps a VM/interp divergence where the VM's counter slot IS the loop var).
    loop_vars: Vec<std::collections::HashSet<String>>,
    /// TICKET-186 — the one record per module slot of the module being checked
    /// ([`globals::GlobalBinding`]: its declarations, seeded / reached / cycle state). Built by
    /// `collect_module_globals` at the start of each `check_module`, before any body is walked.
    globals: HashMap<String, globals::GlobalBinding>,
    /// Per-scope set of names declared `const T` (mirrors `scopes` index-for-index). A const binding
    /// is immutable: `check_assign` rejects any later reassignment of the name. Compile-time-only
    /// (freezes the NAME; the object stays mutable — shallow). Cleared on re-declaration by `declare`
    /// (a shadowing `:=` yields a fresh, possibly-mutable binding), same rule as `loop_vars`.
    const_decls: Vec<std::collections::HashSet<String>>,
    /// TICKET-238 -- per-scope names whose binding the hole check rejected (parallel to `scopes`,
    /// kept site for site as `const_decls` is). A read of one is the cascade signal
    /// (`poison_reads`); the stored type is bare `Ty::Unknown`, which other sources produce too.
    hole_rejected: Vec<std::collections::HashSet<String>>,
    functions: HashMap<String, FnSig>,
    /// Names of functions declared in the CURRENT module (top-level `fn`s only — NOT imported names).
    /// Gates the generic-fn-as-value turbofish B-path (`ident[int]`) so the checker only accepts a
    /// turbofish on a SAME-MODULE generic fn — the exact set the compiler's `fn_names` erases at
    /// codegen, keeping checker-accept ⟺ compiler-erase in lockstep (an imported generic-fn turbofish
    /// stays a clean "cannot index into fn(T) -> T" error, a documented v1 limit). Cleared per-module
    /// with `functions` (`begin_module`) and populated at the same-module fn-sig registration.
    local_fn_names: std::collections::HashSet<String>,
    structs: HashMap<String, StructInfo>,
    /// Structural protocols by name. Program-global (like structs). Pre-seeded with `Comparable`.
    protocols: HashMap<String, ProtocolInfo>,
    /// Generic type parameters currently in scope (name → its protocol bounds), set while
    /// building/checking a generic fn's signature and body. Save/restore to nest. Bounds carry their
    /// type args (`Iterator[T]`) so element-type recovery can read them in scope.
    type_params: HashMap<String, Vec<TyBound>>,
    /// enum name → its variant names, in declaration order (for exhaustiveness).
    enums: HashMap<String, Vec<String>>,
    /// enum name → its generic type parameters (empty for a non-generic enum). Used to build the
    /// substitution from `Tree[int]`'s args onto each variant's payload (which may name `T`).
    enum_type_params: HashMap<String, Vec<TyParam>>,
    /// enum name (runtime key) → its methods (`fn area(self) …`), name-keyed exactly like
    /// `StructInfo.methods`. Resolves `enumval.method(args)` and protocol satisfaction for enums.
    enum_methods: HashMap<String, HashMap<String, FnSig>>,
    /// Keyed by `(enum_name, variant_name)`: variants are scoped under their enum, so two enums may
    /// declare the same variant name. Resolution always carries the enum (a qualified `Enum.Variant`
    /// or, in a pattern slot, the scrutinee's enum) — there is no bare-name lookup.
    variants: HashMap<(String, String), VariantInfo>,
    /// variant name → the enum(s) that declare it. A pure-diagnostic reverse index: lets a bare
    /// variant use (illegal now) report "write it qualified as `Enum.Variant`" and keeps a bare
    /// known-variant from silently becoming a pattern binding (the bare→binding trap).
    variant_owners: HashMap<String, Vec<String>>,
    struct_names: std::collections::HashSet<String>,
    enum_names: std::collections::HashSet<String>,
    /// Transparent type aliases (`type UserId = int`): name → the aliased AST type, resolved on
    /// demand in `resolve_type`. `alias_resolving` is the active resolution stack (cycle guard).
    aliases: HashMap<String, Type>,
    alias_resolving: Vec<String>,
    /// Alias names whose body is a fixed-width FFI type name (`type Len = int32`) that the alias's
    /// DEFINING module imported per-name from `std.ffi`. Program-global like `aliases` (NOT cleared
    /// in `begin_module`), and the precise opt-in for the width-import gate: a width name resolves
    /// through an alias only if the alias is in this set — i.e. its defining module imported the
    /// width. This closes the gate hole where any `type Len = int32` (even with no module importing
    /// int32 anywhere) laundered the bare width name past the per-module import requirement.
    ffi_alias_ok: std::collections::HashSet<String>,
    /// Declared return type of the function body currently being checked (`Nil` at top level).
    current_ret: Ty,
    /// `Some(T)` while resolving a struct/enum method's SIGNATURE or BODY — the concrete
    /// enclosing type `Self` names (e.g. `fn dup(self) -> Self` inside `struct P`). `None` at top
    /// level, inside a free fn, or a nested fn/closure (reset like `current_ret`), so `Self` outside
    /// a method stays `unknown type 'Self'`. A PROTOCOL method keeps `None` here: its `Self` is
    /// already in `type_params` as `Ty::Param("Self")` (resolved by the earlier type-param arm), so
    /// this concrete binding never fires for it. Saved/restored via `mem::replace` at the method-sig
    /// hoist sites and at `infer_inline_fn_ret`/`check_fn_body` entry (from their `self_ty` argument).
    current_self_ty: Option<Ty>,
    /// `Some(T)` while checking a generator function body whose declared return is `Iterator[T]` —
    /// the element type each `yield` must produce. `None` outside a generator, so a stray `yield`
    /// is diagnosed. Saved/restored across nested fn/closure boundaries like `current_ret`.
    yield_ty: Option<Ty>,
    /// The spawn, defer and recover blocks the walk is inside, innermost last, each with the
    /// `loop_depth` at its entry. A fn or closure body is its own frame: it saves and clears the
    /// stack. [`boundary::RULES`] says what may cross each block.
    blocks: Vec<(boundary::Block, usize)>,
    /// True while a walk is SPECULATIVE (an inline body or a global typed by `infer_returns`): its
    /// diagnostics are rolled back and the main walk re-reports them.
    inferring_ret: bool,
    /// True for the whole `infer_returns` call (its return fixpoint, `type_globals_pass` and
    /// `report_untyped_globals`). `resolve_path` writes nothing while it is set: the main walk
    /// is the one writer of every NodeId's Resolution (TICKET-180).
    resolving_returns: bool,
    /// True while checking (or inferring the return of) a generator function body — the sole signal
    /// that a `yield` is in-bounds. Distinct from `yield_ty`: during return inference (`infer_inline_fn_ret`)
    /// the element type is not yet pinned (`yield_ty` is `None`), yet a `yield` is still legal and must
    /// be COLLECTED, not diagnosed. Saved/restored across nested fn/closure boundaries; a closure
    /// resets it to `false` so a (hypothetical) closure `yield` cannot seed the enclosing generator.
    in_generator: bool,
    /// True while checking (or inferring the return of) ANY fn/closure body; FALSE at module
    /// top-level. Saved/restored 1:1 beside `current_ret` at every fn/closure boundary. It exists
    /// solely to distinguish the two `current_ret == Nil` contexts for `?`: module top-level (legal —
    /// the runtime unwinds an unhandled Err/None at the program boundary) vs a nil-returning fn body
    /// (illegal — the propagated Err/None would be silently swallowed). See `infer_try`.
    in_fn_body: bool,
    /// The binders of each let whose value is being checked, innermost last, each with the index
    /// into `scopes` of the scope the let binds into. A binder is not readable inside its own value
    /// (owner 2026-10-18).
    let_binders: Vec<(String, usize)>,
    /// TICKET-239 — set by `infer_null_coalesce` just before it infers the `match` a `??` lowers
    /// to, and taken at the entry of `infer_match`, so a side mismatch names `??`, not "branches".
    coalesce_join: bool,
    /// True while checking the body of a **default-argument provider** — the hidden zero-arg fn
    /// `desugar` synthesizes for a non-inline parameter/field default (`desugar::PROVIDER_PREFIX`).
    /// Its only reader is [`Checker::infer_try`]: a `?` there has no caller to propagate to, and the
    /// generic "returns int, not Result" wording would name a return type the user never wrote.
    /// Saved/restored 1:1 beside `current_ret` at every fn/closure boundary.
    in_default_provider: bool,
    /// True while checking the DECL-SITE copy of a parameter/field default (`fn f(x: T = expr())`,
    /// `struct S: f: T = expr()`). That copy exists only to catch a wrong-typed default at the
    /// declaration; the expression's real home is the synthesized provider (or the splice at each
    /// call), where the enclosing generic's params are actually bound. So a diagnostic whose whole
    /// premise is "nothing can bind this here" must stay silent — the same reason W7-51 neutralized
    /// `current_ret`/`in_fn_body` around this copy for `?`. Read by
    /// [`Checker::report_uninferable_result_params`]. Saved/restored 1:1 around each copy.
    decl_site_default: bool,
    /// TICKET-201 — the struct-body binders a default of the type being checked could name:
    /// `(name, binder span, owner type name)` for each field that carries a default and each
    /// method. Read by [`Checker::check_default_scope`]. Set and restored by the struct and enum
    /// arms around their field defaults and method bodies.
    default_binders: Vec<(String, Span, String)>,
    /// W8-21 — true while checking (or inferring the return of) a fn/closure body whose return type
    /// is DECLARED (a `fn`'s `decl.ret.is_some()`, always true for a closure with an explicit `->`).
    /// The sole gate for the success-coercion sinks: an UN-annotated sink has no `T?`/`T!E` to coerce
    /// into (`sig.ret` there is inferred FROM the body, so gating on it would be circular). Reset to
    /// `false` at module top level. Saved/restored 1:1 beside `current_ret` at every fn/closure
    /// boundary.
    ret_declared: bool,
    /// The name of the fn whose body `check_fn_body` is walking, for the "declares no return
    /// type" text of `check_return`. Saved/restored beside `ret_declared`.
    ret_owner: Option<String>,
    /// The untyped-call probe of `infer_inline_fn_ret`: `Some(None)` while an inline body is
    /// walked, `Some(Some(span))` once a call in it typed to `Unknown`. `Checker::infer` is the
    /// one writer; no call site knows about it.
    untyped_call: Option<Option<Span>>,
    /// Public surfaces of already-checked modules (multi-file programs), keyed by module id.
    module_sigs: HashMap<ModuleId, ModuleSig>,
    /// Names bound to an imported module in the *current* module → which module they refer to.
    imported_modules: HashMap<String, ModuleId>,
    /// TICKET-187 — the `std.json` module's id, when the graph holds it. `decode[T](s)` on this
    /// module (and only this one) is JSON decode (`Resolution::Decode`).
    json_module: Option<ModuleId>,
    /// The runtime key of the `Range` struct `a..b` builds ([`crate::resolver::range_key`]).
    range_key: String,
    /// Every dotted (2+ segment) whole-module import in the current module, as `(path, bound name,
    /// full path bound)` (TICKET-175). The third field is `false` for `import a.b as c`, which binds
    /// only `c`. Read only by the diagnostics for a full path that does not resolve (`a.b.X` after an
    /// aliased import, or naming an unimported module); never a binding itself.
    import_paths: Vec<(Vec<String>, String, bool)>,
    /// A bare name two un-aliased imports both bind (`import a.math` + `import b.math`) → the two
    /// dotted paths (TICKET-175). The name is unbound; a use of it reports the ambiguity, and both
    /// full paths stay usable.
    ambiguous_binds: HashMap<String, (String, String)>,
    /// The module imports that bind no bare name (TICKET-175), keyed by statement span and bound
    /// name. `decide_bare_imports` writes it; `collect_module_globals` and `bind_import` read it.
    nameless_imports: std::collections::HashSet<(Span, String)>,
    /// Every name an `import`/`import … from` binds in the *current* module → the span of its first
    /// import. The registry keeps the first import's span for messages; `GlobalBinding::clashes`
    /// reports a second import. Per-module: cleared in `begin_module`.
    import_binds: HashMap<String, Span>,
    /// Type names `from`-imported into the *current* module that resolve to an aliased `Ty`
    /// (`import Len from m` where `m` declares `type Len = int32`). Resolved in the DEFINING module's
    /// scope and injected here so `resolve_type` returns the right underlying type. Per-module.
    imported_alias_tys: HashMap<String, Ty>,
    /// Parallel to [`Self::imported_alias_tys`] but carrying the alias's width-bearing [`CType`]
    /// (computed in the DEFINING module's scope), so a `from`-imported FFI-width alias resolves to
    /// its true width at an extern boundary in THIS module. `Ty` can't carry it (it collapses all
    /// FFI widths to `Ty::Int`), so this is the channel a named-import hop's real width travels.
    /// Per-module: cleared in `begin_module`.
    imported_alias_ctypes: HashMap<String, Option<CType>>,
    /// Extern-fn resolved C signatures harvested during checking, keyed by `(graph module index, fn
    /// name)`. Only populated when [`resolve_extern_signatures`] drives the pass (the error-gate
    /// `check_graph` leaves it empty). The current module's graph index is set per `check_module`.
    extern_sigs: ExternTable,
    /// AST field types of every user struct in the graph, keyed by IDENTITY KEY, gathered once for
    /// [`resolve_ctype`]: a by-value extern struct's `CType::Struct` is built from these (preserving
    /// each field's real C width, which `StructInfo`'s `Ty` fields collapse to `Ty::Int`). Only
    /// populated when [`resolve_extern_signatures`] drives the pass.
    struct_field_asts: HashMap<String, Vec<(String, Type)>>,
    /// The FULLY-RESOLVED, width-bearing by-value [`CType::Struct`] of every user struct in the graph,
    /// keyed by IDENTITY KEY, each computed ONCE in the struct's OWN DEFINING module's import/alias
    /// scope (extending the [`AliasSig::ctype`] precedent to structs). This is the structural fix to
    /// the FFI qualified/aliased-struct-field saga: an importer's extern returning `mod.DivT` reads
    /// this cache VERBATIM, so a field typed via the defining module's local alias (`type Half =
    /// int32`) keeps its true width — the importer's scope (where `Half` is invisible / colliding) is
    /// never consulted. `None` entry = a field is not a scalar leaf (non-marshallable; the marshal
    /// gate is the actual error). Populated deps-first, so a cross-module / qualified / nested struct
    /// is always cached before an importer needs it. NOT cleared per-module (graph-wide). Only built
    /// when [`resolve_extern_signatures`] drives the pass.
    struct_ctypes: HashMap<String, Option<CType>>,
    /// The CURRENT module's graph index (set in `check_module` when harvesting extern sigs), so each
    /// extern fn's resolved C signature is keyed under the SAME index the backends derive. `None`
    /// for a lone `check`.
    extern_module_idx: Option<usize>,
    /// The argument slot plan of every call [`Self::bind_call`] bound, keyed `(module idx, call
    /// NodeId)`. Recorded in every main walk (TICKET-189: the error-gate pass reads it through
    /// `bound_slots`) and consumed by the compiler's `compile_plan_args`. Produced by
    /// [`resolve_call_tables`].
    call_plans: CallPlanTable,
    /// The call `infer_call` is dispatching: its NodeId, named arguments and variadic-pack origin,
    /// read by [`Self::bind_call`] in whichever arm picks the callee. Saved and restored around a
    /// nested call.
    call_ctx: Option<CallCtx>,
    /// The default provider whose body is being checked, and its declaration span.
    current_provider: Option<(String, Span)>,
    /// Provider → the providers its body's calls fill a default from ([`Self::bind_call`]), for the
    /// cycle check [`Self::check_provider_cycles`] runs once the pass is done.
    provider_edges: HashMap<String, (Vec<String>, Span)>,
    /// Graph module index by module file id, so an inline default fill names its declaring module.
    module_idx_of_file: HashMap<u32, usize>,
    /// True only while [`resolve_call_tables`] drives the pass, licensing `infer_call` to record
    /// into [`Self::witnesses`]. Off during the normal error-gate check
    /// (which discards both tables).
    harvest_keywords: bool,
    /// M24 — both halves of the static-witness contract (which fns need hidden witness params, and
    /// what fills each witness at each call site). Recorded only while [`Self::harvest_keywords`] is
    /// set; consumed verbatim by the compiler. See [`WitnessTable`].
    witnesses: WitnessTable,
    /// W7-43 — which lowering each `?.` carrier takes, keyed by [`carrier_key`] and consumed
    /// verbatim by the compiler (which cannot re-derive it: the decision is the operand's TYPE).
    /// Recorded UNCONDITIONALLY — not gated on [`Self::harvest_keywords`], because a gate is a
    /// second way for the harvest pass and `check_graph` to disagree. Produced by
    /// [`resolve_call_tables`].
    carriers: CarrierTable,
    /// TICKET-250 (D4) — `Some` while `hoist` runs: each deferred [`Obligation`] with the
    /// type-parameter scope it was asked in. `None` outside `hoist`: an obligation is discharged
    /// at once.
    deferred_obligations: Option<Vec<DeferredObligation>>,
    /// TICKET-250 — the hooks each declared type has. See [`HookTable`].
    hook_table: HookTable,
    /// TICKET-250 — the call sites typed through a reserved requirement. See [`HookCallTable`].
    hook_calls: HookCallTable,
    /// TICKET-262 — see [`FieldWidthTable`]. Written only by [`Checker::note_field_widths`].
    field_widths: FieldWidthTable,
    fresh_subscripts: HashSet<CarrierKey>,
    /// Which `.sum()` sites sum a `List[float]` and so need a `0.0` seed, keyed by
    /// [`carrier_key`] on the method-name token and consumed verbatim by the compiler (which cannot
    /// re-derive it: the decision is the ELEMENT's type, and an empty list carries none at runtime).
    /// Recorded UNCONDITIONALLY, for the same reason [`Self::carriers`] is. See [`SumSeedTable`].
    sum_seeds: SumSeedTable,
    /// W8-21 — which implicit success-coercion, if any, each declared `T?`/`T!E` return sink applies,
    /// keyed by [`ret_coerce_key`] on the returned value's own span and consumed verbatim by the
    /// compiler (which cannot re-derive it: the decision is whether the returned expression is
    /// already a carrier). See [`RetCoerceTable`].
    ret_coerce: RetCoerceTable,
    /// TICKET-227 -- the implicit wraps at typed slots; see [`WrapTable`]. Written only by
    /// `record_wrap`.
    wraps: WrapTable,
    /// W7-49 — side-table keys that were asked to hold two DIFFERENT decisions at once. Filled by
    /// [`record_call_table_entry`] (never by ordinary type errors) and returned alongside the three
    /// tables, because this pass DISCARDS its type errors — `self.error` would be swallowed here.
    /// The compiler turns the first entry into a hard `CompileError`: an aliased key means the
    /// backend cannot tell two expressions apart, which is a silent wrong VALUE, not a slow path.
    table_conflicts: Vec<(Span, String)>,
    /// TICKET-180 — the one record of what each name head means; see [`Resolution`].
    resolutions: ResolutionTable,
    /// TICKET-184 — whether each call head never returns, keyed like `resolutions`: the one input
    /// `flow::stmt` takes beyond the AST ([`Checker::call_diverges`]). Written by
    /// `resolve_path` in EVERY walk and overwritten, because inference passes read it; the
    /// Resolution table itself stays main-pass-only (DEC-180).
    callee_diverges: HashMap<(usize, u32), bool>,
    /// TICKET-184 — fns proved unable to fall off their end; see [`FallOffTable`].
    fall_off: FallOffTable,
    /// Graph index of every module, so a from-imported fn's [`Resolution::Fn`] names its home.
    module_idx_of: HashMap<crate::base::module_id::ModuleId, usize>,
    /// The slot list of every module of the graph (`resolver::graph_global_binds`), in
    /// `module_idx_of` order. A `from` import binds a value exactly when its home's list holds it.
    graph_binds: std::rc::Rc<Vec<Vec<(String, crate::resolver::GlobalSource)>>>,
    /// The current module's from-imported fns: bound name -> (declaring module, declared name).
    fn_homes: HashMap<String, (usize, String)>,
    /// W7-43 — a monotonic counter for the `__opt{n}` binding the Option lowering synthesizes, so a
    /// carrier nested inside another carrier's operand can never shadow it. Mirrors the desugar
    /// walker's `next_tmp`; never reset (uniqueness within one checked graph is free).
    next_opt_tmp: usize,
    /// M24 — the witness TYPE-PARAM names whose `$w:T` binding is reachable at the statement
    /// currently being checked: the witness params of the enclosing MODULE-LEVEL free fn or of the
    /// enclosing MEMBER (Task 5 — a method/static method declares its own `[T]`, and the hidden
    /// argument rides on its frame the same way), and empty everywhere else.
    ///
    /// Task 4 — it CARRIES INTO every nested body (a closure, a `spawn:`/`defer:` block, a nested
    /// `fn`), because the compiler appends the enclosing frame's `$w:T` bindings to that body's
    /// capture entries (`compiler::with_witness_captures`). M24-2 narrowed the compiler side to the
    /// bodies that can REACH a witness, and this side deliberately did NOT follow: the compiler's
    /// capture set is a provable SUPERSET of this scope
    /// (`compiler::nested_body_needs_witness` states why), so a mirroring narrowing here would be a
    /// second, similar-looking rule that can never change an outcome. The witness is a plain
    /// `str` — a closure outliving its defining frame, and a `spawn:` task, both stay correct. `T.static_method()` is accepted ONLY when `T` is in here, which
    /// is exactly what the compiler can lower (`compiler::FnComp::witness_ref` is the other half).
    /// A type param of the enclosing TYPE (`struct Bx[T]`) is never in here: its
    /// witness would have to live in the instance, which is a different mechanism.
    witness_scope: Vec<String>,
    /// M24-2 — every METHOD NAME in the program that some declaration takes a hidden witness for (a
    /// method of a struct / enum declaring a type param with a static-carrying bound).
    /// Graph-wide and never cleared per module — modules are checked deps-first, so an imported
    /// type's methods are already in here when an importer hoists.
    ///
    /// It is the NECESSARY half of [`Checker::member_call_forwards_a_witness`], never the sufficient
    /// one. A member call gives a pre-type walk no callee to resolve, so the charge asks THIS CALL
    /// SITE what of `decl`'s own it carries; this set answers the other question that site cannot —
    /// whether the callee could take a witness AT ALL. `sink.push(x)` on a builtin `List` satisfies
    /// the call-site half (`x: T`) and is still not a forward, because no declaration anywhere
    /// declares a witness-taking `push`; keying the charge on this set ALONE was the opposite error,
    /// one unpinnable `get` poisoning every `m.get("a")` in the program.
    ///
    /// Keyed on the NAME (all a pre-type walk has) and read off the DECLARATION rather than the
    /// derived [`FnSig::witness_params`], so it needs no fixpoint of its own and the free-fn loop can
    /// consult it while method charges are still unfinished. Both approximations err toward CHARGING.
    witness_member_names: std::collections::HashSet<String>,
    /// M24 — the manifest `[project] entrypoint`'s FUNCTION name, when this check is for a bare
    /// `chezzi run` (`check_graph_with_entry`). `None` for `chezzi check <file>` and every library
    /// caller: a file run never invokes a function by name.
    entry_fn: Option<String>,
    /// The CURRENT module's graph index, maintained across every graph pass (set per module in
    /// `run_graph_pass`), so a recorded keyword-call permutation / witness entry is keyed under the
    /// SAME index the backends derive. `0` for a lone `check` (single synthetic module).
    graph_module_idx: usize,
    /// The string-interpolation fragment CONTEXT for keyword-call keying: the whole-string span of the
    /// interpolation currently being inferred (or `Span::default()` outside interpolation) paired with
    /// [`Self::kw_frag_ord`], the fragment's 0-based index in that string. A fragment's spans are its
    /// real source positions, so span alone tells two fragments apart; this pair is belt-and-braces
    /// in the [`WitnessKey`] and [`CarrierKey`]. Set (save/restore for nesting) around each fragment
    /// in `check_interp_chunks`; the compiler maintains the identical pair at its own interpolation
    /// boundary.
    kw_frag_ctx: Span,
    kw_frag_ord: usize,
    /// True only while resolving an `extern "lib":` fn's param/return signature. `owned_str` is a
    /// RETURN-ONLY C marshalling form that collapses to `str`; it is legal ONLY inside an extern
    /// signature. This flag licenses `resolve_type`'s `owned_str` arm there and rejects a bare
    /// `owned_str` used as a general type annotation (where it would silently collapse to `str`
    /// with no `import`). Set/reset around the extern fn loop in `hoist_types`.
    in_extern_sig: bool,
    /// Reverse index built once across the whole graph: a user type name → the modules (in graph
    /// load order, deps-first) that declare it. Drives the "import it from <module>" hint when a bare
    /// type name is used without importing its declaring module. NOT cleared per-module.
    types_by_name: HashMap<String, Vec<String>>,
    /// `from`-imported module GLOBALS (`import COUNT from lib.st`) → the dotted module path they came
    /// from. A from-imported global is a SNAPSHOT copy (Python-identical), so REBINDING the bare name
    /// would write a local alias that is silently lost — rejected in `check_assign`, consistent with
    /// the qualified form (`st.COUNT = 5`). Mutating THROUGH the binding (`LST.push(7)`) is untouched:
    /// a container is the same heap object. Per-module: cleared in `begin_module`.
    imported_values: HashMap<String, String>,
    /// The module-owned builtin types (`builtin_type::builtin_owner`: `Shared`, `Socket`, `ptr`,
    /// `int32`, …) a bare name denotes in the *current* module: bind name → the member's own name.
    /// An import fills it like any other import (`import Shared as S from std.concurrency` binds
    /// `S`), and a std module binds the builtins it owns. A name outside it is not a builtin here.
    /// Per-module: cleared in `begin_module`.
    builtin_binds: HashMap<String, String>,
    /// Label of the module currently being checked (`None` = entry); prefixes its error messages.
    current_module_label: Option<String>,
    /// How many enclosing `for`/`while` loops we're inside *within the current function body*.
    /// Reset to 0 when descending into a (nested) function or closure body so an inner `break`
    /// can't escape into an outer loop. `> 0` ⇒ `break`/`continue` are legal here.
    loop_depth: usize,
    /// True while inferring a generic ctor/call/method's args in the unification PREPASS
    /// ([`Checker::infer_generic_arg_tys`]). In that pass an unannotated closure param must stay
    /// `Ty::Unknown` (so unification is driven by the other args / substituted slot type, then the
    /// per-arg [`Checker::check_generic_arg`] re-infers it in checking-mode) — the free-body scan
    /// (sources #2/#3) must NOT pin it here, or a body use like `x.upper()` would force `x: str` and
    /// corrupt unification (e.g. `Mapped(int_iter, fn(x): x.upper())`).
    generic_arg_prepass: bool,
    /// TICKET-225 (R5) — the type-variable store (`tyvar::TyVars`). A `RefCell` because `assignable`
    /// and `join_ty` bind vars and are `&self`. Speculative state: `DiagMark` carries its mark.
    pub(super) tyvars: std::cell::RefCell<tyvar::TyVars>,
    /// Expected-type HINT (checking-mode) for the OUTERMOST generic ctor / generic fn-call currently
    /// being inferred — pure transport from the three annotation sites (a `let`-binding's declared
    /// type, a `return`'s declared return type, a call argument's declared parameter type) into
    /// [`Checker::infer_call`]. `infer_call` `take()`s it FIRST (before inferring any argument), so a
    /// nested arg call sees `None` and the hint never leaks past the one call it was set for. Consumed
    /// to pre-seed the type-param substitution (via `unify` against the ctor/call's declared return
    /// SHAPE) AFTER arg-unification but BEFORE `report_uninferable_closure_params`, so it fills ONLY
    /// the params the arguments left free — turbofish > args > annotation. This breaks the
    /// `Heap([], fn(x, y): x < y)` deadlock: the annotation pins `T`, which then pins the closure
    /// params. Mirrors the existing closure-vs-fn-annotation checking-mode (`infer_arg`).
    expected_hint: Option<Ty>,
    /// TICKET-227 -- the node `expected_hint` was installed FOR (its slot's value). Only `infer` of
    /// that node may wrap it into the hint's carrier; every other reader treats the hint as a seed.
    hint_owner: Option<crate::ast::NodeId>,
    /// TICKET-269 -- a member call head's receiver, inferred once by `bracket_is_index` and taken
    /// by the chosen reading's own `infer` of that node, so a `recv.name[T](args)` chain is linear.
    handed_recv: Option<(crate::ast::NodeId, Ty)>,
    /// TICKET-227 -- set by `infer` for one `infer_kind_inner` call: the node being inferred owns its
    /// hint. An if/match that owns its hint makes each branch value own it.
    hint_owned: bool,
    /// TICKET-142 (W14-33): true while the checker is inferring the CHILD of a `Binary`/`Unary`
    /// node, so `infer_kind` starts its constant-overflow scan only at the root of a maximal
    /// arithmetic tree (one linear scan per tree, never one per node). Set/restored by the
    /// `infer_kind` wrapper.
    arith_parent: bool,
    /// TICKET-142 (W14-33): total nodes entered by every `consteval::eval`; pinned by a test so the
    /// scan stays linear on a `MAX_AST_DEPTH` chain.
    pub(super) const_scan_visits: usize,
    /// TICKET-142 (W14-33): spans whose constant-overflow error is already reported, so a
    /// re-inferred root does not report twice. Speculative state: `DiagMark` snapshots and
    /// `diag_rollback` restores it.
    const_overflow_seen: std::collections::HashSet<Span>,
    /// TICKET-268: `Binary` node span -> its op and the fault its constant right operand gives every
    /// int left operand. The tree scan notes it; `infer_binary` reports it once the left type is
    /// `int`. A function of the AST alone (a span carries its module), so no `DiagMark` entry.
    const_operand_faults:
        std::collections::HashMap<Span, (BinaryOp, crate::ast::consteval::IntFault)>,
    /// True while checking a `std.*` module — structs hoisted now are tagged `StructOrigin::Builtin`.
    current_module_is_stdlib: bool,
    /// Phase 4c-net — the harvested `StructInfo` (method table) for std.net's reserved `Socket` /
    /// `Listener` handles, captured from the file-backed `std/net.chz` harvest the first time std.net is
    /// checked in the graph. `Socket`/`Listener` resolve to the RESERVED `Ty::Socket`/`Ty::Listener`
    /// (opaque VM handles, NOT nominal structs), so — unlike Match/Response — their layout is not seeded
    /// for field access; only the METHOD table is re-seeded (bare, into `self.structs["Socket"]` /
    /// `["Listener"]`) by [`seed_stdlib_structs`] so `socket.read(...)` / `listener.accept(...)` resolve
    /// via the normal method path in every module. Bare-name annotation stays import-gated by
    /// `builtin_binds` + `resolve_type`'s builtin arm — the seed adds NO bare licensing. `None` until
    /// std.net is harvested (and on the single-module `check` path, which has no graph).
    net_socket_seed: Option<StructInfo>,
    net_listener_seed: Option<StructInfo>,
    /// R2 — the harvested `StructInfo` (method table) for std.io's reserved `Writer` handle, captured
    /// from the file-backed `std/io.chz` harvest the first time std.io is checked in the graph. Like
    /// `net_socket_seed`, `Writer` resolves to the RESERVED `Ty::Writer` (opaque VM handle) — only the
    /// METHOD table is re-seeded (bare, into `self.structs["Writer"]`) by `seed_stdlib_structs` so
    /// `w.write(...)` / `w.close(...)` resolve via the normal method path in every module. Bare-name
    /// annotation is bound by an import (`builtin_binds`). `None` until
    /// std.io is harvested.
    io_writer_seed: Option<StructInfo>,
    /// R2b — the harvested `StructInfo` (method table) for std.io's reserved `Reader` handle — a
    /// SEPARATE field from `io_writer_seed` (one per type, so neither method table clobbers the other),
    /// re-seeded bare into `self.structs["Reader"]` by `seed_stdlib_structs`. `None` until std.io is
    /// harvested.
    io_reader_seed: Option<StructInfo>,
    /// Phase 4c-concurrency — the harvested `StructInfo` (method table) for each of std.concurrency's
    /// four reserved GENERIC handles (`Shared`/`RwShared`/`Atomic`/`Executor`), keyed by bare name,
    /// captured from the file-backed `std/concurrency.chz` harvest (AFTER `attach_native_module_metadata`
    /// ran the closure-param metadata port for `RwShared.read`/`Executor.submit`) the first time
    /// std.concurrency is checked in the graph. Like `net_socket_seed`, these resolve to the RESERVED
    /// `Ty::Shared`/etc (opaque VM handles, NOT nominal structs) — only the METHOD table is re-seeded
    /// (bare, into `self.structs[name]`) by [`seed_stdlib_structs`] so `s.set(...)` / `a.cas(...)`
    /// resolve via the normal method path in every module, with each generic method sig's `Ty::Param`
    /// substituted with the value's element type at the call site. Bare-name annotation stays
    /// import-gated by `builtin_binds` + `resolve_type`'s builtin arm. Empty until harvested.
    concurrency_seeds: HashMap<String, StructInfo>,
    /// Phase 5a-containers — the harvested `StructInfo` (method table) for each of the three RESERVED
    /// UNIVERSE container types (`List`/`Map`/`Set`), keyed by bare name, captured from the always-linked
    /// `std/prelude.chz`'s `native struct` decls the first time the prelude module is checked in the graph
    /// (order[0], before entry/all others). Like `concurrency_seeds`, these resolve to
    /// the RESERVED `Ty::List`/`Ty::Map`/`Ty::Set` (NOT nominal structs) — only the METHOD table is
    /// re-seeded (bare, into `self.structs[name]`) by [`seed_stdlib_structs`] so `xs.push(...)` /
    /// `m.get(...)` / `s.insert(...)` resolve via the normal method path, with each generic method sig's
    /// `Ty::Param` substituted with the value's element/key/value type at the call site. The literal
    /// syntax + turbofish ctor stay compiler-wired (`resolve_type`/`builtin_container_sig`). Unlike the
    /// import-gated seeds, these are UNIVERSE (always in scope) — no licensing set gates them. Empty until
    /// harvested.
    container_seeds: HashMap<String, StructInfo>,
    /// The prelude's `native enum` decls, registered as ordinary enums in every module by
    /// `seed_carrier_enums`. Harvested before the prelude itself is checked.
    carrier_seeds: CarrierSeeds,
    /// Bare variant names of the current module: bound name → the variant an `import V from Enum`
    /// binds it to. The only table that makes a bare name a variant; the prelude's own two imports
    /// feed it through `carrier_seeds`.
    imported_variants: HashMap<String, ImportedVariant>,
    /// The signatures of the eight migrated universe builtins (`ord`/`chr`/`panic`/`int`/`float`/
    /// `str`/`bytes`/`bytearray`), harvested from the always-linked `std/prelude.chz`'s `native
    /// fn`/`native ctor` decls (phase 3a). This REPLACES the hand-built `sig_ord`/… Rust functions —
    /// the `.chz` decls are now the single source of truth for these signatures. `builtin_sig` reads
    /// this registry (falling back to the still-hard-coded `print`/container arms). Populated once when
    /// the prelude module is hoisted (it's always linked before the entry, so this is filled before any
    /// user module's inference reads it). Empty on the lone single-module `check` path (no graph → no
    /// prelude), where `builtin_sig` returns `None` for these names — the same as before this table for
    /// hover-only queries; every real program goes through `check_graph` with the prelude linked.
    native_prelude_sigs: HashMap<String, FnSig>,
    /// TICKET-255 — set per module by `run_graph_pass` from [`ModuleGraph::skips_range_methods`]: the
    /// prelude `Range` method bodies are not checked, because the graph builds no `Range` value.
    skip_range_bodies: bool,
    /// TICKET-265 — set per module by `run_graph_pass` from [`ModuleGraph::fn_skips`]: the top-level
    /// fns of a std module that the program does not reach. Their bodies are not checked.
    skip_fn_bodies: crate::resolver::SkippedFns,
    /// Module-scoped types: `(declaring module id, bare type name) → runtime key`. Bare in the
    /// no-collision case; `<dotted>::Name` on a genuine cross-module clash. Built once in
    /// [`check_graph`] with the IDENTICAL graph-order, first-declarer-wins-bare rule as the compiler's
    /// `assign_type_keys`, so the checker and compiler agree on every type's runtime key (and thus on
    /// which declarer is disambiguated). A name absent from this map keys bare. NOT cleared per-module.
    type_keys: HashMap<(ModuleId, String), String>,
    /// The id of the module currently being checked (`None` for a lone `check`), so local type
    /// declarations + bare annotations resolve to THIS module's runtime key. Set per `check_module`.
    current_module_id: Option<ModuleId>,
    /// The CURRENT module's bare-resolvable type names → their runtime key: locally declared +
    /// `from`-imported + std whole-module. Resolves a bare-written
    /// type name (annotation / constructor) to the module-scoped runtime key. Rebuilt per module.
    bare_types: HashMap<String, String>,
    /// EDITOR HOVER (LSP): when `Some`, a single-position probe — the 1-based `(line, col)` of the
    /// token under the cursor. `None` for an ordinary check (zero overhead). See [`hover_type`].
    hover_probe: Option<(u32, u32)>,
    /// The entry module the probe applies to; recording is gated on `current_module_id == hover_entry`
    /// so a same-named symbol in an imported dependency can't shadow the entry-buffer hit.
    hover_entry: Option<ModuleId>,
    /// First probe hit: the inferred type + its classification + the symbol's doc-comment (if any).
    /// First-write-wins (children infer before parents; only leaves/bindings record), so the smallest
    /// covering symbol's type is kept.
    hover_result: Option<(Ty, HoverKind, Option<String>)>,
    /// EDITOR HOVER: doc-comment for the entry module's non-fn type decls (struct/enum/protocol/
    /// alias) + top-level lets, keyed by simple name. Populated per module from the AST in
    /// `collect_docs`; consulted by the hover Ident/type-name sites. Keyed by simple name is safe
    /// because hover only fires in the entry module (gated by `current_module_id == hover_entry`),
    /// where this table holds exactly that module's decls (mirrors how `functions` is entry-scoped).
    /// Free fns / methods carry their doc on `FnSig::doc` instead. Runtime-inert (LSP only).
    name_docs: HashMap<String, String>,
    /// TICKET-139 (W14-2) — bindings CERTAIN to hold one known function: an unannotated single-name
    /// `:=` of a top-level user fn or a closure literal, or a nested `fn`'s own name. Only a
    /// keyword call through such a binding is legal (labels are surface-only, so any other callee
    /// may hold a fn with different parameter names). Keyed `(owning_scope_idx, name)`; drained at
    /// `pop_scope` (the next pushed scope reuses the index). TICKET-197: the value lists the keys of
    /// the bindings this binding's certainty was read from (`w := v` over a nested fn lists `v`),
    /// so a write to any of them voids it.
    kw_certain: HashMap<(usize, String), Vec<(usize, String)>>,
    /// TICKET-139 (W14-2) — bindings written after their declaration: a `check_assign` Ident write
    /// or a same-scope re-declaration. Same key and drain as `kw_certain`.
    kw_written: std::collections::HashSet<(usize, String)>,
    /// TICKET-139 (W14-2) — every accepted keyword call through a `kw_certain` binding, settled at
    /// the binding's `pop_scope` against `kw_written` so the verdict does not depend on whether the
    /// write comes before or after the call. Deduplicated by `(keys, use)` (DEC-025: a closure body
    /// is inferred more than once). The span is the keyword call's.
    /// TICKET-197: an entry waits on every key it lists (the binding, then its `kw_certain`
    /// dependencies); each key settles at its own scope's `pop_scope`, and a write to any voids it.
    kw_pending: Vec<(Vec<(usize, String)>, Span)>,
    /// TICKET-238 -- where the statement being checked began (`Checker::with_stmt_mark`).
    stmt_mark: StmtMark,
    /// TICKET-238 -- reads of a `hole_rejected` binding so far. Written during a body walk, so
    /// `DiagMark` saves and restores it.
    poison_reads: usize,
    /// TICKET-157 (W12-12) — memo of the SPECULATIVE `infer_inline_fn_ret` a nested un-annotated `fn` runs
    /// (`sig.rs`'s nested-fn arm), keyed by the decl's name span: the inferred return type, then the
    /// errors and warnings that inference emitted AFTER its own `diag_rollback` (the finalize
    /// diagnostic). Everything the walk did before that rollback is erased by it (`DiagMark`), so a
    /// hit replays these two lists and nothing else. `check_fn_body` is NEVER memoized. Cleared at the top of every OUTERMOST
    /// fn walk (`!in_fn_body`), so an entry never outlives one `infer_returns` pass.
    ret_memo: HashMap<Span, (Ty, Vec<CheckError>, Vec<CheckError>)>,
    /// `false` turns the memo off (test-only twin entry points compare with and without it).
    memo_enabled: bool,
    /// `CHEZZI_MEMO_VERIFY` — on a memo hit recompute the inference and print `MEMO MISMATCH` when it
    /// differs from the stored entry. A debugging gate, off by default.
    memo_verify: bool,
    /// Re-entrancy guard for `memo_verify`: while set, the recompute's own nested lookups are served
    /// from the memo, so verification costs one extra walk per hit, not a second exponential tree.
    memo_verifying: bool,
    /// TICKET-233 — `check_fn_body` entries of this pass. A cost counted, not timed, so CPU load
    /// cannot move it; read by [`check_counting_fn_body_checks`].
    #[cfg(any(test, feature = "test-api"))]
    fn_body_checks: usize,
    /// TICKET-253 — `index_value` entries of this pass; read by [`check_counting_index_values`].
    #[cfg(any(test, feature = "test-api"))]
    index_value_checks: usize,
    /// TICKET-269 — member-head receivers `bracket_is_index` inferred in this pass; read by
    /// [`check_counting_bracket_receivers`].
    #[cfg(any(test, feature = "test-api"))]
    bracket_receivers: usize,
}

mod args;
mod boundary;
mod branch;
mod builtin_call;
mod builtin_type;
mod closure;
mod ctor_call;
mod exhaust;
mod expr;
mod flow;
mod fn_call;
mod fn_value;
mod fn_writes;
mod globals;
mod infer;
mod infer_access;
mod infer_ops;
pub mod json_decode;
mod method_call;
mod pattern;
mod resolve;
// `pub(crate)` for `proto::INTRINSIC_PROTO_METHODS` — the intrinsic-grant ↔ VM-arm pairing table,
// which `vm::tests::intrinsic_grants_all_have_vm_arms` reads to assert the pairing (W6-3).
mod assign;
mod assignable;
mod entry;
mod eq;
mod fn_body;
mod fresh;
mod generic_call;
mod harvest;
mod hoist;
mod hooks;
mod infer_ret;
mod iter_index;
mod module_imports;
mod op_kinds;
pub mod proto;
mod resolve_ctype;
mod resolve_type;
mod satisfies;
mod scope;
mod setup;
mod sig;
mod stmt;
pub mod suggest;
mod targs;
mod tyvar;
mod unused;
mod witness_sig;
mod writes;
pub use entry::*;
mod tables;
pub use tables::*;
mod graph_pass;
mod ty_util;
pub(crate) use ty_util::*;
mod builtin_methods;
use builtin_methods::*;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod graph_tests;
