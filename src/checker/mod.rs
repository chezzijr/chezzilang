//! M4 — the type checker. A static pass between parse and run that catches type errors *before*
//! any code executes, collecting **all** errors (Go-style) rather than stopping at the first.
//!
//! Design: pragmatic local inference (see `ty.rs`). Explicit function signatures give us call
//! types for free; locals are inferred from their initializers. [`Ty::Unknown`] suppresses
//! cascades. Two passes: pass 1 hoists every top-level declaration (so forward references work,
//! matching the serial-VM parity oracle's hoist); pass 2 walks bodies and accumulates errors.

mod ty;

use crate::ast::{
    AssignOp, BinaryOp, Block, Bound, CompClause, CompKind, DeferTarget, Expr, ExprKind, FnDecl,
    Import, LitPattern, MethodSig, NativeDecl, Param, Pattern, Span, SpawnTarget, Stmt, StmtKind,
    Type, TypeParam, UnaryOp, WaitArm, WaitArmKind, WaitTarget,
};
use crate::native::cffi::CType;
use crate::resolver::{ModuleGraph, ModuleId, ResolvedImport};
use std::collections::HashMap;
use std::fmt;

pub use ty::Ty;
use ty::compatible;
pub use ty::{
    CarrierKey, CarrierMode, CarrierTable, FnLabels, KeywordKey, KeywordTable, ProtoEqTable,
    WitnessCallee, WitnessKey, WitnessSrc, WitnessTable,
};

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
}

/// Resolved C signatures for every `extern` fn in a module graph, keyed by `(graph module index,
/// fn name)` — the SAME index both backends derive (`compile_graph`'s enumerate / interp's
/// `module_idx_of`). The backends consume these instead of re-resolving alias names themselves, so
/// every alias spelling (local chain, named-import hop, qualified hop, mixed) resolves in its own
/// module's scope, collision-proof by construction. Produced by [`resolve_extern_signatures`].
pub type ExternTable = HashMap<(usize, String), ExternCSig>;

/// The harvested SHAPE of a `native enum` (phase 5b): its variant map (variant name → resolved payload
/// types, with the enum's type params left as `Ty::Param`) plus any leading-`self`-stripped method
/// table. Produced by [`Checker::harvest_native_enum_table`] purely as the drift-guard mirror for the
/// reserved `Option`/`Result` shapes — never a runtime-consumed table.
type NativeEnumShape = (HashMap<String, Vec<Ty>>, HashMap<String, FnSig>);

/// What a `match` scrutinee is being matched against, threaded through the match-checking helpers.
enum MatchKind {
    /// Enum/Result/Option scrutinee — arms are variant patterns.
    Variants {
        label: String,
        variants: HashMap<String, Vec<Ty>>,
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

/// A type error, with the source span it occurred at. Mirrors `ParseError` / `RuntimeError`.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckError {
    pub message: String,
    pub span: Span,
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "type error ({}): {}", self.span, self.message)
    }
}

/// The dotted path an import targets, for error messages (`core.db`).
fn module_label(import: &Import) -> String {
    let path = match import {
        Import::Module { path, .. } | Import::From { path, .. } => path,
    };
    path.join(".")
}

/// `Result`/`Option` are builtin generic types — the `?` operator and the top-level-error logic in
/// both engines key on these literal names, so a user type that shadows them would collide. Reject
/// the redefinition at declaration. `Iterator` is likewise reserved: as a value type it names the
/// experimental generator existential (`Ty::Struct("Iterator", [T])`) that `iter_elem` / `.next()`
/// typing key on, so a user `struct Iterator[T]` would be silently shadowed (and crash at runtime
/// on a phantom `.next()`).
///
/// The same hazard applies to EVERY bare name `resolve_type` maps to a builtin: a user `struct int`
/// / `enum List` / `struct Socket` would type-check clean (the decl guards only consulted this set)
/// and then the use-site (`x: int` / `l: List[int]` / `s: Socket`) would silently resolve to the
/// builtin, yielding an unreachable user type + self-contradictory diagnostics (`expected Socket,
/// found Socket`). So the builtin SCALAR (`int`/`float`/`bool`/`str`/`bytes`/`bytearray`/`nil`),
/// CONTAINER (`List`/`Set`/`Map`/`Channel`/`range`), and HANDLE (`Socket`/`Listener`/`ptr`/
/// `owned_str`) type names are all reserved at declaration too — `struct X` is rejected with the same
/// `type 'X' is reserved (builtin)` error `struct Result` already gives. (The fixed-width FFI integer
/// names like `int32` are reserved via `native::ffi::TYPE_NAMES` at the decl guards, not listed here.)
fn is_reserved_type(name: &str) -> bool {
    name == "Result"
        || name == "Option"
        || name == "Iterator"
        // Builtin scalar primitives — a user `struct int` / `enum str` would shadow the scalar arm in
        // `resolve_type`.
        || name == "int"
        || name == "float"
        || name == "bool"
        || name == "str"
        || name == "bytes"
        || name == "bytearray"
        || name == "nil"
        // Builtin container/collection type names — recognized by `resolve_type`'s generic arms
        // (`List[T]`/`Set[T]`/`Map[K,V]`/`Channel[T]`) and `range` (a reserved callable whose ctor a
        // `struct range` would silently shadow). (List/Map/Set/range's `CallBuiltin` DISPATCH is
        // table-sourced — the `Intrinsic::Ctor` PRELUDE rows; their generic TYPE-IDENTITY is HERE +
        // in `resolve_type`/`infer_named_call`. See the `Intrinsic` doc.)
        || name == "List"
        || name == "Set"
        || name == "Map"
        || name == "Channel"
        || name == "range"
        // `tuple` is the global structural tuple type (values exist only via `(a, b)` literals /
        // destructuring, never a from-nothing ctor). It carries no resolvable bare-type or ctor, so a
        // user `struct tuple` shadows nothing reachable — but it's a documented global (CLAUDE.md's
        // "global surface", the hover table), and a global reserved name is a one-way ratchet, so it is
        // reserved at declaration alongside its container siblings for consistency.
        || name == "tuple"
        // Runtime handle TYPE names: the std.net TCP handles (`Socket`/`Listener`) and the FFI
        // marshalling primitives (`ptr` and the return-only `owned_str`). Reserved at declaration even
        // though their USE is import-gated (std.net / std.ffi) — the two gates are independent, exactly
        // as for the concurrency ctors below.
        || name == "Socket"
        || name == "Listener"
        // R2 — the std.io `Writer` write-only file/stream handle. Reserved at declaration even though
        // its USE is import-gated (std.io) — the two gates are independent, like Socket/Listener above.
        || name == "Writer"
        // R2b — the std.io `Reader` read-only file handle (the read twin of `Writer`). Same dual gate.
        || name == "Reader"
        || name == "ptr"
        || name == "owned_str"
        // The four runtime concurrency ctor/TYPE names stay RESERVED — a user `struct Shared` /
        // `struct Executor` is rejected at declaration (a clean `reserved` error), NOT silently
        // shadowed by the builtin ctor. This is SEPARATE from the `import std.concurrency` gate (which
        // governs USE): the names are reserved AND require the import to use the builtin. (`Executor`
        // was already reserved here; `Shared`/`RwShared`/`Atomic` join it so all four behave alike.)
        || name == "Shared"
        || name == "RwShared"
        || name == "Atomic"
        || name == "AtomicInt"
        || name == "Executor"
        // `timer` is a runtime ctor/builtin name (not a real type), but it STAYS reserved here so a
        // user `struct timer` / `enum timer` / `type timer` is rejected at declaration rather than
        // silently shadowed by the opcode-backed builtin. SEPARATE from the `import std.time` gate
        // (which governs USE): the name is reserved AND requires the import to call `timer(ms)`.
        || name == "timer"
}

/// The note appended to a `float`-sink mismatch whose actual expression is a TYPED int — the rule is
/// Go's: an untyped int CONSTANT adapts to a float context, a typed int VALUE never does. Empty for
/// any other mismatch, and empty when the offending expression IS an untyped int constant (it was
/// rejected because the sink does not widen at all — a builtin-method arg, an enum payload, a call
/// through a function VALUE — not because it is typed; claiming otherwise would be a lie).
fn widen_note(expected: &Ty, actual: &Ty, e: &Expr) -> &'static str {
    if matches!((expected, actual), (Ty::Float, Ty::Int)) && !crate::ast::untyped_int_const(e) {
        " (a typed int never widens to float — write float(x))"
    } else {
        ""
    }
}

/// The collection element-widening hint derived from a RESOLVED `let` annotation: `List[float]` →
/// `Elem`, `Map[_, float]` → `MapValue`. The `Ty` twin of the compiler's `Compiler::float_elem_hint`
/// (which resolves the same thing from the syntactic `Type`, aliases included).
/// (`Set[float]` is impossible — float is not Hashable — so it is intentionally not handled.)
fn float_elem_hint_ty(ty: &Ty) -> Option<crate::ast::ElemFloatHint> {
    match ty {
        Ty::List(e) if **e == Ty::Float => Some(crate::ast::ElemFloatHint::Elem),
        Ty::Map(_, v) if **v == Ty::Float => Some(crate::ast::ElemFloatHint::MapValue),
        _ => None,
    }
}

/// A short, surface-faithful label for a return-only extern `Type` in a marshallability error
/// (`owned_str`, `str?`, `owned_str?`). Only ever called on the forms `is_return_only_extern_type`
/// already matched, so non-matching shapes fall back to a generic label.
fn describe_extern_type(t: &Type) -> String {
    match t {
        Type::Named { name: n, .. } => n.clone(),
        Type::Generic(n, args, ..) if n == "Option" => match args.first() {
            Some(Type::Named { name: inner, .. }) => format!("{inner}?"),
            _ => "str?".to_string(),
        },
        _ => "owned_str".to_string(),
    }
}

/// Names an `extern` C fn may NOT take: a builtin (`len`/`range`/`int`/`float`/`str`/`ord`/`chr`/
/// `set`), `print`, or a runtime constructor (`Channel`/`Shared`/`RwShared`/`Atomic`/`timer`/`Executor`). Both
/// backends resolve these names to a special op *before* a plain named call (`compiler::compile_call`
/// / `interp::eval_call`), so an extern fn with one of these names is silently shadowed — dead code
/// that the compiler's eager `MakeCffi` would still `dlsym` (aborting on a symbol it can never call).
/// Mirrors `compiler::is_builtin` + the constructor/`print` special cases. (Struct- and variant-name
/// collisions are caught separately against the built registries, since those are user-declared.)
/// Every reserved CALLABLE builtin name: the free functions (`print`/`panic`/`range`/`int`/`float`/
/// `str`/`ord`/`chr`) and the container/runtime constructors (`List`/`Set`/`Map`/`bytes`/`bytearray`/
/// `Channel`/`Shared`/`RwShared`/`Atomic`/`timer`/`Executor`). Every name here is CALLED (none is a
/// pure type marker), so each must have a [`builtin_sig`] entry for editor hover — enforced by the
/// `reserved_callables_all_have_builtin_sig` drift-guard test. (The generic ctors `Ok`/`Err`/`Some`
/// are NOT reserved — a user may shadow them — so they are intentionally out of this set and keep
/// hovering `None` for v1.) Mirrors `compiler::is_builtin` + the constructor/`print` special cases.
const RESERVED_CALLABLE: &[&str] = &[
    // builtins (mirrors compiler::is_builtin / interp::builtins::is_builtin)
    "range",
    "int",
    "float",
    "bool",
    "str",
    "ord",
    "chr",
    "Set",
    "List",
    "Map",
    "bytes",
    "bytearray",
    // the special print op
    "print",
    // the diverging panic(msg) op (raises a recoverable RuntimeError; bottom-typed)
    "panic",
    // runtime constructors the backends special-case before a plain call
    "Channel",
    "Shared",
    "RwShared",
    "Atomic",
    "AtomicInt",
    "timer",
    "Executor",
];

fn is_reserved_name(name: &str) -> bool {
    RESERVED_CALLABLE.contains(&name)
}

/// True iff `name` may NOT be an import-alias TARGET — a reserved builtin, whether CALLABLE
/// (`import x as int`) or a pure reserved TYPE name (`import x as Result`). Aliasing TO either
/// silently rebinds the builtin (the builtin wins at call/type sites, the import binding is dead),
/// so both are rejected `reserved (builtin)`, symmetric with the struct/enum/type DECL guard which
/// already rejects all of these via `is_reserved_type`. Reuses that same predicate — no second list.
/// EXCEPTION: `nil` is a shadowable value-builtin (`nil := 5` is accepted, unlike `true := 5` which
/// is a parse error), NOT a type, so it is carved out of the type-name reject to avoid over-rejecting
/// a legit `import x as nil` — the one name `is_reserved_type` lists that is a value, not a type.
fn is_reserved_alias_target(name: &str) -> bool {
    is_reserved_name(name) || (is_reserved_type(name) && name != "nil")
}

/// The four BUILT-IN variant constructors of `Result`/`Option`. They are NOT in `is_reserved_type`
/// (a user may deliberately shadow them at a DECL site — see that fn's doc), so they need their own
/// predicate for the places that must recognize them as builtin ctors.
pub(super) fn is_builtin_variant(name: &str) -> bool {
    matches!(name, "Ok" | "Err" | "Some" | "None")
}

/// True iff `name` may NOT be the bound name of a module import — ALIASED (`import lib.geo as Ok`)
/// or UN-aliased (the last path segment: `import lib.int`). A module bind lands in the VALUE
/// namespace, where it beats a same-named builtin/ctor in EXPRESSION position (`import std.str` used
/// to make `str(5)` fail with "module str is not callable"). A RESERVED bound name is therefore
/// rejected — the module stays usable under a non-reserved alias (`import lib.int as ints`).
/// Covers reserved CALLABLES + reserved TYPE names (`is_reserved_alias_target`) + `nil` + the builtin
/// variant ctors. `nil` is carved out of `is_reserved_alias_target` because a from-import ALIAS binds
/// a VALUE (and a value still works as a value); a MODULE is not a value, so `import lib.nil` /
/// `import m as nil` would silently retype the `nil` literal — reject it here.
/// The FROM-import path guards the same VALUE namespace with `is_reserved_alias_target ||
/// is_builtin_variant` (this predicate minus `nil`) — see `bind_import`.
/// RESIDUAL (deliberately NOT gated here): a module bind colliding with a USER struct/enum ctor of
/// the same name (`import lib.Point` + `struct Point`) still wins in expression position. Unlike a
/// reserved name that would be silently destroyed, this one is a hard TYPE ERROR at the ctor call and
/// the alias is the cure (Python-normal), so it stays a DIAGNOSTIC — the not-callable arm in
/// `expr.rs` names the collision. Rejecting it here would need the checker to know the user's type
/// names at import-bind time; a real module namespace is the principled fix, and is a resolver change.
pub(super) fn is_reserved_module_bind(name: &str) -> bool {
    is_reserved_alias_target(name) || name == "nil" || is_builtin_variant(name)
}

/// The kind of intrinsic a universe builtin lowers to on a DIRECT call. `Print` → the dedicated
/// `Op::CallPrint`/`Op::CallPrintSep` opcodes (variadic + `sep=`/`end=`); `Builtin` →
/// `Op::CallBuiltin(name, argc)` dispatched by name in the VM's `do_builtin`; `Ctor` → likewise
/// `Op::CallBuiltin` — the phase-2a scalar-conversion constructors (int/float/str/bytes/bytearray) AND
/// the phase-2b GENERIC / reserved-type container constructors (range/List/Map/Set). The
/// `Print`/`Builtin` kinds back FIRST-CLASS fn values; `Ctor` rows are NON-first-class (types are not
/// first-class values — uniform with user struct ctors), so a `Ctor` row never emits a
/// `LoadBuiltin`/`Ty::BuiltinFn`. This table is the single source of truth for the `is_builtin` /
/// `CallBuiltin` DISPATCH + name-set; a container ctor's generic TYPE-IDENTITY (`List[int]` →
/// `Ty::List(Int)`, the Map hashable-key check, range arity) is NOT a flat `FnSig` and stays in
/// `resolve_type`/`infer_named_call` (dispatch here, identity there). Metadata only — runtime dispatch
/// stays name-keyed and unchanged (the `NativeFn` host seam only accepts int/str/map args, which is
/// exactly why `print` needs its own `Value::Builtin`/opcode path).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Intrinsic {
    Print,
    Builtin,
    Ctor,
}

/// One row of the (now HOLLOW) native-prelude METADATA table. Phase 3a split the concern: this table
/// keeps only the name → intrinsic → first_class METADATA — the single source of truth read by the
/// backends (`compiler::is_builtin`, `interp::builtins::is_builtin`, `is_firstclass_builtin_fn`), which
/// have no module-graph access and need only intrinsic/first_class, never a `FnSig`. The SIGNATURES of
/// the eight migrated builtins moved OUT to the always-linked `std/prelude.chz` `native fn`/`native
/// ctor` decls (harvested into [`Checker::native_prelude_sigs`], read by [`Checker::builtin_sig`]).
/// `print` is now file-backed too — a variadic `native fn print(...args: Any, sep, end)` decl in
/// `std/prelude.chz` (no synthetic Rust signature remains); only its fixed 1-arg VALUE form
/// (`p := print`) is still synthesized in `infer_ident`, since the specialized `CallPrint` opcodes
/// are unreachable through a bound value. The two halves are reconciled by
/// `prelude_table_is_single_source_of_truth` so the `.chz` decls stay authoritative.
pub(crate) struct PreludeFn {
    pub(crate) name: &'static str,
    pub(crate) intrinsic: Intrinsic,
    pub(crate) first_class: bool,
}

/// The native prelude (phase 1). The four universe FUNCTIONS that are FIRST-CLASS values (bindable,
/// passable, `defer`-able as a bare call) — unlike type/container/runtime constructors
/// (`int`/`List`/`Channel`/…) and user struct/enum ctors, which stay non-first-class and must be
/// wrapped in a function. First-class fns carry a dedicated runtime value
/// (`Value::Builtin`/`Obj::Builtin`); direct calls still lower to their specialized opcodes, only
/// value-position uses take the first-class path. The SCALAR-CONVERSION constructors
/// (`int`/`float`/`str`/`bytes`/`bytearray`) landed in phase 2a as `Intrinsic::Ctor` rows —
/// `first_class: false` (types are not first-class values), so they source their `CallBuiltin`
/// dispatch metadata from this table but never emit a `LoadBuiltin`/`Ty::BuiltinFn`. Phase 3a moved
/// their SIGNATURES (and `ord`/`chr`/`panic`'s) to `std/prelude.chz` `native` decls; this table now
/// carries only metadata (name/intrinsic/first_class). Phase 2b folded the GENERIC / reserved-type
/// container ctors (`range`/`List`/`Map`/`Set`) in as `Intrinsic::Ctor` rows too — so their
/// `is_builtin`/`CallBuiltin` DISPATCH + name-set flow through THIS one table — but they are NOT
/// `.chz`-declared (they are generic; native ctor generic-decl support is a later, maybe-never
/// concern), and their generic TYPE-IDENTITY stays in `resolve_type`/`infer_named_call`
/// (`builtin_container_sig` supplies only a flat display/placeholder sig). See PROGRESS.md
/// "native-prelude table".
pub(crate) const PRELUDE: &[PreludeFn] = &[
    PreludeFn {
        name: "print",
        intrinsic: Intrinsic::Print,
        first_class: true,
    },
    PreludeFn {
        name: "ord",
        intrinsic: Intrinsic::Builtin,
        first_class: true,
    },
    PreludeFn {
        name: "chr",
        intrinsic: Intrinsic::Builtin,
        first_class: true,
    },
    PreludeFn {
        name: "panic",
        intrinsic: Intrinsic::Builtin,
        first_class: true,
    },
    // Phase 2a — the scalar-conversion CTORS (int/float/bool/str/bytes/bytearray). `first_class:
    // false` (ALWAYS, for every Ctor row):
    // a value-position use (`f := int`) stays rejected, uniform with `f := Point` / `f := List`.
    PreludeFn {
        name: "int",
        intrinsic: Intrinsic::Ctor,
        first_class: false,
    },
    PreludeFn {
        name: "float",
        intrinsic: Intrinsic::Ctor,
        first_class: false,
    },
    PreludeFn {
        name: "bool",
        intrinsic: Intrinsic::Ctor,
        first_class: false,
    },
    PreludeFn {
        name: "str",
        intrinsic: Intrinsic::Ctor,
        first_class: false,
    },
    PreludeFn {
        name: "bytes",
        intrinsic: Intrinsic::Ctor,
        first_class: false,
    },
    PreludeFn {
        name: "bytearray",
        intrinsic: Intrinsic::Ctor,
        first_class: false,
    },
    // Phase 2b — the four GENERIC / reserved-type container CTORS. `first_class: false` (uniform with
    // every Ctor row): `f := List` / `f := range` stay checker errors. The table is the DISPATCH
    // single-source (`is_builtin`/`CallBuiltin`) + name-set; their generic TYPE-IDENTITY — turning
    // `List[int]` into `Ty::List(Int)`, the Map hashable-key check, the range-arity/overload typing —
    // is NOT a flat `FnSig` and stays in `resolve_type` (generic arms) + `infer_named_call` (ctor
    // return typing); `builtin_container_sig` supplies only the FLAT DISPLAY/PLACEHOLDER sig used for
    // hover/value-position. `range` is a Ctor row but NON-generic (`name_is_generic` false), so
    // `range[int]()` still errors in `infer_named_call` — table membership is orthogonal to genericity.
    PreludeFn {
        name: "range",
        intrinsic: Intrinsic::Ctor,
        first_class: false,
    },
    PreludeFn {
        name: "List",
        intrinsic: Intrinsic::Ctor,
        first_class: false,
    },
    PreludeFn {
        name: "Map",
        intrinsic: Intrinsic::Ctor,
        first_class: false,
    },
    PreludeFn {
        name: "Set",
        intrinsic: Intrinsic::Ctor,
        first_class: false,
    },
];

/// Look up a native-prelude row by name (linear scan over the table's rows).
pub(crate) fn prelude_fn(name: &str) -> Option<&'static PreludeFn> {
    PRELUDE.iter().find(|p| p.name == name)
}

/// True if `name` is a first-class universe builtin fn — the table's `.first_class` view.
pub(crate) fn is_firstclass_builtin_fn(name: &str) -> bool {
    prelude_fn(name).is_some_and(|p| p.first_class)
}

/// THE declaration of the reserved (prebuilt) protocol set — one list, three consumers:
/// [`is_reserved_protocol`] (the redeclaration gate), [`prebuilt_protocols`] (the live runtime seed,
/// whose key set this must EQUAL — asserted by the drift guard), and
/// `Checker::assert_native_protocol_shape_matches` (which iterates this to prove each name's
/// `std/prelude.chz` mirror byte-matches its seed). Previously three hand-maintained copies that
/// nothing forced to agree: `PathLike` sat in the first two and was missing from the third for months,
/// so its shape was the one that could silently drift (`docs/gaps.md` §L4b). Adding a protocol now
/// means adding it HERE and seeding it; omit either and the debug guard fails.
pub(crate) const RESERVED_PROTOCOLS: &[&str] = &[
    "Any",
    "Comparable",
    "Eq",
    "Stringable",
    "Hashable",
    "Error",
    "Add",
    "Sub",
    "Mul",
    "Div",
    "Mod",
    "Neg",
    "Arithmetic",
    "Iterator",
    "Iterable",
    "Index",
    "IndexSet",
    "Slice",
    "Convert",
    "Contains",
    "PathLike",
];

/// Prebuilt protocols a user program may use as bounds but must not redeclare (the
/// [`RESERVED_PROTOCOLS`] membership test). Every caller is a per-DECLARATION hoist/setup check
/// (`hoist_protocol`, the `struct`/`enum`/`newtype`/`type` reserved-name arms), so the linear scan is
/// off any hot path — it sits beside an identical `native::ffi::TYPE_NAMES.contains(..)` scan.
fn is_reserved_protocol(name: &str) -> bool {
    RESERVED_PROTOCOLS.contains(&name)
}

/// True if `name` is one of the four recognized suite lifecycle hooks.
fn is_lifecycle_hook(name: &str) -> bool {
    crate::vm::op::LIFECYCLE_HOOKS.contains(&name)
}

/// A function (or method) signature: parameter types and return type. `type_params` is non-empty
/// only for generic functions (`fn max[T: Comparable]`), where `params`/`ret` contain `Ty::Param`s.
#[derive(Clone)]
struct FnSig {
    params: Vec<Ty>,
    /// Swift-style parameter labels parallel to `params` (the declaration's param names; `None` for
    /// `self` or an unnamed slot). Surface-only — used ONLY to build a labelled `Ty::Func` value type
    /// in `infer_ident` so `g := f; g(name=…)` resolves. Excluded from `fn_sig_eq` (labels are not
    /// type identity). Empty for synthetic/native sigs built via `plain`/`optional_tail`.
    labels: Vec<Option<String>>,
    ret: Ty,
    type_params: Vec<TypeParam>,
    /// `where T: Bound` clauses (empty for the common case). For a NATIVE method sig they name the
    /// enclosing native struct's type param and are enforced at each call site by the container
    /// method-dispatch arm (`enforce_bounds`); for a USER fn they are already MERGED into
    /// `type_params` by `fn_sig`, so this stays empty there. Excluded from `fn_sig_eq` (bound
    /// documentation, not type identity) — see the note there.
    where_bounds: Vec<TypeParam>,
    /// D6c — the minimum number of arguments this signature accepts; `params.len()` for an ordinary
    /// fixed-arity signature (set by [`FnSig::plain`]), but smaller when trailing params are optional
    /// (the net socket ops' optional `timeout_ms`). [`Checker::check_args`] accepts any arg count in
    /// `min_params..=params.len()`.
    min_params: usize,
    /// A struct/enum method whose first parameter is NOT `self` (or which has no params) is a STATIC
    /// (associated) method — called `Type.method(args)`, not `value.method(args)`. `false` for every
    /// instance method, free function, closure, and native sig (the default via `plain`/`optional_tail`);
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
}

impl FnSig {
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
        }
    }

    /// D6c — a non-generic signature whose last `optional` params may be omitted (the net socket ops'
    /// optional trailing `timeout_ms`). `check_args` accepts `params.len() - optional ..= params.len()`.
    fn optional_tail(params: Vec<Ty>, ret: Ty, optional: usize) -> FnSig {
        let min_params = params.len() - optional;
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
    type_params: Vec<TypeParam>,
    fields: Vec<(String, Ty)>,
    methods: HashMap<String, FnSig>,
    origin: StructOrigin,
    /// The struct's own decl docstring (the `#` block above `struct X:`), captured in `capture_sig`
    /// from `name_docs` so it crosses the module boundary to an importer's hover. Editor-only; `None`
    /// for a builtin/native struct and until `capture_sig` populates it. Never read by checking/codegen.
    doc: Option<String>,
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
    embeds: Vec<Bound>,
}

/// A user enum variant: which enum it belongs to and its payload field types.
#[derive(Clone)]
struct VariantInfo {
    enum_name: String,
    payload: Vec<Ty>,
}

/// A module's public surface, computed when it's checked, consumed by its importers: its top-level
/// functions, top-level values (`:=` / typed lets), and the type names it declares.
#[derive(Clone, Default)]
struct ModuleSig {
    functions: HashMap<String, FnSig>,
    values: HashMap<String, Ty>,
    /// Subset of `values` declared `const` (a `const` top-level let, or a native constant). Carried
    /// across the module boundary so an importer's rebind of the name (`import PI from m; PI = x`, or
    /// qualified `m.PI = x`) reports a const-specific message instead of the generic snapshot/field one.
    const_values: std::collections::HashSet<String>,
    types: std::collections::HashSet<String>,
    /// Native functions whose result type follows their argument type (int args → int, float args
    /// → float) instead of the fixed `FnSig` (gap #12: `std.math` `abs`/`min`/`max`). The `FnSig`
    /// still records arity; the result/param strictness is handled by `infer_numeric_poly`.
    numeric_poly: std::collections::HashSet<String>,
    /// Resolved struct definitions this module declares (module-scoped types — D? feature). An
    /// importer injects these into its own per-module `structs`/`struct_names` so a `from`-imported
    /// or qualified-access struct resolves with the right field layout.
    struct_defs: HashMap<String, StructInfo>,
    /// Resolved enum definitions this module declares: variant names (in order), generic type
    /// params, and each variant's payload `VariantInfo`.
    enum_defs: HashMap<String, EnumSigInfo>,
    /// Resolved newtype definitions this module declares: the underlying `Ty` and the name-keyed
    /// methods. An importer injects these into its per-module `newtype_defs`/`newtype_names`.
    newtype_defs: HashMap<String, NewTypeSigInfo>,
    /// Transparent type aliases this module declares, with their body RESOLVED in the DEFINING
    /// module's scope (so a cross-module `type Len = int32` carries its FFI-width license). The bool
    /// records whether the alias was licensed (`ffi_alias_ok`) in the defining module. The
    /// `Option<String>` is the name of an embedded FFI width that the defining module did NOT import
    /// (so the alias is unlicensed): an importer must reject it with "unknown type" — the width can't
    /// be laundered through an unlicensed alias.
    type_aliases: HashMap<String, AliasSig>,
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

/// A newtype's exported shape inside a `ModuleSig`: its resolved underlying `Ty` and its name-keyed
/// methods, ferried across the module boundary so an imported newtype constructs/unwraps/dispatches.
#[derive(Clone)]
struct NewTypeSigInfo {
    underlying: Ty,
    /// The newtype's generic type params (empty for a scalar newtype), ferried across the module
    /// boundary so an imported generic newtype's instantiation/dispatch/cast-unwrap resolves.
    type_params: Vec<TypeParam>,
    methods: HashMap<String, FnSig>,
    /// The newtype's own decl docstring, carried across the module boundary for an importer's hover
    /// (see [`StructInfo::doc`]). Editor-only; never read by checking/codegen.
    doc: Option<String>,
}

/// An enum's exported shape inside a `ModuleSig`: variant names in declaration order, the enum's
/// generic type params, and each variant's resolved `VariantInfo` (payload types).
#[derive(Clone)]
struct EnumSigInfo {
    variant_names: Vec<String>,
    type_params: Vec<TypeParam>,
    variants: Vec<VariantInfo>,
    /// The enum's methods (`fn area(self) …`), name-keyed like `StructInfo.methods`. Ferried across
    /// the module boundary so an imported enum's methods resolve in the importer.
    methods: HashMap<String, FnSig>,
    /// The enum's own decl docstring, carried across the module boundary for an importer's hover
    /// (see [`StructInfo::doc`]). Editor-only; never read by checking/codegen.
    doc: Option<String>,
}

/// Type-check a single parsed module (no imports). Retained as the unit-test entry point; the CLI
/// drives [`check_graph`] so single- and multi-file programs share one path.
///
/// Runs on [`crate::on_frontend_stack_scoped`]'s dedicated stack — see that fn's doc comment. This is
/// the sole reason `check_src`/`ok`/`rejects` and the ~4000 other Rust test-harness callers that go
/// through this fn are no longer the smallest stack in the tree: they now get the same 1 GiB frontend
/// stack production always had, by construction, instead of whatever the ambient test thread happens
/// to be sized at.
#[cfg(test)]
pub fn check(module: &crate::ast::Module) -> Result<(), Vec<CheckError>> {
    crate::on_frontend_stack_scoped(move || {
        let mut c = Checker::new();
        // Single-module path (no graph): the always-linked std/prelude.chz was never hoisted, so seed
        // the eight migrated universe-builtin signatures from it directly (graph path hoists them
        // normally).
        c.seed_native_prelude_sigs();
        c.check_module(&module.stmts, None, &[]);
        if c.errors.is_empty() {
            Ok(())
        } else {
            Err(c.errors)
        }
    })
}

/// Entry point for a multi-file program: type-check every module in the graph (dependencies
/// before dependents), accumulating all errors across all modules (Go-style). User types are
/// MODULE-SCOPED: a type declared in one module is private to it and visible elsewhere only via
/// import. The same type name may appear in several modules.
pub fn check_graph(graph: &ModuleGraph) -> Result<(), Vec<CheckError>> {
    check_graph_with_entry(graph, None)
}

/// [`check_graph`] for the manifest-entrypoint run (`chezzi run` with no file): `entry_fn` is the
/// `[project] entrypoint = "mod:fn"` function name, which the runtime invokes BY NAME with no
/// arguments. That is CLI state the checker cannot discover, and the only thing it changes is one
/// extra rejection (M24: such a function may not take a hidden type witness — see the entry-fn arm
/// in `run_graph_pass`). `None` ⇒ byte-identical to `check_graph`.
///
/// Runs on [`crate::on_frontend_stack_scoped`]'s dedicated stack — see that fn's doc comment. This is
/// the PRODUCTION entry point (`check_graph` delegates here), so this is where the CLI, LSP
/// diagnostics/hover, and `test_runner` all land; wrapping HERE rather than at each of those callers
/// makes the 1 GiB stack a structural guarantee every entry point gets by construction, not a
/// convention each future caller has to remember. Nesting (a caller that also wraps, e.g. the CLI) is
/// harmless — one extra thread.
pub fn check_graph_with_entry(
    graph: &ModuleGraph,
    entry_fn: Option<&str>,
) -> Result<(), Vec<CheckError>> {
    crate::on_frontend_stack_scoped(move || {
        let mut c = Checker::new();
        c.entry_fn = entry_fn.map(str::to_string);
        c.run_graph_pass(graph, false);
        if c.errors.is_empty() {
            Ok(())
        } else {
            Err(std::mem::take(&mut c.errors))
        }
    })
}

/// Classification of the symbol a hover landed on, returned alongside the inferred type display so
/// editor tooling can label it (`local`/`param`/`fn`/`field`/`struct`/literal). Secondary metadata —
/// the type string is the load-bearing payload. `Param` is PRODUCED at param-DECL hover sites (fn /
/// method / closure signature); a param's body-USE still reports `Local` (different span). `Other`
/// covers a leaf the classifier can't bucket.
// `Struct` is part of the public hover-kind contract but not yet produced (a struct-name hover
// currently resolves through other paths) — these variants are also constructed by editor consumers,
// not this crate's default `chezzi` bin (which compiles `checker` privately and reaches hover only
// via the lib's `editor`), so allow the bin-only dead-code lint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum HoverKind {
    Local,
    Param,
    Func,
    Field,
    Struct,
    /// A TYPE token inside an annotation (`x: Id`, a param/return/field type, a `let` annotation):
    /// the hovered name resolves through `resolve_type` and the display is the resolved `Ty`.
    Type,
    Literal,
    Other,
}

/// Editor-tooling introspection (LSP hover): re-run the SAME deps-first checking pass as
/// [`check_graph`] over `graph`, but with a single-position PROBE armed on the ENTRY module. Returns
/// `Some((type_display, kind))` for the leaf expression / binding / field-name whose anchor position
/// is the 1-based `(line, col)` token start — and ONLY when the whole program type-checks (mirrors
/// the "no type when the program doesn't check" contract). `None` otherwise. Behavior-preserving:
/// the probe only records a type already computed by the normal pass; it changes no checker decision.
///
/// Consumed by the lib's `editor::hover` (and through it the `chezzi-lsp` server); the default
/// `chezzi` bin compiles `checker` privately without reaching it, so allow that target's dead-code lint.
///
/// Runs on [`crate::on_frontend_stack_scoped`]'s dedicated stack, like every other fn here that drives
/// `run_graph_pass`. This one was left unwrapped and relied on `editor::hover` wrapping its caller
/// instead — a convention, not a guarantee, and the exact hole that already bit this repo once when
/// hover ran the whole checker on a ~2 MiB LSP tokio worker. Wrapping HERE is what makes the doc block
/// on `EQ_BOUNDS_MAX_NODES` ("every caller is on 1 GiB by construction") true rather than aspirational.
#[allow(dead_code)]
pub fn hover_type(
    graph: &ModuleGraph,
    line: u32,
    col: u32,
) -> Option<(String, HoverKind, Option<String>)> {
    crate::on_frontend_stack_scoped(move || {
        let mut c = Checker::new();
        c.hover_probe = Some((line, col));
        c.hover_entry = Some(graph.entry.clone());
        c.run_graph_pass(graph, false);
        if !c.errors.is_empty() {
            return None;
        }
        c.hover_result
            .take()
            .map(|(ty, kind, doc)| (ty.to_string(), kind, doc))
    })
}

/// FFI ROOT FIX (fix4): resolve the fully-resolved, width-bearing C signature of every `extern` fn
/// in the graph, each in its DEFINING module's import/alias scope — the SINGLE resolver both backends
/// consume so every alias spelling (local chain, named-import hop, qualified hop, mixed) resolves
/// collision-proof by construction. Runs the SAME module-scoped pass as [`check_graph`] (deps-first,
/// `begin_module` / `bind_import` / `module_sigs`) but harvests the extern table and ignores type
/// errors (the error gate is `check_graph`, run separately by the CLI). The returned table is keyed
/// by `(graph module index, fn name)`.
///
/// Runs on [`crate::on_frontend_stack_scoped`]'s dedicated stack — see that fn's doc comment, and
/// `EQ_BOUNDS_MAX_IN_PROGRESS`'s doc in `checker::proto` for why this one matters: `chezzi run`/
/// `chezzi test` call this from `compiler::compile_graph`, itself called from INSIDE the VM's own
/// `VM_STACK_BYTES` thread (384 MiB) — a smaller stack than the checker's other two entry points get,
/// so the `Eq` walk this pass runs (`run_graph_pass`) needs the same 1 GiB floor they do (W7-55
/// Important-3 follow-up).
pub fn resolve_extern_signatures(graph: &ModuleGraph) -> ExternTable {
    crate::on_frontend_stack_scoped(move || {
        let mut c = Checker::new();
        c.run_graph_pass(graph, true);
        std::mem::take(&mut c.extern_sigs)
    })
}

/// Resolve extern C signatures for a SINGLE-FILE (standalone, source-string) program, going through
/// the EXACT SAME [`resolve_extern_signatures`] pass as the multi-file CLI — so there is exactly ONE
/// extern-type resolver in the whole codebase and the backends never re-resolve. Wraps `stmts` in a
/// synthetic one-module [`ModuleGraph`]: id `<main>` (so `module_keys` yields `<main>`, matching the
/// backends' `STANDALONE_MODULE_KEY` struct-identity key), no imports, no native. Width names
/// (`int32`) resolve as reserved leaves; local aliases/structs resolve from the parsed stmts; no
/// qualified / named-import forms exist in single-file source, so nothing else is needed.
///
/// Test-only: the single-file standalone compile/run paths (`compile_module_standalone`,
/// `Interp::execute`) are `#[cfg(test)]`. The production CLI — single- AND multi-file — always goes
/// through `build_graph` → `resolve_extern_signatures`.
#[cfg(test)]
pub fn resolve_extern_signatures_standalone(stmts: &[Stmt]) -> ExternTable {
    let id = crate::resolver::ModuleId(std::path::PathBuf::from("<main>"));
    let graph = ModuleGraph {
        entry: id.clone(),
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
    };
    resolve_extern_signatures(&graph)
}

/// Swift-style keyword arguments through a function VALUE: resolve every value call that carries
/// labels (`g(name="Bob")`) into a positional slot PERMUTATION, keyed `(graph module index, call
/// span)`. Runs the SAME deps-first checking pass as [`check_graph`] (full inference — the callee's
/// labelled `Ty::Func` must be known), but harvests the keyword table and ignores type errors (the
/// error gate is `check_graph`, run separately). Both backends consume the returned table to lower a
/// value+keyword call to a plain positional `Op::Call`, so the runtime ABI stays positional.
///
/// M24 rides the SAME pass and the SAME `harvest_keywords` licence (one extra deps-first check, not
/// two): the [`WitnessTable`] the static-witness lowering consumes comes back alongside.
///
/// W7-43 rides it too, as the third element: the [`CarrierTable`] telling the compiler which
/// lowering each `?.` carrier takes. Unlike the other two it is recorded UNCONDITIONALLY (not gated
/// on `harvest_keywords`) — a gate would be a second way for this pass and [`check_graph`] to
/// disagree about the same program.
/// W7-53 I1′ rides it as the fourth element: the [`ProtoEqTable`] telling the compiler which
/// `.eq(x)` call sites are PROTOCOL dispatch through a generic bound (and so mean `==`) rather than
/// ordinary by-name method calls. Recorded unconditionally, exactly like the carriers.
/// W7-49 rides it as the last element: the key CONFLICTS this pass refused to overwrite. This pass
/// discards its type errors, so a conflict cannot travel as one — the compiler turns the first into a
/// hard `CompileError`. See [`record_call_table_entry`].
/// Runs on [`crate::on_frontend_stack_scoped`]'s dedicated stack — same reason as
/// [`resolve_extern_signatures`]: this is the other checker pass `compiler::compile_graph` runs from
/// inside the VM's 384 MiB thread (W7-55 Important-3 follow-up).
pub fn resolve_call_tables(
    graph: &ModuleGraph,
) -> (
    KeywordTable,
    WitnessTable,
    CarrierTable,
    ProtoEqTable,
    TableConflicts,
) {
    crate::on_frontend_stack_scoped(move || {
        let mut c = Checker::new();
        c.harvest_keywords = true;
        c.run_graph_pass(graph, false);
        (
            std::mem::take(&mut c.keyword_calls),
            std::mem::take(&mut c.witnesses),
            std::mem::take(&mut c.carriers),
            std::mem::take(&mut c.proto_eq_calls),
            std::mem::take(&mut c.table_conflicts),
        )
    })
}

/// W7-49 — side-table keys asked to hold two different decisions, as `(span, message)`. Empty for
/// every well-formed program; a non-empty one is a hard compile error, never a warning.
pub type TableConflicts = Vec<(Span, String)>;

/// Resolve value-keyword calls for a SINGLE-FILE (standalone, source-string) program, through the
/// EXACT SAME [`resolve_call_tables`] pass as the multi-file CLI (one resolver, both engines
/// consume, module index `0`). Wraps `stmts` in a synthetic one-module graph, mirroring
/// [`resolve_extern_signatures_standalone`]. Test-only (the standalone compile/run paths are
/// `#[cfg(test)]`; production always goes through `build_graph`).
#[cfg(test)]
pub fn resolve_call_tables_standalone(
    stmts: &[Stmt],
) -> (
    KeywordTable,
    WitnessTable,
    CarrierTable,
    ProtoEqTable,
    TableConflicts,
) {
    let id = crate::resolver::ModuleId(std::path::PathBuf::from("<main>"));
    let graph = ModuleGraph {
        entry: id.clone(),
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
    };
    resolve_call_tables(&graph)
}

/// The [`KeywordTable`] key span for a value call that carries keyword arguments. The AST call-node
/// span is NOT unique across chained postfix calls: the parser gives every link of a `g(a=..)(b=..)`
/// chain the SAME primary-expression span (`parse_postfix`'s `let span = e.span;`), so keying the
/// table on it would alias two distinct keyword calls into one slot (the later insert wins and the
/// wrong permutation is applied — an out-of-range index or silent mis-routing). The FIRST named-arg
/// VALUE expression is, by contrast, a distinct source node per call, so its span uniquely identifies
/// the call WITHIN one lexed source (a module, or ONE interpolation fragment). Recording and BOTH
/// backend lookups run this helper, so they agree on the key. Only used when `named` is non-empty
/// (the sole record/lookup condition), so `first()` is always `Some`; the `call_span` fallback is
/// unreachable defensive code.
pub fn keyword_key_span(named: &[(String, Expr)], call_span: Span) -> Span {
    named.first().map(|(_, v)| v.span).unwrap_or(call_span)
}

/// Build the full [`KeywordKey`] for a value+keyword call: `(module, fragment-context span, fragment
/// ordinal, first-named-arg span)`. The checker's record site and BOTH backend lookup sites call this
/// one helper so they can never disagree on the key. `frag_ctx`/`frag_ord` are the interpolation
/// fragment discriminators (inert `Span::default()`/`0` outside interpolation); see [`KeywordKey`].
pub fn keyword_key(
    module_idx: usize,
    frag_ctx: Span,
    frag_ord: usize,
    named: &[(String, Expr)],
    call_span: Span,
) -> crate::checker::KeywordKey {
    (
        module_idx,
        frag_ctx,
        frag_ord,
        keyword_key_span(named, call_span),
    )
}

/// W7-49 — record ONE entry into a checker→compiler side table, refusing to overwrite a key that is
/// already bound to a DIFFERENT value.
///
/// This is a BACKSTOP under an already-injective key ([`Span::file`] is what makes the key injective
/// across modules), not a substitute for one. It exists for the surviving residual: the same default
/// expression spliced twice into the SAME module keeps one set of spans, so both splices share one
/// key — and a default is cloned into the CALLER's scope, so a caller-side local can shadow the
/// definer's global and the two splices can genuinely resolve differently. Overwriting there applies
/// the second site's decision to the first: a silent wrong value under a green `chezzi check`,
/// identical on both engines. Better loud than silent.
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
                 the backend cannot tell the two call sites apart. The known cause is a default \
                 parameter spliced into two call sites of one module: a default is cloned into each \
                 CALLER's scope, so a caller-side local that shadows the definer's global makes the \
                 two splices resolve differently — renaming that local is the workaround. If no \
                 default parameter is involved, this is a compiler bug; please report it with the \
                 source (docs/gaps.md W7-49)"
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
/// [`WitnessKey`]. `frag_ctx`/`frag_ord` are the same interpolation discriminators [`keyword_key`]
/// uses.
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
/// `frag_ctx`/`frag_ord` are the same interpolation discriminators [`keyword_key`] uses.
pub fn carrier_key(
    module_idx: usize,
    frag_ctx: Span,
    frag_ord: usize,
    name_span: Span,
) -> crate::checker::CarrierKey {
    (module_idx, frag_ctx, frag_ord, name_span)
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
        _ => call_span,
    }
}

impl Checker {
    /// The shared deps-first module-checking pass behind both [`check_graph`] and
    /// [`resolve_extern_signatures`]. When `harvest_externs` is set, gathers every struct's AST field
    /// types up front and stamps each module's graph index so the extern loop records resolved C
    /// signatures into `self.extern_sigs`.
    fn run_graph_pass(&mut self, graph: &ModuleGraph, harvest_externs: bool) {
        let c = self;
        // ROOT REDESIGN — module-scoped IDENTITY KEYS: scan every non-native module's struct/enum/alias
        // names and key EACH one `<module-key>::Name` (via the shared `resolver::module_keys`, the SAME
        // derivation the compiler uses), so the checker, compiler, and VM agree on every key (parity) and
        // every user type is unique by construction (a cross-module name clash is just two distinct keys).
        {
            let mkeys = crate::resolver::module_keys(graph);
            for (idx, lm) in graph.modules.iter().enumerate() {
                // ROOT REDESIGN — std modules' types are RESERVED/NATIVE: keep their BARE name (skip the
                // qualified key), so `Ref`/`Iterator`/FFI widths resolve bare, like the synthetic natives.
                if lm.native.is_some() || lm.is_std() {
                    continue;
                }
                for s in &lm.ast.stmts {
                    if let StmtKind::Struct { name, .. }
                    | StmtKind::Enum { name, .. }
                    | StmtKind::NewType { name, .. }
                    | StmtKind::TypeAlias { name, .. } = &s.kind
                    {
                        c.type_keys.insert(
                            (lm.id.clone(), name.clone()),
                            format!("{}::{name}", mkeys[idx]),
                        );
                    }
                }
            }
        }
        // FFI ROOT FIX (fix4): when harvesting extern signatures, gather every user struct's AST field
        // types up front, keyed by IDENTITY KEY (`<module-key>::Name`, the SAME derivation), so
        // `resolve_ctype` can build a by-value extern struct's `CType::Struct` with each field's real C
        // width — even when the struct is declared after the extern block, or in another module.
        if harvest_externs {
            let mkeys = crate::resolver::module_keys(graph);
            for (idx, lm) in graph.modules.iter().enumerate() {
                if lm.native.is_some() {
                    continue;
                }
                let is_std = lm.is_std();
                for s in &lm.ast.stmts {
                    if let StmtKind::Struct { name, fields, .. } = &s.kind {
                        let key = if is_std {
                            name.clone()
                        } else {
                            format!("{}::{name}", mkeys[idx])
                        };
                        c.struct_field_asts.insert(
                            key,
                            fields
                                .iter()
                                .map(|f| (f.name.clone(), f.ty.clone()))
                                .collect(),
                        );
                    }
                }
            }
        }
        // Build the reverse index name → declaring module label(s), in graph (deps-first) order, so a
        // bare unimported-type error can hint "import it from <module>". A non-entry module is labelled
        // by its dotted/file name; the entry module is excluded (its types are local, never imported).
        for lm in &graph.modules {
            if lm.native.is_some() || lm.id == graph.entry {
                continue;
            }
            let label = lm.label();
            for s in &lm.ast.stmts {
                if let StmtKind::Struct { name, .. }
                | StmtKind::Enum { name, .. }
                | StmtKind::NewType { name, .. }
                | StmtKind::TypeAlias { name, .. } = &s.kind
                {
                    c.types_by_name
                        .entry(name.clone())
                        .or_default()
                        .push(label.clone());
                }
            }
        }
        // The synthetic native-module structs are module-owned too: register their owning module so a
        // bare unimported `m: Match` errors with the "import it from std.regex" hint (the native-module
        // loop above skips them — they have no AST). Pushed after the AST loop so a same-named user
        // struct (declared in some module) is still hinted first.
        for (tn, owner) in [
            ("Match", "std.regex"),
            ("Response", "std.request"),
            ("ProcResult", "std.process"),
            ("FileInfo", "std.fs"),
        ] {
            c.types_by_name
                .entry(tn.to_string())
                .or_default()
                .push(owner.to_string());
        }
        for (idx, lm) in graph.modules.iter().enumerate() {
            // A native std module (std.math/io/os) has no AST: its public surface is a static table.
            if let Some(name) = lm.native {
                // A native std module is always stdlib. The file-backed harvest resolves its own decls'
                // types — including std.net's RESERVED `Socket`/`Listener` return types (`connect ->
                // Result[Socket]`, `accept -> Result[Socket]`), whose `resolve_type` reserved arm is
                // gated on `net_licensed` → `current_module_is_stdlib`. The native branch never runs
                // `begin_module` (which sets this for AST modules), so set it here or the harvest would
                // error `unknown type 'Socket'`. Additive-safe: every native module IS std, and the
                // other file-backed modules resolve their non-reserved types via the transient
                // `struct_names` arm regardless of this flag.
                c.current_module_is_stdlib = true;
                let mut sig = native_module_sig(name);
                // FILE-BACKED native modules (std.regex 4b; std.encoding/crypto/uuid/time 4e;
                // std.process/std.request 4f; std.math/io/os/rand/fs 4d) harvest their whole callable
                // SIGNATURE from the real `std/<M>.chz` AST the resolver loaded — NOT hand-built in
                // `native_module_sig`. For most the arm is fully deleted (returns empty). std.time keeps
                // a MINIMAL arm carrying only the `timer` opcode-license in `sig.types` (harvest then
                // fills its 4 real fns on top). Same predicate as the resolver's `visit_native_file`
                // gate — lockstep by construction.
                // W7-8 — a native `.chz` may `import` a sibling std module and NAME ITS TYPES in the
                // harvested SIGNATURES (`std.fs`'s `list_dir(p: PathLike) -> Result[List[path.Path]]`).
                // The harvest RESOLVES those types, so the imports must be bound BEFORE it — the
                // `has_bodied` bind below (which exists for bodied BODIES) runs far too late and the
                // signature errored `unknown module 'path'`. `begin_module` first, so the harvest sees
                // the same clean, stdlib-seeded env `check_module` gives an AST module rather than
                // whatever the previously-checked module left behind.
                // Gated on actually HAVING an import, so every pure-native module's live-table state is
                // byte-identical to before this change.
                if !lm.imports.is_empty() {
                    c.begin_module(Some(lm.label()));
                    c.current_module_is_stdlib = true;
                    c.push_scope(); // `bind_import` declares the bound name — it needs a live scope
                    for imp in &lm.imports {
                        c.bind_import(imp);
                    }
                }
                if crate::native::is_file_backed_native(name) {
                    c.harvest_native_module(&lm.ast, &mut sig);
                }
                // Phase 4c-net — cache std.net's harvested `Socket`/`Listener` method tables so
                // `seed_stdlib_structs` can re-seed them (bare, method-table only) into `self.structs`
                // for every subsequent module, letting `socket.read(...)`/`listener.accept(...)` resolve
                // via the normal method path (the retired bespoke `socket_method_sig` arm's replacement).
                if name == "std.net" {
                    c.net_socket_seed = sig.struct_defs.get("Socket").cloned();
                    c.net_listener_seed = sig.struct_defs.get("Listener").cloned();
                }
                // R2 — cache std.io's harvested `Writer` method table so `seed_stdlib_structs` re-seeds
                // it (bare, method-table only) for every subsequent module, letting `w.write(...)`/
                // `w.close(...)` resolve via the normal method path.
                if name == "std.io" {
                    c.io_writer_seed = sig.struct_defs.get("Writer").cloned();
                    // R2b — cache std.io's harvested `Reader` method table (parallel to `io_writer_seed`;
                    // a SEPARATE field so neither type's table clobbers the other).
                    c.io_reader_seed = sig.struct_defs.get("Reader").cloned();
                }
                // Re-attach the checker-side metadata that native decls can't express (hover docs,
                // module constants like math.pi/e, numeric-poly fns like math.abs, std.concurrency's
                // `RwShared.read`/`Executor.submit` closure-param sigs). Run for EVERY native module
                // (idempotent for those it doesn't cover) so the doc/const/poly attach no longer lives
                // in the deleted per-module arms.
                attach_native_module_metadata(name, &mut sig);
                // Phase 4c-concurrency — cache std.concurrency's harvested `Shared`/`RwShared`/`Atomic`/
                // `Executor` method tables so `seed_stdlib_structs` can re-seed them (bare, method-table
                // only) into `self.structs` for every subsequent module, letting `s.set(...)`/`a.cas(...)`
                // resolve via the normal method path (the retired bespoke `shared_method_sig`/etc arms'
                // replacement). Cached AFTER `attach_native_module_metadata` (unlike net's before-attach
                // cache) because that step mutates `RwShared.read`/`Executor.submit`'s closure-param sigs
                // — caching before would seed the pre-port (unannotated) tables.
                if name == "std.concurrency" {
                    for tn in ["Shared", "RwShared", "Atomic", "AtomicInt", "Executor"] {
                        if let Some(info) = sig.struct_defs.get(tn) {
                            c.concurrency_seeds.insert(tn.to_string(), info.clone());
                        }
                    }
                }
                // A HYBRID native module carries BODIED Chezzi decls alongside its bodyless native
                // ones: module-level `fn`s (PASS 2b harvested them into `sig.functions`) and
                // native-struct `bodied_methods` (`Reader.lines`, in the struct method table). The
                // native arm skips `check_module`, so those bodies would go UNCHECKED — a `str`
                // returned under an `int` sig would slip straight through. Type-check them here via the
                // same `check_fn_body` a normal module uses, on a clean `begin_module` env. Gated on
                // actually having a bodied decl so a pure-native module (os/fs/regex/…) pays nothing
                // and its live-table state is left exactly as the harvest above produced it.
                let has_bodied = lm.ast.stmts.iter().any(|s| {
                    matches!(&s.kind, StmtKind::Fn(_))
                        || matches!(&s.kind, StmtKind::NativeStruct { bodied_methods, .. } if !bodied_methods.is_empty())
                });
                if has_bodied {
                    c.begin_module(Some(lm.label()));
                    c.current_module_is_stdlib = true;
                    c.push_scope();
                    // Bind this native module's own imports so a bodied fn body can use them (a native
                    // `.chz` may `import` like any other module; deps are checked earlier in graph order,
                    // so their sigs are already in `module_sigs`).
                    for imp in &lm.imports {
                        c.bind_import(imp);
                    }
                    // `begin_module` cleared the live tables; repopulate the callables/types the
                    // harvested `sig` holds so a bodied body can call a sibling fn or name a sibling
                    // native struct (`-> Reader`).
                    for (n, f) in &sig.functions {
                        c.functions.insert(n.clone(), f.clone());
                    }
                    for (n, info) in &sig.struct_defs {
                        c.structs.insert(n.clone(), info.clone());
                        c.bare_types.insert(n.clone(), n.clone());
                        c.struct_names.insert(n.clone());
                    }
                    // Module-level bodied fns (`divmod`): free-fn sig, no `self`, decl↔sig params
                    // align by index exactly as `check_module`'s top-level-fn path.
                    for s in &lm.ast.stmts {
                        if let StmtKind::Fn(decl) = &s.kind
                            && let Some(fsig) = sig.functions.get(&decl.name).cloned()
                        {
                            c.check_fn_body(decl, None, fsig);
                        }
                    }
                    // Native-struct bodied methods (`Reader.lines`): use a FRESH `fn_sig` (which keeps
                    // the leading `self`), NOT the leading-`self`-STRIPPED method-table sig — else
                    // `check_fn_body`'s positional decl↔sig param map (self at decl index 0) shifts
                    // every real param to `Unknown`.
                    for s in &lm.ast.stmts {
                        if let StmtKind::NativeStruct {
                            name,
                            type_params,
                            bodied_methods,
                            ..
                        } = &s.kind
                            && !bodied_methods.is_empty()
                        {
                            let saved = c.enter_type_params(type_params);
                            // A native struct's `self` is usually a RESERVED opaque handle
                            // (`Reader`→`Ty::Reader`, `Socket`→`Ty::Socket`, …) whose method dispatch
                            // goes through the reserved-`Ty` arm (which reads the leading-`self`-stripped
                            // method table correctly). Use that Ty for `self` so `self.read_line()`
                            // resolves — `Ty::Struct("Reader")` would route to the generic struct arm and
                            // wrongly demand a receiver slot. Fall back to the nominal struct type for a
                            // plain (non-reserved) native struct.
                            let args: Vec<Ty> = type_params
                                .iter()
                                .map(|tp| Ty::Param(tp.name.clone()))
                                .collect();
                            let self_ty = c
                                .qualified_builtin_ty(name, &args)
                                .unwrap_or_else(|| c.struct_self_ty(name));
                            for m in bodied_methods {
                                let msig = c.fn_sig(m, m.name_span);
                                c.check_fn_body(m, Some(self_ty.clone()), msig);
                            }
                            c.exit_type_params(saved);
                        }
                    }
                }
                // The native arm is the ONE module path that never ends on a `begin_module`, so drop
                // this module's import bindings explicitly: a leaked `imported_modules`/`structs` entry
                // would make an UNIMPORTED type resolve by accident in the NEXT native module harvested
                // (an AST module clears them itself). Same `!imports.is_empty()` gate, so a pure-native
                // module's post-state is untouched.
                if !lm.imports.is_empty() {
                    c.begin_module(None);
                }
                c.module_sigs.insert(lm.id.clone(), sig);
                continue;
            }
            let label = if lm.id == graph.entry {
                None
            } else {
                Some(lm.label())
            };
            c.begin_module(label);
            // Stamp the current module's graph index so the extern loop keys its resolved C signatures
            // the SAME way both backends look them up. `None` outside the harvesting pass.
            c.extern_module_idx = if harvest_externs { Some(idx) } else { None };
            // Maintained on every graph pass (cheap) so a recorded value+keyword call is keyed under
            // the SAME module index the backends derive — mirrors the extern-sig keying.
            c.graph_module_idx = idx;
            c.current_module_is_stdlib = lm.is_std();
            let sig = c.check_module(&lm.ast.stmts, Some(&lm.id), &lm.imports);
            // Phase 5a-containers — capture the always-linked prelude's `List`/`Map`/`Set`/`Channel`
            // native-struct METHOD tables so `seed_stdlib_structs` can re-seed them (bare, method-table
            // only) into `self.structs` for every subsequent module, letting `xs.push(...)`/`m.get(...)`/
            // `s.add(...)`/`ch.send(...)` resolve via the normal method path (the retired bespoke
            // `list_method_sig`/`map_method_sig`/`set_method_sig`/`channel_method_sig` arms' replacement).
            // Harvested here (NOT from `sig.struct_defs` — the prelude
            // is a normal AST module whose `check_module` no-ops native structs) by resolving the decls
            // over `lm.ast` while still in the prelude's stdlib module context (type params + `Hashable`
            // in scope). The prelude is order[0] (always-linked, no deps), so this is populated before
            // entry/all others. The `List`/`Map`/`Set` names still resolve
            // to the RESERVED `Ty::List`/`Ty::Map`/`Ty::Set` via `resolve_type`'s reserved arms; the
            // ctors/literals stay compiler-wired.
            if c.container_seeds.is_empty() && lm.dotted == ["std", "prelude"] {
                for tn in ["List", "Map", "Set", "Channel", "str", "bytes", "bytearray"] {
                    if let Some(info) = c.harvest_native_struct_table(&lm.ast, tn) {
                        c.container_seeds.insert(tn.to_string(), info);
                    }
                }
            }
            // Phase 5b-native-enum — DRIFT GUARD (assert-only, resolution-inert). Option/Result's variant
            // SHAPE is now ALSO declared in `std/prelude.chz` as `native enum Option[T]`/`Result[T, E]`,
            // but their identity, `?` propagation, match exhaustiveness, and `Ok`/`Err`/`Some`/`None`
            // construction stay 100% Rust-inline (`variants_of`/`match_kind`/`resolve_type`, untouched).
            // The `.chz` decl is a checked source-of-truth MIRROR: assert the parsed+resolved variant set
            // byte-equals the inline `variants_of` maps so the two can't drift. Runs on the always-linked
            // prelude module; keeps `harvest_native_enum_table` production-live (no dead_code) AND is
            // assert-only (no effect on resolution/output), so behavior + 3-engine parity are unchanged.
            if lm.dotted == ["std", "prelude"] {
                c.assert_native_enum_shape_matches(&lm.ast);
                // Phase 5c-protocols — DRIFT GUARD (assert-only, resolution-inert). All 18 reserved
                // protocols (`Any`, Comparable/Stringable/Error/Hashable, the operator protocols, the
                // `Arithmetic` bundle, `Iterator`, `Iterable`, `Index`/`IndexSet`/`Slice`, `Convert`) are now ALSO declared in
                // `std/prelude.chz` as plain `protocol` decls, but `prebuilt_protocols` stays the live
                // runtime source (conformance/operator-lowering/`check_bounds` untouched). Assert the
                // parsed+resolved shape byte-equals the Rust seed so the two can't drift. Keeps
                // `harvest_protocol_shape` production-live (no dead_code) AND is assert-only (no effect on
                // resolution/output), so behavior + 3-engine parity are unchanged.
                c.assert_native_protocol_shape_matches(&lm.ast);
            }
            // M24 — the manifest's `[project] entrypoint = "mod:fn"` names a function the runtime
            // invokes BY NAME at a fixed arity of ZERO, exactly like a `test fn`. A hidden witness
            // parameter has no call site to come from there, so a bare `chezzi run` would fault at
            // startup after a green check with the hidden arity leaked into the message
            // ("expects 1 argument(s), got 0"). Rejected here, where the name is known.
            if lm.id == graph.entry
                && let Some(f) = c.entry_fn.clone()
            {
                let decl = lm.ast.stmts.iter().find_map(|s| match &s.kind {
                    StmtKind::Fn(d) if d.name == f => Some((d, s.span)),
                    _ => None,
                });
                let span = decl.map_or(Span::RUNTIME, |(_, sp)| sp);
                // Two ways the zero-argument call cannot be satisfied, one message. The DECLARED
                // params are the plain one — `fn main(a: int)` used to check green and then die at
                // startup with "function 'main' expects 1 argument(s), got 0", which reads as a
                // caller's mistake when the caller is the runtime. The hidden witness params are the
                // M24 one: no call site exists to pin `T`.
                let declared = decl.map_or(0, |(d, _)| d.params.len());
                let w = c.stored_witness_params(None, &f);
                let cause = if declared > 0 {
                    Some(format!(
                        "it declares {declared} parameter(s), which nothing can supply. Give the entrypoint a nullary signature and read its inputs inside it (e.g. `std.os.args`)"
                    ))
                } else if !w.is_empty() {
                    Some(format!(
                        "it cannot construct through its static-protocol bound ({}) — the hidden type witness has no call site to come from. Give the entrypoint a non-generic signature and move the construction into a helper it calls with a concrete type",
                        w.join(", ")
                    ))
                } else {
                    None
                };
                if let Some(cause) = cause {
                    c.error(
                        span,
                        format!(
                            "the manifest entrypoint '{f}' is invoked with no arguments, so {cause}"
                        ),
                    );
                }
            }
            c.module_sigs.insert(lm.id.clone(), sig);
        }
    }
}

/// The static type signatures of a native std module's members (M6c). This is the **third**
/// lockstep table: it must agree with the runtime members in `src/native/<module>.rs` and the
/// per-engine value lowering. `std.math` params are `float` (the language has no implicit int→float,
/// so callers pass floats); `pi`/`e` are float constants.
fn native_module_sig(name: &str) -> ModuleSig {
    // Only the residual opcode/type-license modules still have a hand-built arm (concurrency's ctor
    // type names, time's `timer`, ffi's C-ABI type-license tail) — every one inserts ONLY `sig.types`,
    // so there is no `func()`/`sig.functions` helper here anymore (all callable fns are file-backed and
    // harvested from `std/<M>.chz`). See `is_file_backed_native` + `harvest_native_module`.
    let mut sig = ModuleSig::default();
    match name {
        // std.math / std.io / std.os / std.rand (phase 4d) and std.process (phase 4f) are FILE-BACKED:
        // their whole signatures are declared in `std/<M>.chz` and harvested via `harvest_native_module`
        // — NO hand-built arm here (this fn returns the default-empty sig for them). The checker-side
        // metadata native decls can't express (math's `pi`/`e` values + `abs`'s numeric polymorphism +
        // hover docs) is re-attached post-harvest by `attach_native_module_metadata`.
        // std.net (phase 4c-net) is FILE-BACKED: its `connect`/`listen` free fns AND its `Socket`/
        // `Listener` native structs (WITH harvested method tables — the native-method-binding
        // capability) are declared in `std/net.chz` and harvested via `harvest_native_module` — NO
        // hand-built arm here. The `Socket`/`Listener` TYPE names still resolve to the RESERVED
        // `Ty::Socket`/`Ty::Listener` (opaque VM handles, import-gated via `imported_net`); the
        // `connect`/`listen`/`read`/`write`/`accept` calls stay VM-intercepted at runtime by name.
        // std.fs (phase 4d) and std.encoding / std.crypto / std.uuid (phase 4e) are FILE-BACKED: their
        // whole signatures are declared in `std/<M>.chz` (bodyless `native fn`s) and harvested via
        // `harvest_native_module` — NO hand-built arm here (this fn returns the default-empty sig for
        // them). See the caller's `is_file_backed_native` gate.
        // std.concurrency (phase 4c-concurrency) is FILE-BACKED: its four GENERIC native structs
        // (`Shared[T]`/`RwShared[T]`/`Atomic[T]`/`Executor`, WITH harvested method tables — the
        // native-method-binding capability extended to generics) are declared in `std/concurrency.chz`
        // and harvested via `harvest_native_module` — NO hand-built arm here (this fn returns the
        // default-empty sig for it). This was the LAST virtual native module: its arm is DELETED
        // ENTIRELY (unlike std.net which needed no residual either — the four TYPE names come from the
        // file, not a type-license tail). The `Shared`/`RwShared`/`Atomic`/`Executor` names still
        // resolve to the RESERVED `Ty::Shared`/`Ty::RwShared`/`Ty::Atomic`/`Ty::Executor` (opaque VM
        // handles, import-gated via `imported_concurrency`); the ctors stay lowered to
        // `Op::NewShared`/etc by name, and the methods stay VM-intercepted at runtime.
        "std.time" => {
            // std.time is FILE-BACKED (phase 4e): its 4 callable fns (now/monotonic/sleep_ms/format) are
            // declared in `std/time.chz` and harvested via `harvest_native_module` on top of this sig.
            // `timer` is ALSO declared there (as `native fn timer`) — but it's an opcode-backed builtin
            // (NOT a callable native member): it carries NO runtime value and lowers via the compiler's
            // name→opcode dispatch. Harvest routes its sig to the `time_timer_sig` field (NOT
            // `sig.functions`, which the From-import arm would bind as a real callable). This arm keeps
            // `timer` in `sig.types` ONLY so `import timer from std.time` validates membership and
            // `bind_import` records it into the per-module `imported_time` set; `infer_named_call` then
            // accepts the bare `timer(ms)` call only in a module that imported it. (Its `sig.functions`
            // entry stays absent by design — see `harvest_native_module` PASS 2.)
            sig.types.insert("timer".to_string());
        }
        // std.regex is FILE-BACKED (phase 4b): its whole signature (the `native struct Match` + the 5
        // `native fn`s) is declared in `std/regex.chz` and harvested via `harvest_native_module` — there
        // is NO hand-built arm here (this fn returns the default-empty sig for it). See the caller.
        // std.request is FILE-BACKED (phase 4f): its whole signature (the `native struct Response` +
        // get/post/request — with an OPTIONAL trailing `timeout_ms` — plus put/patch/delete/head) is
        // declared in `std/request.chz` and harvested via `harvest_native_module`. NO hand-built arm.
        "std.ffi" => {
            // std.ffi is FILE-BACKED (phase 4c): its 59 callable fns (`null`/`is_null`, the load_*/
            // store_* families in base + `_at` forms, and `alloc`/`alloc_zeroed`/`free`) are declared
            // in `std/ffi.chz` (bodyless `native fn`s) and harvested via `harvest_native_module` on top
            // of this sig. This arm is retained ONLY for the type-license tail — the C-ABI type NAMES
            // std.ffi exports, which carry NO runtime value and CANNOT be spelled as a `native fn` decl
            // (there is no .chz syntax for a bare type-license name aliasing Ty::Int/Ty::Ptr). Mirrors
            // the residual std.net/std.concurrency/std.time arms.
            //
            // The eight fixed-width C-ABI integer TYPE names (Chezzi's first type imports). They live in
            // `sig.types` so `import int32 from std.ffi` validates; the checker's `bind_import` records
            // the import into `imported_ffi_types` and `resolve_type` then resolves the name to `Ty::Int`
            // only in modules that imported it.
            for tn in crate::native::ffi::TYPE_NAMES {
                sig.types.insert((*tn).to_string());
            }
            // The opaque `ptr` handle type is ALSO exported by `std.ffi` (kept out of `TYPE_NAMES`,
            // which routes a name through the ungated C-marshalling path `resolve_ctype_d`). Listing
            // it in `sig.types` lets `import ptr from std.ffi` validate; the checker licenses it into
            // `imported_ffi_types` (whole-module on `import std.ffi`, per-name on the from-import).
            // NOTE: harvesting `native fn null() -> ptr` from std/ffi.chz needs `ptr` to resolve, but
            // harvest runs WITHOUT `begin_module` (so `imported_ffi_types` is empty) — `harvest_native_module`
            // transiently licenses the `ptr`/TYPE_NAMES names it finds in `sig.types` for the harvest,
            // then restores, so this insert is load-bearing for the harvest too.
            sig.types.insert("ptr".to_string());
        }
        _ => {}
    }
    sig
}

/// Re-attach the checker-side metadata a `native fn` decl CANNOT express, on top of a native module's
/// `ModuleSig` (whether hand-built by `native_module_sig` or harvested from a file-backed `std/M.chz`).
/// Runs in the graph loop for EVERY native module (idempotent for those it doesn't cover). Three pieces:
///   (a) editor hover docs (`MODULE_FN_DOCS`) — a concise one-line blurb on each authored fn's `FnSig.doc`
///       (excluded from `fn_sig_eq`, so purely informational; `record_method_hover` forwards it at the
///       `module.fn` hover site). Drift-guarded by `module_fn_docs_all_resolve`.
///   (b) module CONSTANT values (`math.pi`/`e`) — read from `native::native_consts` (the runtime table,
///       reused so there is no hardcoded pi/e here) and exposed as `float` module values.
///   (c) numeric-POLYMORPHIC fns (`math.abs`: int→int / float→float) — the `FnSig` fixes only arity;
///       `infer_numeric_poly` does the real typing. Listed in the `MODULE_NUMERIC_POLY` side-table.
/// Moved out of the (now-deleted) per-module `native_module_sig` arms so the file-backed migration
/// (phase 4d) keeps hover/const/poly byte-identical. The synthetic struct LAYOUTS (Match/Response/
/// ProcResult) + request's optional-tail fns are NOT here either — they are file-backed (4b/4f) and
/// harvested from the parsed `.chz` by `harvest_native_module`.
fn attach_native_module_metadata(name: &str, sig: &mut ModuleSig) {
    if let Some((_, docs)) = MODULE_FN_DOCS.iter().find(|(m, _)| *m == name) {
        for (fname, doc) in *docs {
            if let Some(f) = sig.functions.get_mut(*fname) {
                f.doc = Some((*doc).to_string());
            }
        }
    }
    for (cname, _) in crate::native::native_consts(name) {
        sig.values.insert((*cname).to_string(), Ty::Float);
        // A native module constant (`math.pi`/`e`/`inf`/`nan`) is immutable — mark it const so a
        // rebind (`m.pi = x` or `import pi from m; pi = x`) reports it as const, not a mutable field.
        sig.const_values.insert((*cname).to_string());
    }
    if let Some((_, polys)) = MODULE_NUMERIC_POLY.iter().find(|(m, _)| *m == name) {
        for p in *polys {
            sig.numeric_poly.insert((*p).to_string());
        }
    }
    // Phase 4c-concurrency — two method sigs a plain harvested sig CANNOT express, re-attached here.
    // Both are closure params whose expressible constraint is arity + (for `read`) the box's element
    // type, but whose RETURN must be `?` (any R) — a shape `std/concurrency.chz` declares as an
    // UNANNOTATED param (`Ty::Unknown`) and this step retypes.
    if name == "std.concurrency" {
        // `RwShared[T].read(f)` — `read(fn(T) -> R) -> R` is R-polymorphic: the param is retyped to
        // `fn(T) -> ?` (any closure return accepted; the real R is recovered at the call site by the
        // `Ty::RwShared` dispatch arm). `T` is RwShared's own type param (kept as `Ty::Param` in the
        // seeded table; the dispatch arm substitutes the concrete element type).
        if let Some(rw) = sig.struct_defs.get_mut("RwShared") {
            let t = rw
                .type_params
                .first()
                .map(|tp| Ty::Param(tp.name.clone()))
                .unwrap_or(Ty::Unknown);
            if let Some(read) = rw.methods.get_mut("read")
                && let Some(p0) = read.params.first_mut()
            {
                *p0 = Ty::Func {
                    params: vec![t],
                    ret: Box::new(Ty::Unknown),
                    labels: FnLabels::default(),
                };
            }
        }
        // `Executor.submit(f)` — a detached zero-arg task closure whose return is discarded: the param
        // is retyped to `fn() -> ?` so any-return closures are accepted while a wrong-ARITY closure is
        // still rejected (Executor is non-generic — no `Ty::Param` involved).
        if let Some(ex) = sig.struct_defs.get_mut("Executor")
            && let Some(submit) = ex.methods.get_mut("submit")
            && let Some(p0) = submit.params.first_mut()
        {
            *p0 = Ty::Func {
                params: vec![],
                ret: Box::new(Ty::Unknown),
                labels: FnLabels::default(),
            };
        }
    }
}

/// Numeric-polymorphic native module fns (int args → int, float args → float), keyed by module. The
/// `FnSig` in the module's `.chz` fixes only arity (`abs(x: float) -> float`); `infer_numeric_poly`
/// does the real per-call typing when the fn is in its module's `numeric_poly` set. Parallels
/// `MODULE_FN_DOCS`; applied by `attach_native_module_metadata`. (Was inline in the deleted math arm.)
const MODULE_NUMERIC_POLY: &[(&str, &[&str])] = &[("std.math", &["abs", "sign"])];

/// Editor hover (Tier C): concise one-line `(module, [(fn, doc)])` blurbs for native stdlib module
/// functions, paraphrased from `docs/stdlib.md §4`. Applied to `FnSig.doc` in `native_module_sig`
/// and surfaced verbatim at the `module.fn` hover site. Drift-guarded by `module_fn_docs_all_resolve`
/// (every listed fn must exist). Coverage is `std.math` / `std.io` / `std.os` for v1; other native
/// modules are a follow-up (noted in PROGRESS.md) — their fns simply hover doc-less, as before.
#[allow(clippy::type_complexity)]
const MODULE_FN_DOCS: &[(&str, &[(&str, &str)])] = &[
    (
        "std.math",
        &[
            (
                "abs",
                "absolute value (numeric-polymorphic: int→int, float→float)",
            ),
            ("floor", "round down to the nearest integer (toward -inf)"),
            ("ceil", "round up to the nearest integer (toward +inf)"),
            (
                "round",
                "round to the nearest integer (half away from zero)",
            ),
            ("pow", "pow(base, exp): base raised to exp"),
            ("sqrt", "square root (NaN for a negative argument)"),
            ("sin", "sine of an angle in radians"),
            ("cos", "cosine of an angle in radians"),
            ("tan", "tangent of an angle in radians"),
            ("asin", "arcsine in radians (NaN outside [-1, 1])"),
            ("acos", "arccosine in radians (NaN outside [-1, 1])"),
            ("atan", "arctangent in radians"),
            (
                "atan2",
                "atan2(y, x): angle of the vector (x, y) in radians",
            ),
            ("exp", "e raised to the power x"),
            ("ln", "natural (base-e) logarithm (-inf at 0, NaN below)"),
            ("log2", "base-2 logarithm"),
            ("log10", "base-10 logarithm"),
            (
                "log",
                "log(value, base): logarithm of value in the given base",
            ),
            ("is_nan", "true if x is NaN"),
            ("is_inf", "true if x is +inf or -inf"),
            ("is_finite", "true if x is neither NaN nor infinite"),
        ],
    ),
    (
        "std.io",
        &[
            ("print", "write a line to stdout (with a trailing newline)"),
            ("eprint", "write a line to stderr (with a trailing newline)"),
            (
                "read_line",
                "read one line from stdin, newline stripped (None at EOF)",
            ),
            (
                "flush",
                "flush this process's stdout (a no-op: streamed output is unbuffered)",
            ),
            (
                "input",
                "input(prompt): print the prompt (no newline), flush, read one line (None at EOF)",
            ),
            ("read_file", "read a whole file as text (Result)"),
            (
                "write_file",
                "write/overwrite a file with the given text (Result)",
            ),
        ],
    ),
    (
        "std.os",
        &[
            (
                "args",
                "program arguments (positionals after the script path)",
            ),
            ("env", "look up an environment variable (None if unset)"),
            ("getcwd", "the current working directory (Result)"),
            (
                "exit",
                "halt the program with an exit code (does NOT run defers)",
            ),
        ],
    ),
];

/// B3.3 (Task 2a) — one non-sendable LOCAL capture of a closure/nested-fn value: the captured
/// binding's name and its checker type. Recorded at the value's decl site and replayed as a compile
/// error at a spawn callee/arg site.
#[derive(Clone)]
struct Capture {
    name: String,
    ty: Ty,
}

struct Checker {
    errors: Vec<CheckError>,
    scopes: Vec<HashMap<String, Ty>>,
    /// Per-scope set of names bound as `for`-loop variables. Mirrors `scopes` index-for-index (a
    /// loop var is immutable — rebound fresh each iteration — so assigning to it is rejected; this
    /// sidesteps a VM/interp divergence where the VM's counter slot IS the loop var).
    loop_vars: Vec<std::collections::HashSet<String>>,
    /// Every name declared by a TOP-LEVEL `let`/`:=` in the module currently being checked. Used to
    /// distinguish a genuine first-class builtin (`f := print`, no such global) from a same-named
    /// module global read before its definition line (a use-before-def error, like any other global):
    /// `infer_ident` suppresses the first-class-builtin arm when the name is in this set so the read
    /// falls through to the same `unknown name` error, keeping the VM (pre-slotted `nil`) and the
    /// interp (source-order env) from diverging. Rebuilt at the start of each `check_module`.
    module_global_lets: std::collections::HashSet<String>,
    /// Per-scope set of names declared `const T` (mirrors `scopes` index-for-index). A const binding
    /// is immutable: `check_assign` rejects any later reassignment of the name. Compile-time-only
    /// (freezes the NAME; the object stays mutable — shallow). Cleared on re-declaration by `declare`
    /// (a shadowing `:=` yields a fresh, possibly-mutable binding), same rule as `loop_vars`.
    const_decls: Vec<std::collections::HashSet<String>>,
    functions: HashMap<String, FnSig>,
    /// Names of functions declared in the CURRENT module (top-level `fn`s only — NOT imported names).
    /// Gates the generic-fn-as-value turbofish B-path (`ident[int]`) so the checker only accepts a
    /// turbofish on a SAME-MODULE generic fn — the exact set the compiler's `fn_names` erases at
    /// codegen, keeping checker-accept ⟺ compiler-erase in lockstep (an imported generic-fn turbofish
    /// stays a clean "cannot index into fn(T) -> T" error, a documented v1 limit). Cleared per-module
    /// with `functions` (`begin_module`) and populated at the same-module fn-sig registration.
    local_fn_names: std::collections::HashSet<String>,
    /// Names this module has already RESOLVED THROUGH `functions` — i.e. code whose types were fixed
    /// against a top-level (or from-imported) fn's signature. Recorded at the two sites that really
    /// type user code against a `FnSig`: the bare-value read (`infer_ident`) and the by-name call
    /// dispatch (`infer_named_call`); NOT at the display/hover/existence lookups, which decide
    /// nothing. It is what makes `reject_redeclare`'s fn arm fire only when a re-declaration can
    /// actually break something: `f := fn(a: int) -> int: …` after `fn f(a: int, b: int = 2)` is
    /// SOUND while nothing above it read `f` (CPython agrees, measured), and unsound the moment a
    /// reader sits above the let. Keyed on the READERS, never on where the `fn` was declared — the
    /// fn's slot is filled before any statement runs, so its position says nothing. Per-module:
    /// cleared in `begin_module` alongside `functions`.
    fn_reads: std::collections::HashSet<String>,
    structs: HashMap<String, StructInfo>,
    /// Structural protocols by name. Program-global (like structs). Pre-seeded with `Comparable`.
    protocols: HashMap<String, ProtocolInfo>,
    /// Generic type parameters currently in scope (name → its protocol bounds), set while
    /// building/checking a generic fn's signature and body. Save/restore to nest. Bounds carry their
    /// type args (`Iterator[T]`) so element-type recovery can read them in scope.
    type_params: HashMap<String, Vec<Bound>>,
    /// enum name → its variant names, in declaration order (for exhaustiveness).
    enums: HashMap<String, Vec<String>>,
    /// enum name → its generic type parameters (empty for a non-generic enum). Used to build the
    /// substitution from `Tree[int]`'s args onto each variant's payload (which may name `T`).
    enum_type_params: HashMap<String, Vec<TypeParam>>,
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
    /// newtype name (BARE, current-module visibility) — mirrors `enum_names`/`struct_names`.
    newtype_names: std::collections::HashSet<String>,
    /// newtype runtime key (`bare_key`) → its (underlying `Ty`, name-keyed methods). Mirrors
    /// `enum_methods` + the layout tables: drives construct, cast-unwrap, same-type operators,
    /// method dispatch, and protocol satisfaction. NOT cleared per-module (graph-wide, like `enums`).
    newtype_defs: HashMap<String, (Ty, HashMap<String, FnSig>)>,
    /// newtype runtime key → its generic type parameters (empty for a scalar newtype). Mirrors
    /// `enum_type_params`: builds the substitution from a `Ty::NewType(key, args)`'s args onto the
    /// underlying type + method signatures (which may name the params). A non-empty entry marks the
    /// newtype as generic — gating off the scalar-newtype native operator auto-flow (methods-only).
    newtype_type_params: HashMap<String, Vec<TypeParam>>,
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
    /// `Some(T)` while resolving a struct/enum/newtype method's SIGNATURE or BODY — the concrete
    /// enclosing type `Self` names (e.g. `fn dup(self) -> Self` inside `struct P`). `None` at top
    /// level, inside a free fn, or a nested fn/closure (reset like `current_ret`), so `Self` outside
    /// a method stays `unknown type 'Self'`. A PROTOCOL method keeps `None` here: its `Self` is
    /// already in `type_params` as `Ty::Param("Self")` (resolved by the earlier type-param arm), so
    /// this concrete binding never fires for it. Saved/restored via `mem::replace` at the method-sig
    /// hoist sites and at `infer_fn_ret`/`check_fn_body` entry (from their `self_ty` argument).
    current_self_ty: Option<Ty>,
    /// `Some(T)` while checking a generator function body whose declared return is `Iterator[T]` —
    /// the element type each `yield` must produce. `None` outside a generator, so a stray `yield`
    /// is diagnosed. Saved/restored across nested fn/closure boundaries like `current_ret`.
    yield_ty: Option<Ty>,
    /// `> 0` while checking statements lexically inside a `recover:` block (within the current
    /// function — reset across nested fn/closure boundaries). A `?` here targets the recover
    /// boundary (yielding to `r`), not the enclosing function's return.
    recover_depth: u32,
    /// True while pass-1 is inferring a function's return type: `check_return` records each
    /// return's type into `collected_rets` instead of diagnosing against `current_ret`.
    inferring_ret: bool,
    /// Return types gathered from the body during return-type inference (see `infer_fn_ret`).
    collected_rets: Vec<Ty>,
    /// True while checking (or inferring the return of) a generator function body — the sole signal
    /// that a `yield` is in-bounds. Distinct from `yield_ty`: during return inference (`infer_fn_ret`)
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
    /// True while checking the body of a **default-argument provider** — the hidden zero-arg fn
    /// `desugar` synthesizes for a non-inline parameter/field default (`desugar::PROVIDER_PREFIX`).
    /// Its only reader is [`Checker::infer_try`]: a `?` there has no caller to propagate to, and the
    /// generic "returns int, not Result" wording would name a return type the user never wrote.
    /// Saved/restored 1:1 beside `current_ret` at every fn/closure boundary.
    in_default_provider: bool,
    /// True while checking statements lexically inside a `defer:` BLOCK (reset across nested
    /// fn/closure boundaries, like `recover_depth`). A `?` here is DISCARDED at the block boundary
    /// (`syntax.md`: "a `?` short-circuit inside the block is discarded — a cleanup body has no
    /// error-return contract"), so it must NOT be validated against the enclosing function's return
    /// type. See `infer_try`. Entering a defer block also zeroes `recover_depth` (the block is its
    /// own closure — a `?` in it cannot target an outer `recover:` boundary).
    in_defer_block: bool,
    /// True while checking statements lexically inside a `spawn:` BLOCK. Set at the task boundary
    /// (`SpawnTarget::Block`) and cleared across every nested fn/closure boundary, where it is
    /// saved/restored 1:1 beside `current_ret` — a `fn`/closure DECLARED inside a task has a caller
    /// of its own, so a `?` in its body is legal.
    ///
    /// A spawned task is its own frame with no caller, so a `?` directly in it has nowhere to
    /// propagate to — the nursery discards a task's returned `Err` by design (W7-46, Go's
    /// contract). This flag exists so that rejection can say so. The alternative — zeroing
    /// `current_ret` at the task boundary — reports "'?' used in a function that returns nil",
    /// which reads as a claim about the ENCLOSING fn the user actually wrote, and additionally
    /// makes `check_return` emit a second, false "function returns nothing" for a `return` in the
    /// task. See `infer_try` (W7-48).
    in_spawn_block: bool,
    /// Element types gathered from every `yield` during a generator's return-type inference
    /// (`infer_fn_ret`, `inferring_ret` mode). The FIRST pins the generator's element `T`
    /// (strict-first-yield); pass-2 `check_yield` validates the rest against it. Drained per-fn.
    collected_yields: Vec<Ty>,
    /// Public surfaces of already-checked modules (multi-file programs), keyed by module id.
    module_sigs: HashMap<ModuleId, ModuleSig>,
    /// Names bound to an imported module in the *current* module → which module they refer to.
    imported_modules: HashMap<String, ModuleId>,
    /// First segment of each imported DOTTED module path (`std` from `import std.concurrency`) →
    /// `(dotted_path, bound_name)` of the FIRST import that introduced it. Used ONLY to give a
    /// targeted two-level-path hint for a multi-level mistake like `std.concurrency.Shared(0)` (the
    /// head `std` is a path PREFIX, not a bound name) instead of the misleading "unknown name 'std'".
    /// Never a bound value/type itself, so it cannot mask a genuine typo.
    import_path_heads: HashMap<String, (String, String)>,
    /// The first TWO segments of each imported module's dotted path → its bound name (last segment /
    /// alias), or `None` when two imports share those two segments (ambiguous). Keyed on two segments
    /// because a too-deep-path mistake fires from `infer_field` with only the head + the NEXT segment
    /// visible (the call/field already consumed the rest): `(std, net)` → `net`, and
    /// `(std, concurrency)` → `collection` for `import std.concurrency.collection`. Correct for 2- and
    /// 3+-level imports and sibling collisions alike — unlike `import_path_heads` (head-only,
    /// first-wins), which named the wrong sibling / the wrong segment.
    module_prefix2: HashMap<(String, String), Option<(String, String)>>,
    /// Every name an `import`/`import … from` binds in the *current* module → the span of its first
    /// import, across ALL import namespaces (values, functions, modules, type-names). Used to reject
    /// a SECOND import binding the same name (`import f from lib` + `import f from lib2`, or
    /// value-then-fn `import v` + `import v`), which the separate namespace tables otherwise let pass
    /// last-wins (and which makes the checker and runtime disagree on the binding). Per-module:
    /// cleared in `begin_module`.
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
    /// Swift-style keyword-argument resolution for VALUE calls: `perm[i]` = index into the combined
    /// `[positional args ++ named exprs]` list that fills parameter slot `i`. Keyed `(module idx,
    /// call span)` — module-scoped exactly like [`Self::extern_sigs`]. Recorded during `infer_call`
    /// (only when [`Self::harvest_keywords`] is set — the error-gate `check_graph` leaves it empty) and
    /// consumed by both backends to lower a value+keyword call to a positional `Op::Call`. Produced by
    /// [`resolve_call_tables`].
    keyword_calls: KeywordTable,
    /// True only while [`resolve_call_tables`] drives the pass, licensing `infer_call` to record
    /// into [`Self::keyword_calls`] / [`Self::witnesses`]. Off during the normal error-gate check
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
    /// W7-53 I1′ — which dispatch each `.eq(x)` call site takes, keyed by [`carrier_key`] and
    /// consumed verbatim by the compiler (which cannot re-derive it: the decision is the RECEIVER's
    /// type). Recorded UNCONDITIONALLY, for the same reason [`Self::carriers`] is. See
    /// [`ProtoEqTable`].
    proto_eq_calls: ProtoEqTable,
    /// W7-49 — side-table keys that were asked to hold two DIFFERENT decisions at once. Filled by
    /// [`record_call_table_entry`] (never by ordinary type errors) and returned alongside the three
    /// tables, because this pass DISCARDS its type errors — `self.error` would be swallowed here.
    /// The compiler turns the first entry into a hard `CompileError`: an aliased key means the
    /// backend cannot tell two expressions apart, which is a silent wrong VALUE, not a slow path.
    table_conflicts: Vec<(Span, String)>,
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
    /// `str`, so it crosses BY VALUE — a closure outliving its defining frame, and the `spawn:`
    /// airlock, both stay correct. `T.static_method()` is accepted ONLY when `T` is in here, which
    /// is exactly what the compiler can lower (`compiler::FnComp::witness_ref` is the other half).
    /// A type param of the enclosing TYPE (`struct Bx[T]`) is never in here: its
    /// witness would have to live in the instance, which is a different mechanism.
    witness_scope: Vec<String>,
    /// M24-2 — every METHOD NAME in the program that some declaration takes a hidden witness for (a
    /// method of a struct / enum / newtype declaring a type param with a static-carrying bound).
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
    /// [`Self::kw_frag_ord`], the fragment's 0-based index in that string. Because each `{…}` fragment
    /// is re-lexed from a fresh source its sub-expression spans restart at `(1,1)`, so span alone
    /// cannot tell two fragments apart; these two fields disambiguate them in the [`KeywordKey`]. Set
    /// (save/restore for nesting) around each fragment in `check_interpolation`; the compiler and
    /// interp maintain the identical pair at their own interpolation boundaries.
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
    /// `from`-imported names that are numeric-polymorphic native fns (`abs`/`min`/`max`), so a bare
    /// call resolves their result type by argument type instead of the float-only `FnSig` (gap #12).
    imported_poly: std::collections::HashSet<String>,
    /// `from`-imported module GLOBALS (`import COUNT from lib.st`) → the dotted module path they came
    /// from. A from-imported global is a SNAPSHOT copy (Python-identical), so REBINDING the bare name
    /// would write a local alias that is silently lost — rejected in `check_assign`, consistent with
    /// the qualified form (`st.COUNT = 5`). Mutating THROUGH the binding (`LST.push(7)`) is untouched:
    /// a container is the same heap object. Per-module: cleared in `begin_module`.
    imported_values: HashMap<String, String>,
    /// Subset of `imported_values` whose source binding was `const` (a native constant, or a `const`
    /// top-level let). The rebind guard reports these as const rather than as a mutable snapshot copy.
    /// Per-module: cleared in `begin_module` alongside `imported_values`.
    imported_consts: std::collections::HashSet<String>,
    /// Fixed-width C-ABI integer TYPE names (`int8`..`uint64`) imported into the *current* module from
    /// `std.ffi` (`import int32 from std.ffi`). These are NOT callable values — they only gate
    /// `resolve_type`, which maps a width name to `Ty::Int` iff it's in this set (else an unknown-type
    /// error). Per-module: cleared in `begin_module` so module B can't use a name module A imported.
    imported_ffi_types: std::collections::HashSet<String>,
    /// The four runtime concurrency ctor/TYPE names (`Shared`/`RwShared`/`Atomic`/`Executor`) imported
    /// into the *current* module from `std.concurrency` (whole-module `import std.concurrency` licenses
    /// all four; selective `import Shared from std.concurrency` licenses just the named ones). Like
    /// `imported_ffi_types`, these are NOT callable values — they only gate `resolve_type` /
    /// `infer_named_call`, which accept the bare name iff it's in this set (else an unknown-type error
    /// with the `import std.concurrency` hint). Per-module: cleared in `begin_module`. std/* modules
    /// are EXEMPT (they may use the four bare) via `current_module_is_stdlib`.
    imported_concurrency: std::collections::HashSet<String>,
    /// The opcode-backed `timer` builtin licensed into the *current* module from `std.time` (whole-
    /// module `import std.time` OR selective `import timer from std.time`). Like `imported_concurrency`,
    /// this is NOT a callable value — it only gates `infer_named_call`, which accepts the bare
    /// `timer(ms)` call iff `"timer"` is in this set (else an unknown-function error with the
    /// `import std.time` hint). Per-module: cleared in `begin_module`. std/* modules are EXEMPT (they
    /// may call `timer` bare) via `current_module_is_stdlib`.
    imported_time: std::collections::HashSet<String>,
    /// The std.net TCP handle TYPE names (`Socket`/`Listener`) licensed into the *current* module from
    /// `std.net` (whole-module `import std.net` licenses both; selective `import Socket from std.net`
    /// licenses just the named ones). Like `imported_concurrency`, these are NOT callable values — they
    /// only gate `resolve_type`, which accepts the bare `Socket`/`Listener` annotation iff it's in this
    /// set (else an unknown-type error with the `import std.net` hint; the ctors are `connect`/`listen`,
    /// already real native fns). Per-module: cleared in `begin_module`. std/* modules are EXEMPT (they
    /// may use the two bare) via `current_module_is_stdlib`.
    imported_net: std::collections::HashSet<String>,
    /// R2 — the std.io `Writer` handle TYPE name licensed into the *current* module from `std.io`
    /// (whole-module `import std.io` or selective `import Writer from std.io`). Like `imported_net`, not
    /// a callable value — it only gates `resolve_type`, which accepts the bare `Writer` annotation iff
    /// it's in this set (else an unknown-type error with the `import std.io` hint; values come from
    /// `create`/`append`/`stdout`/`stderr`/`buffered`, already real native fns). Per-module: cleared in
    /// `begin_module`. std/* modules are EXEMPT (may use `Writer` bare) via `current_module_is_stdlib`.
    imported_io: std::collections::HashSet<String>, // R2/R2b — licenses BOTH `Writer` and `Reader`
    /// The bare type names that an `import` licensed into the *current* module as a
    /// `StructOrigin::Builtin` struct layout (the std struct-modeled natives — `Ref`/`Match`/
    /// `Response`/`ProcResult` and every other import-gated std struct like `Token`/`Heap`/`Deque`).
    /// Populated at the two struct-import insert sites (whole-module `import std.regex` and selective
    /// `import Match from std.regex`), keyed on `info.origin == StructOrigin::Builtin`. A same-named
    /// user `struct X` decl IN A MODULE THAT IMPORTED `X` is then rejected as `reserved (builtin)` —
    /// closing the soundness hole where the user layout overwrote the Builtin seed yet the runtime
    /// still returned/constructed the native shape (a check-clean program that trapped at runtime).
    /// NOT reusable from `self.structs` membership: `Match`/`Response`/`ProcResult` are seeded with
    /// Builtin origin GLOBALLY even without any import, and THAT seeded-but-not-imported state is the
    /// legal bare-decl case — only the import EVENT licenses the name, so it's tracked separately.
    /// Per-module: cleared in `begin_module`.
    imported_builtin_types: std::collections::HashSet<String>,
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
    /// True while inferring the arguments of a generic METHOD call ([`Checker::infer_generic_method`],
    /// the ONE prepass whose bare-ident arg is re-pinned afterwards by
    /// [`Checker::try_pin_generic_fn_value_arg`]). Such an argument gets no `expected_hint` — the slot
    /// type isn't substituted yet — so a same-module generic fn passed there types rigid
    /// (`fn(T) -> str` for `[1,2,3].map(conv)`) and only becomes concrete after the pin. The
    /// "not determined here" wall in [`Checker::infer_ident`] must therefore stay silent for it: the
    /// read is not yet the final word on the type.
    ///
    /// SET AT THAT ONE CALL SITE, not inside `infer_generic_arg_tys`. The helper's other six callers
    /// (generic free fn, struct / qualified-struct / enum-variant / newtype ctor) pin nothing
    /// afterwards, so there the read IS final and the wall must fire. Setting it in the shared helper
    /// silenced all seven and let `Bx(ident)` through to the very "argument 1 of 'f': expected T,
    /// found int" this rule exists to replace. Separate from `generic_arg_prepass`, which also changes
    /// closure-param binding and hover recording.
    generic_fn_value_prepass: bool,
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
    /// One-way int→float ELEMENT-widening license for the collection literal directly bound to an
    /// annotated `let` (`xs: List[float] = [1, f]`). SEPARATE from `expected_hint` on purpose:
    /// `expected_hint` is also set for call arguments, and licensing off it would re-open the hole
    /// (`f([a, 2.5])` into a `List[float]` param — the compiler has NO annotation there and cannot
    /// coerce). This mirrors the compiler's own `float_elem_hint` exactly (same `let`-only set site,
    /// same `take()`-at-expr-entry clear), which is what makes the checker's accepted set a subset of
    /// what the compiler lowers.
    float_elem_hint: Option<crate::ast::ElemFloatHint>,
    /// For each `spawn:` block body currently being checked, the local-scope depth (`scopes.len()`)
    /// at the point the task body opened. A binding living at a scope index *below* the innermost
    /// floor is a **captured** binding — read-only inside the task (assigning to it is an error).
    /// Empty outside any `spawn:` block.
    capture_floors: Vec<usize>,
    /// B3.3 (Task 2a) — per-scope side-table of the NON-SENDABLE LOCAL captures of each
    /// closure/nested-fn value declared in that scope, keyed by the bound name. Mirrors `scopes`
    /// index-for-index (pushed/popped by `push_scope`/`pop_scope`). Populated at the closure/nested-fn
    /// DECL site (a `let name := fn(...)` RHS or a nested `fn name(...)` body) using the SAME free-var
    /// over-approximation (`free_names_*`) the runtime uses to build the closure's captures — so the
    /// gate matches exactly what actually crosses the airlock. Consulted at a `spawn <name>()` callee /
    /// `spawn f(<name>)` arg site to reject a captured `ref` (or other non-sendable local) at compile
    /// time, mirroring the `spawn:` block form. A module-global (scope 0) capture is EXCLUDED at record
    /// time (a read-only global, not a per-task capture — never gated).
    capture_table: Vec<HashMap<String, Vec<Capture>>>,
    /// True while checking a `std.*` module — structs hoisted now are tagged `StructOrigin::Builtin`.
    current_module_is_stdlib: bool,
    /// Phase 4c-net — the harvested `StructInfo` (method table) for std.net's reserved `Socket` /
    /// `Listener` handles, captured from the file-backed `std/net.chz` harvest the first time std.net is
    /// checked in the graph. `Socket`/`Listener` resolve to the RESERVED `Ty::Socket`/`Ty::Listener`
    /// (opaque VM handles, NOT nominal structs), so — unlike Match/Response — their layout is not seeded
    /// for field access; only the METHOD table is re-seeded (bare, into `self.structs["Socket"]` /
    /// `["Listener"]`) by [`seed_stdlib_structs`] so `socket.read(...)` / `listener.accept(...)` resolve
    /// via the normal method path in every module. Bare-name annotation stays import-gated by
    /// `imported_net` + `resolve_type`'s reserved arm — the seed adds NO bare licensing. `None` until
    /// std.net is harvested (and on the single-module `check` path, which has no graph).
    net_socket_seed: Option<StructInfo>,
    net_listener_seed: Option<StructInfo>,
    /// R2 — the harvested `StructInfo` (method table) for std.io's reserved `Writer` handle, captured
    /// from the file-backed `std/io.chz` harvest the first time std.io is checked in the graph. Like
    /// `net_socket_seed`, `Writer` resolves to the RESERVED `Ty::Writer` (opaque VM handle) — only the
    /// METHOD table is re-seeded (bare, into `self.structs["Writer"]`) by `seed_stdlib_structs` so
    /// `w.write(...)` / `w.close(...)` resolve via the normal method path in every module. Bare-name
    /// annotation stays import-gated by `imported_io` + `resolve_type`'s reserved arm. `None` until
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
    /// import-gated by `imported_concurrency` + `resolve_type`'s reserved arm. Empty until harvested.
    concurrency_seeds: HashMap<String, StructInfo>,
    /// The harvested `FnSig` for `std.time`'s `timer(ms) -> Channel[bool]`, captured from the
    /// file-backed `std/time.chz` harvest. `timer` is a BARE-CALLABLE opcode builtin (lowers to
    /// `Op::NewTimer`, carries no runtime value), so — unlike now/monotonic/sleep_ms/format — its sig is
    /// routed HERE by `harvest_native_module` rather than into `sig.functions` (which the From-import arm
    /// would bind as a normal callable, breaking bare-callability). The bare `timer(...)` expr arm reads
    /// its arg/return types from this field; the import-license stays in `sig.types`. `None` until
    /// std.time is harvested (and on the lone single-module `check` path with no graph — the arm falls
    /// back to the built-in `[int] -> Channel[bool]` shape).
    time_timer_sig: Option<FnSig>,
    /// Phase 5a-containers — the harvested `StructInfo` (method table) for each of the three RESERVED
    /// UNIVERSE container types (`List`/`Map`/`Set`), keyed by bare name, captured from the always-linked
    /// `std/prelude.chz`'s `native struct` decls the first time the prelude module is checked in the graph
    /// (order[0], before entry/all others). Like `concurrency_seeds`, these resolve to
    /// the RESERVED `Ty::List`/`Ty::Map`/`Ty::Set` (NOT nominal structs) — only the METHOD table is
    /// re-seeded (bare, into `self.structs[name]`) by [`seed_stdlib_structs`] so `xs.push(...)` /
    /// `m.get(...)` / `s.add(...)` resolve via the normal method path, with each generic method sig's
    /// `Ty::Param` substituted with the value's element/key/value type at the call site. The literal
    /// syntax + turbofish ctor stay compiler-wired (`resolve_type`/`builtin_container_sig`). Unlike the
    /// import-gated seeds, these are UNIVERSE (always in scope) — no licensing set gates them. Empty until
    /// harvested.
    container_seeds: HashMap<String, StructInfo>,
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
    /// `from`-imported + std whole-module. Mirrors the compiler's `bare_types`; resolves a bare-written
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
    /// newtype/alias) + top-level lets, keyed by simple name. Populated per module from the AST in
    /// `collect_docs`; consulted by the hover Ident/type-name sites. Keyed by simple name is safe
    /// because hover only fires in the entry module (gated by `current_module_id == hover_entry`),
    /// where this table holds exactly that module's decls (mirrors how `functions` is entry-scoped).
    /// Free fns / methods carry their doc on `FnSig::doc` instead. Runtime-inert (LSP only).
    name_docs: HashMap<String, String>,
    /// PART A — pending "un-constrained empty collection" sites: a local `b := []`/`{}`/`Set()` whose
    /// element/key/value slot is still `Unknown` (an un-annotated empty literal). Each entry is
    /// `(owning_scope_idx, name, decl_span)`. A later constraining op (`push`/`add`/`insert`/`extend`,
    /// `m[k]=v`) calls `drop_empty_site` to clear the requirement; at end-of-scope (fn body / module)
    /// `finalize_empty_coll_sites` errors on any site whose binding STILL carries `Unknown`-in-slot
    /// (never refined → no element type could be inferred → require an annotation).
    empty_coll_sites: Vec<(usize, String, Span)>,
    /// PART B — retroactive hover: when the hover probe lands on an occurrence of a binding whose
    /// recorded type still carries `Unknown`-in-slot (a not-yet-refined empty collection), we stash the
    /// binding's `(owning_scope_idx, name, kind, doc)` here INSTEAD of locking `hover_result`, then at
    /// the end-of-scope seam that OWNS the binding overwrite `hover_result` with the binding's FINAL
    /// (refined) type. The owning-scope index gates the finalize so an intervening inner fn/method seam
    /// can't resolve it prematurely to the still-unrefined type. Probe-gated; parity-neutral.
    hover_pending: Option<(usize, String, HoverKind, Option<String>)>,
}

mod expr;
mod pattern;
// `pub(crate)` for `proto::INTRINSIC_PROTO_METHODS` — the intrinsic-grant ↔ VM-arm pairing table,
// which `vm::tests::intrinsic_grants_all_have_vm_arms` reads to assert the pairing (W6-3).
pub(crate) mod proto;
mod setup;
mod sig;

/// Render a resolved type-arg list for a user-facing redirect hint (`int, str`), used by the
/// removed-gliding-form error to suggest the type-side form `Enum[int, str].Variant(...)`.
fn render_targs(targs: &[Ty]) -> String {
    targs
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The first identifier bound more than once WITHIN a single pattern (Rust's rule), or `None`. Walks
/// a pattern's binders but COUNTS instead of dedup'ing: `_` (wildcard) binds nothing and is
/// skipped; literals/ranges bind nothing. Or-alternatives are NOT descended — each `A(x) | B(x)`
/// alternative is its own binding context (consistency across alts is governed elsewhere); a
/// duplicate inside one alternative is caught when `bind_match_arm` recurses on that alternative.
/// `is_binder(name)` returns `true` iff a bare `Pattern::Ident(name)` actually BINDS a fresh variable
/// here, rather than naming a (refutable) nullary variant. A bare `None`/`Ok`/`Err`/`Some` or a user
/// variant name binds nothing (it's a variant test — see `bind_subpattern`), so two of them in one
/// pattern (e.g. `(None, None, None)`) is NOT a duplicate binding. Mirrors `bind_subpattern`'s
/// Ident-as-variant recognition so this pre-pass doesn't falsely reject correct code.
fn first_duplicate_binder(p: &Pattern, is_binder: &impl Fn(&str) -> bool) -> Option<String> {
    fn go(
        p: &Pattern,
        seen: &mut std::collections::HashSet<String>,
        is_binder: &impl Fn(&str) -> bool,
    ) -> Option<String> {
        match p {
            Pattern::Ident(n, _) => {
                if !is_binder(n) {
                    return None;
                }
                if !seen.insert(n.clone()) {
                    return Some(n.clone());
                }
                None
            }
            Pattern::Variant { bindings, .. } | Pattern::Tuple(bindings) => {
                for b in bindings {
                    if let Some(dup) = go(b, seen, is_binder) {
                        return Some(dup);
                    }
                }
                None
            }
            // An or-pattern's alternatives are separate binding contexts, so a name reused ACROSS
            // alternatives (`A(x) | B(x)`) is NOT a duplicate. But (a) each alternative must itself
            // be duplicate-free, and (b) the names an or-pattern binds STILL collide with binders
            // OUTSIDE the or (`(x, A(x) | B(x))` binds `x` twice on a matching path). So check each
            // alternative on its own, then merge the or's binder set ONCE into the outer `seen`.
            Pattern::Or(alts) => {
                let mut or_binds = std::collections::BTreeSet::new();
                for alt in alts {
                    let mut alt_seen = std::collections::HashSet::new();
                    if let Some(dup) = go(alt, &mut alt_seen, is_binder) {
                        return Some(dup);
                    }
                    or_binds.extend(alt_seen);
                }
                for n in or_binds {
                    if !seen.insert(n.clone()) {
                        return Some(n);
                    }
                }
                None
            }
            Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => None,
        }
    }
    let mut seen = std::collections::HashSet::new();
    go(p, &mut seen, is_binder)
}

/// The prebuilt protocols every program starts with. `Comparable` requires
/// `compare(self, other: Self) -> int`; primitives (int/float/str) satisfy it intrinsically.
/// M22 — structural equality of two protocol method signatures (params + return + arity), used to
/// dedup identical embed-pulled methods (the legal diamond) vs. flag a conflicting-signature embed.
fn fn_sig_eq(a: &FnSig, b: &FnSig) -> bool {
    a.params == b.params && a.ret == b.ret && a.min_params == b.min_params
}

fn prebuilt_protocols() -> HashMap<String, ProtocolInfo> {
    let mut m = HashMap::new();
    m.insert(
        // `Any` — the TOP type: an EMPTY structural protocol (zero embeds, zero methods) so EVERY
        // type satisfies it (scalars included — see the empty-protocol short-circuit in
        // `satisfies_args_d`). This stays the RUNTIME source of truth; since empty protocols are now
        // expressible (`protocol Any:\n    pass`), it is ALSO mirrored + drift-guarded in
        // `std/prelude.chz` like the other reserved protocols. A user empty protocol behaves
        // identically (the accept-all behaviour is not special-cased on the name "Any"). Used as the
        // honest element type of a variadic display slot (`print(...args: Any)`); NOT dynamic typing —
        // a value typed `Any` carries no methods, so it can only be passed around / displayed.
        "Any".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            methods: Vec::new(),
        },
    );
    m.insert(
        // `Comparable` embeds `Eq` (M23) — mirrors Rust's `Ord: Eq`: a type ordered must also be
        // equatable, and `eq` must agree with `compare` (the implementor's contract, unchecked). Built
        // with the SAME `embeds` field `Arithmetic` uses — no special-casing. int/float/str still need
        // no explicit `eq` method: `satisfies_args_d`'s embed-flattening loop DOES run for them now
        // (embeds is no longer empty), but it recurses into `Eq`'s OWN intrinsic-grant early-out (every
        // scalar satisfies `Eq`), so the embed passes trivially before `Comparable`'s own intrinsic
        // grant is reached — no `Comparable`-specific short-circuit needed.
        "Comparable".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: vec![Bound {
                name: "Eq".to_string(),
                args: Vec::new(),
            }],
            // receiver `self` (Unknown), `other: Self` (Param "Self"), returning int.
            methods: vec![(
                "compare".to_string(),
                FnSig::plain(vec![Ty::Unknown, Ty::Param("Self".into())], Ty::Int),
            )],
        },
    );
    m.insert(
        // `Eq` — user-defined equality: `eq(self, other: Self) -> bool`. Every scalar satisfies it
        // intrinsically (all FOUR — `==` is defined on `bool` too, unlike `Comparable`'s ordering);
        // a struct/enum satisfies it structurally through its own `eq`. Embedded by `Comparable` (M23):
        // a type ordered must also be equatable.
        "Eq".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            // receiver `self` (Unknown), `other: Self` (Param "Self"), returning bool.
            methods: vec![(
                "eq".to_string(),
                FnSig::plain(vec![Ty::Unknown, Ty::Param("Self".into())], Ty::Bool),
            )],
        },
    );
    m.insert(
        "Stringable".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            // receiver `self` (Unknown) only, returning str. A struct with `str(self) -> str`
            // satisfies it; `print`/`str()`/interpolation dispatch to that method at runtime.
            methods: vec![("str".to_string(), FnSig::plain(vec![Ty::Unknown], Ty::Str))],
        },
    );
    m.insert(
        // The default error type (Go-style `error`): one method `message(self) -> str`. `str`
        // conforms intrinsically; any struct with `message(self) -> str` conforms structurally.
        "Error".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            methods: vec![(
                "message".to_string(),
                FnSig::plain(vec![Ty::Unknown], Ty::Str),
            )],
        },
    );
    m.insert(
        // W7-8 — `PathLike`: the INPUT position of every path-taking std fn (Python's `os.PathLike`,
        // Rust's `AsRef<Path>`). Its sole method `as_path(self) -> bytes` hands back the RAW OS bytes,
        // so a non-UTF-8 filename never has to round-trip through the validated-UTF-8 `str`.
        // `str`/`bytes`/`bytearray` conform INTRINSICALLY (they have no `as_path` of their own — the
        // grant rows in `INTRINSIC_PROTO_METHODS` + the `satisfies_args_d` early-out are the only
        // seam); `path.Path` conforms STRUCTURALLY through its own `as_path`.
        // Deliberately DISTINCT from the future byte-DATA protocol's `as_bytes`, so a type satisfying
        // both has no method ambiguity.
        "PathLike".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            methods: vec![(
                "as_path".to_string(),
                FnSig::plain(vec![Ty::Unknown], Ty::Bytes),
            )],
        },
    );
    m.insert(
        "Hashable".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            // receiver `self` (Unknown) only, returning int. A struct with `hash(self) -> int`
            // satisfies it. WIRED TO MAP/SET KEYS: `map`/`set` are real hash tables (insertion-order
            // entries + a hash→position index), so any `Hashable` type can be a key/element — the
            // scalars int/str/bool intrinsically, or a struct via its `hash()` (dispatched at
            // runtime, hash confirmed by structural `==`). CONTRACT: two structurally-equal structs
            // (the `==` used to confirm a probe) MUST return the same `hash()` — the implementor owns
            // this (like Rust's `Hash`/`Eq`); the checker can't enforce purity.
            methods: vec![("hash".to_string(), FnSig::plain(vec![Ty::Unknown], Ty::Int))],
        },
    );
    // Per-operator numeric protocols (M10-G3, M22): a struct satisfying `Add`/`Sub`/`Mul`/`Div`/`Mod`
    // (method `add`/`sub`/`mul`/`div`/`mod`(self, other: Self) -> Self) overloads `+`/`-`/`*`/`/`/`%`.
    // `Self` for `other` and the return makes them binary same-type operators (mirrors `Comparable`'s
    // `compare`).
    for (proto, method) in [
        ("Add", "add"),
        ("Sub", "sub"),
        ("Mul", "mul"),
        ("Div", "div"),
        ("Mod", "mod"),
    ] {
        m.insert(
            proto.to_string(),
            ProtocolInfo {
                type_params: Vec::new(),
                embeds: Vec::new(),
                methods: vec![(
                    method.to_string(),
                    FnSig::plain(
                        vec![Ty::Unknown, Ty::Param("Self".into())],
                        Ty::Param("Self".into()),
                    ),
                )],
            },
        );
    }
    // `Neg` (M22) — the UNARY `-` protocol: a struct/type-param satisfying `Neg` (method
    // `neg(self) -> Self`, one param `self` only, no `other`) overloads unary `-`. int/float satisfy
    // it intrinsically (their native negation). Mirrors `Stringable`'s single-`self`-param shape.
    m.insert(
        "Neg".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: Vec::new(),
            methods: vec![(
                "neg".to_string(),
                FnSig::plain(vec![Ty::Unknown], Ty::Param("Self".into())),
            )],
        },
    );
    // `Arithmetic` (M22) — a builtin protocol BUNDLE: embeds `Add + Sub + Mul + Div` and adds no own
    // methods. `[T: Arithmetic]` flattens to "has add/sub/mul/div", so int/float and any 4-op struct
    // satisfy it. Built with the SAME `embeds` field user bundles use — no special-casing of builtins.
    m.insert(
        "Arithmetic".to_string(),
        ProtocolInfo {
            type_params: Vec::new(),
            embeds: vec![
                Bound {
                    name: "Add".to_string(),
                    args: Vec::new(),
                },
                Bound {
                    name: "Sub".to_string(),
                    args: Vec::new(),
                },
                Bound {
                    name: "Mul".to_string(),
                    args: Vec::new(),
                },
                Bound {
                    name: "Div".to_string(),
                    args: Vec::new(),
                },
            ],
            methods: Vec::new(),
        },
    );
    // `Iterator[T]` — the language's one parameterized protocol. The method shape mirrors the
    // structural detection (`next(self) -> Option[T]`); conformance is decided in `satisfies` — a cursor
    // (`.iter()` / a generator result) or a user struct with a structural `next`, NOT a raw collection
    // (which holds no position and satisfies only `Iterable`, W6-3b) and NOT the generic structural
    // loop. The bound's `[T]` arg recovers the element type at call sites.
    m.insert(
        "Iterator".to_string(),
        ProtocolInfo {
            // One type param (the element) so the generic arity check in `check_bounds` treats
            // `Iterator[T]` uniformly; its conformance + element recovery stay special-cased.
            type_params: vec!["Elem".to_string()],
            embeds: Vec::new(),
            methods: vec![(
                "next".to_string(),
                FnSig::plain(
                    vec![Ty::Unknown],
                    Ty::Option(Box::new(Ty::Param("Self".into()))),
                ),
            )],
        },
    );
    // `Iterable[T]` — "can produce a fresh cursor". The method shape mirrors the structural detection
    // (`iter(self) -> Iterator[T]`); conformance is decided in `satisfies_args` (built-in collections
    // + any `Iterator[T]` intrinsically — every Iterator IS Iterable via `iter() == self` — plus a
    // user struct with a structural `iter`), NOT the generic structural loop, and the bound's `[T]`
    // recovers the element type at call sites (`infer_method_call`'s `Iterable.iter` special-case).
    // Distinct from `Iterator[T]`: an `Iterable` only promises a cursor; an `Iterator` also has `next`.
    // So `[S: Iterable[T], T]` is the bound that accepts ANY iterable (Rust's `IntoIterator`), and
    // `[S: Iterator[T], T]` is the one that needs a real cursor (Rust's `Iterator`).
    m.insert(
        "Iterable".to_string(),
        ProtocolInfo {
            type_params: vec!["Elem".to_string()],
            embeds: Vec::new(),
            methods: vec![(
                "iter".to_string(),
                FnSig::plain(
                    vec![Ty::Unknown],
                    Ty::Struct("Iterator".to_string(), vec![Ty::Param("Elem".into())]),
                ),
            )],
        },
    );
    // The indexing pair (Rust `Index`/`IndexMut`-style) + `Slice`. Built-in `list`/`map`/`str`
    // satisfy these intrinsically (see `satisfies_args`); a user struct satisfies them structurally
    // via `index`/`set_index`/`slice`. `K`/`V`/`R` are recovered at call sites by `recover_index_args`
    // (mirrors `Iterator[T]`'s element recovery).
    m.insert(
        "Index".to_string(),
        ProtocolInfo {
            type_params: vec!["K".to_string(), "V".to_string()],
            embeds: Vec::new(),
            methods: vec![(
                "index".to_string(),
                FnSig::plain(
                    vec![Ty::Unknown, Ty::Param("K".into())],
                    Ty::Param("V".into()),
                ),
            )],
        },
    );
    m.insert(
        "IndexSet".to_string(),
        ProtocolInfo {
            type_params: vec!["K".to_string(), "V".to_string()],
            embeds: Vec::new(),
            methods: vec![
                (
                    "index".to_string(),
                    FnSig::plain(
                        vec![Ty::Unknown, Ty::Param("K".into())],
                        Ty::Param("V".into()),
                    ),
                ),
                (
                    "set_index".to_string(),
                    FnSig::plain(
                        vec![Ty::Unknown, Ty::Param("K".into()), Ty::Param("V".into())],
                        Ty::Nil,
                    ),
                ),
            ],
        },
    );
    m.insert(
        "Slice".to_string(),
        ProtocolInfo {
            type_params: vec!["R".to_string()],
            embeds: Vec::new(),
            methods: vec![(
                "slice".to_string(),
                // Python-style: three `Option[int]` components (start/end/step), each `None` when
                // omitted. Both engines always pass all three explicitly.
                FnSig::plain(
                    vec![
                        Ty::Unknown,
                        Ty::option(Ty::Int),
                        Ty::option(Ty::Int),
                        Ty::option(Ty::Int),
                    ],
                    Ty::Param("R".into()),
                ),
            )],
        },
    );
    // `Convert[S]` — the extensible type-conversion protocol (Rust `From`, target-keyed). Its sole
    // method `convert(x: S) -> Self` is STATIC (first param is `x: S`, NOT `self`), so it harvests to
    // params `[Ty::Param("S")]` (no leading `Ty::Unknown` self slot) with `is_static: true`. `S` is the
    // one arity param, so the bound reads `[T: Convert[str]]`. Slice 1: declared + reserved +
    // bound-parseable only; structural witnessing (slice 2) + `T.convert` through a bound (slice 3) are
    // NOT wired yet. `is_static` is load-bearing for those slices but NOT compared by `fn_sig_eq`/the
    // drift guard (params + ret + min_params only); set it to match the parsed prelude shape.
    m.insert(
        "Convert".to_string(),
        ProtocolInfo {
            type_params: vec!["S".to_string()],
            embeds: Vec::new(),
            methods: vec![("convert".to_string(), {
                let mut sig = FnSig::plain(vec![Ty::Param("S".into())], Ty::Param("Self".into()));
                sig.is_static = true;
                sig
            })],
        },
    );
    // `Contains[Item]` — the membership protocol (Python `__contains__`): `x in obj` dispatches to
    // `contains(self, item: Item) -> bool`. Built-in list/set/map/str test membership intrinsically
    // (see `op_contains`); a user struct/enum satisfies it structurally. `Item` (the element type) is
    // recovered at the `in` site by `contains_item_ty` (mirrors `Index[K,V]`).
    m.insert(
        "Contains".to_string(),
        ProtocolInfo {
            type_params: vec!["Item".to_string()],
            embeds: Vec::new(),
            methods: vec![(
                "contains".to_string(),
                FnSig::plain(vec![Ty::Unknown, Ty::Param("Item".into())], Ty::Bool),
            )],
        },
    );
    m
}

/// The substitution from a struct's type parameters to a concrete instantiation's type arguments
/// (`Stack[int]` ⇒ `{T: int}`). Empty for a non-generic struct.
fn struct_param_map(info: &StructInfo, targs: &[Ty]) -> HashMap<String, Ty> {
    info.type_params
        .iter()
        .map(|tp| tp.name.clone())
        .zip(targs.iter().cloned())
        .collect()
}

/// Is `ty` fully concrete — free of any type parameter or `Unknown`? Used to decide when a forwarded
/// parameterized bound's args can be compared strictly (only a concrete-vs-concrete mismatch is an
/// error; anything still generic forwards loosely).
fn ty_fully_concrete(ty: &Ty) -> bool {
    match ty {
        Ty::Unknown | Ty::Param(_) => false,
        Ty::List(x) | Ty::Option(x) | Ty::Set(x) => ty_fully_concrete(x),
        Ty::Map(k, v) => ty_fully_concrete(k) && ty_fully_concrete(v),
        Ty::Result(a, b) => ty_fully_concrete(a) && ty_fully_concrete(b),
        Ty::Struct(_, a) | Ty::Enum(_, a) => a.iter().all(ty_fully_concrete),
        // A parameterized protocol existential carrying a free type-param (`Container[T]`) is NOT
        // concrete — recurse into the carried args (no catch-all Protocol laundering).
        Ty::Protocol(_, a) => a.iter().all(ty_fully_concrete),
        Ty::Tuple(ts) => ts.iter().all(ty_fully_concrete),
        Ty::Func { params, ret, .. } => {
            params.iter().all(ty_fully_concrete) && ty_fully_concrete(ret)
        }
        _ => true,
    }
}

/// Does `ty` still mention a type PARAMETER anywhere? Strictly weaker than `!ty_fully_concrete`: a
/// `Ty::Unknown` does NOT count. That split is the whole point — an `Unknown` is the empty-collection
/// / cascade sentinel (a slot nothing filled), whereas a surviving `Ty::Param` is a type parameter
/// that was never determined. Arm-for-arm identical to `ty_fully_concrete`, so the two stay in
/// lockstep as `Ty` grows.
fn ty_has_param(ty: &Ty) -> bool {
    match ty {
        Ty::Param(_) => true,
        Ty::Unknown => false,
        Ty::List(x) | Ty::Option(x) | Ty::Set(x) => ty_has_param(x),
        Ty::Map(k, v) => ty_has_param(k) || ty_has_param(v),
        Ty::Result(a, b) => ty_has_param(a) || ty_has_param(b),
        Ty::Struct(_, a) | Ty::Enum(_, a) | Ty::Protocol(_, a) => a.iter().any(ty_has_param),
        Ty::Tuple(ts) => ts.iter().any(ty_has_param),
        Ty::Func { params, ret, .. } => params.iter().any(ty_has_param) || ty_has_param(ret),
        _ => false,
    }
}

/// The verdict of [`pin_generic_fn_value`] — the ONE answer to *"does this position determine every
/// type parameter of the generic function read here?"*.
enum FnValuePin {
    /// Every type param bound to a concrete type. Carries the bindings (so the caller can
    /// `enforce_bounds`) and the fully-substituted concrete `fn(..) -> ..` the value takes on.
    Pinned(HashMap<String, Ty>, Ty),
    /// A type param is unbound, or bound to a still-free `Ty::Param`. Nothing about this position
    /// can determine it, so the read cannot become a function value — the rule reports.
    Undetermined,
    /// Not this rule's business, and never an error here: the slot is not a matching-arity `fn(..)`
    /// (an arity / shape diagnostic owns that and says it accurately), the slot's own PARAMETER
    /// positions are not concrete (`[].map(ident)`'s `fn(?) -> U` — the empty-collection sentinel,
    /// which runs fine and prints `[]`), or the pin came out `Unknown`-cored.
    Skip,
}

/// THE derivation behind the uninstantiated-generic-function-value rule, asked at every position a
/// generic fn is read as a VALUE: a binding / annotation / return (via `infer_ident`'s expected-type
/// hint), a generic method's argument slot (the interleaved pin in `try_pin_generic_fn_value_arg`),
/// and again at the END of that call, where `report_undetermined_generic_fn_value_args` reports the
/// [`FnValuePin::Undetermined`] ones. `declared` is the fn's own signature (type params still free);
/// `want` is the expected type / declared slot with everything known SO FAR substituted in.
///
/// ORDERING: this is a pure question about the bindings it is handed, so *when* it is asked is the
/// caller's responsibility. The argument-position caller must ask it only once the whole call has
/// been inferred — `[1,2,3].fold(0, pick)` pins `pick`'s `T` from the FIRST argument while `pick` is
/// the SECOND, so asking per-argument would refuse a program Go accepts.
fn pin_generic_fn_value(type_params: &[TypeParam], declared: &Ty, want: &Ty) -> FnValuePin {
    let (Ty::Func { params: dp, .. }, Ty::Func { params: wp, .. }) = (declared, want) else {
        return FnValuePin::Skip;
    };
    // Only the slot's PARAMETERS are gated: its RETURN may legitimately be `Unknown` (a
    // discarded-return HOF slot like `for_each`'s `fn(int) -> ?`) and the question is still
    // answerable — a fn whose `T` appears nowhere is undetermined there just the same.
    if dp.len() != wp.len() || !wp.iter().all(ty_fully_concrete) {
        return FnValuePin::Skip;
    }
    let mut map: HashMap<String, Ty> = HashMap::new();
    unify(declared, want, &mut map);
    if !type_params.iter().all(|tp| map.contains_key(&tp.name)) {
        return FnValuePin::Undetermined;
    }
    let refined = subst(declared, &map);
    if ty_has_param(&refined) {
        return FnValuePin::Undetermined;
    }
    if !ty_fully_concrete(&refined) {
        return FnValuePin::Skip;
    }
    FnValuePin::Pinned(map, refined)
}

/// True iff `t` is a COMPOUND type whose recursive structure contains a `Ty::Unknown` anywhere in a
/// type-argument / element / key / value position. A bare top-level `Ty::Unknown` returns FALSE —
/// that is the cascade-suppression sentinel (a real type error already happened, or a permissive
/// receiver); refining it would fight cascade-suppression. Drives the refine-on-first-use gate: we
/// only narrow a binding whose empty-slot Unknown is reachable, never the bare sentinel.
fn contains_unknown_in_slot(t: &Ty) -> bool {
    fn has_unknown(t: &Ty) -> bool {
        match t {
            Ty::Unknown => true,
            Ty::List(x)
            | Ty::Option(x)
            | Ty::Set(x)
            | Ty::Channel(x)
            | Ty::Shared(x)
            | Ty::RwShared(x)
            | Ty::Atomic(x) => has_unknown(x),
            Ty::Map(k, v) => has_unknown(k) || has_unknown(v),
            Ty::Result(a, b) => has_unknown(a) || has_unknown(b),
            Ty::Struct(_, a) | Ty::Enum(_, a) => a.iter().any(has_unknown),
            Ty::Tuple(ts) => ts.iter().any(has_unknown),
            _ => false,
        }
    }
    match t {
        Ty::Unknown => false, // bare sentinel — never refine
        _ => has_unknown(t),
    }
}

/// Structural merge for refine-on-first-use: fill `Ty::Unknown` slots in `a` with the corresponding
/// concrete slot from `shape`, recursing to arbitrary depth (so `list[Option[Box[int]]]` fills in a
/// single merge). A bare `Unknown` in `a` becomes `shape` (when `shape` is concrete). For matching
/// compounds (List/Set/Option/Channel/Shared/Atomic ×1, Map/Result ×2, Tuple ×n, Struct/Enum by
/// NAME + arity) it recurses pairwise. On a shape-NAME or arity mismatch (e.g. pushing a different
/// generic enum) it leaves `a` unchanged — no refine — so the normal `check_args` mismatch fires.
pub(crate) fn merge_unknown(a: &Ty, shape: &Ty) -> Ty {
    use Ty::*;
    if shape.is_unknown() {
        return a.clone();
    }
    if a.is_unknown() {
        return shape.clone();
    }
    match (a, shape) {
        (List(ae), List(se)) => List(Box::new(merge_unknown(ae, se))),
        (Set(ae), Set(se)) => Set(Box::new(merge_unknown(ae, se))),
        (Option(ae), Option(se)) => Option(Box::new(merge_unknown(ae, se))),
        (Channel(ae), Channel(se)) => Channel(Box::new(merge_unknown(ae, se))),
        (Shared(ae), Shared(se)) => Shared(Box::new(merge_unknown(ae, se))),
        (RwShared(ae), RwShared(se)) => RwShared(Box::new(merge_unknown(ae, se))),
        (Atomic(ae), Atomic(se)) => Atomic(Box::new(merge_unknown(ae, se))),
        (Map(ak, av), Map(sk, sv)) => Map(
            Box::new(merge_unknown(ak, sk)),
            Box::new(merge_unknown(av, sv)),
        ),
        (Result(at, ae), Result(st, se)) => Result(
            Box::new(merge_unknown(at, st)),
            Box::new(merge_unknown(ae, se)),
        ),
        (Tuple(ats), Tuple(sts)) if ats.len() == sts.len() => Tuple(
            ats.iter()
                .zip(sts)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
        ),
        (Struct(an, aa), Struct(sn, sa)) if an == sn && aa.len() == sa.len() => Struct(
            an.clone(),
            aa.iter()
                .zip(sa)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
        ),
        (Enum(an, aa), Enum(sn, sa)) if an == sn && aa.len() == sa.len() => Enum(
            an.clone(),
            aa.iter()
                .zip(sa)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
        ),
        (NewType(an, aa), NewType(sn, sa)) if an == sn && aa.len() == sa.len() => NewType(
            an.clone(),
            aa.iter()
                .zip(sa)
                .map(|(x, y)| merge_unknown(x, y))
                .collect(),
        ),
        // Shape/name/arity mismatch: leave `a` unchanged (no refine — normal mismatch fires later).
        _ => a.clone(),
    }
}

/// Substitute generic type parameters in `ty` using `map` (e.g. `Self ↦ Point`, `T ↦ int`).
/// Unmapped params are left as-is.
fn subst(ty: &Ty, map: &HashMap<String, Ty>) -> Ty {
    match ty {
        Ty::Param(n) => map.get(n).cloned().unwrap_or_else(|| ty.clone()),
        Ty::List(t) => Ty::list(subst(t, map)),
        Ty::Option(t) => Ty::option(subst(t, map)),
        Ty::Result(t, e) => Ty::result_e(subst(t, map), subst(e, map)),
        Ty::Map(k, v) => Ty::map(subst(k, map), subst(v, map)),
        Ty::Set(t) => Ty::set(subst(t, map)),
        Ty::Tuple(ts) => Ty::Tuple(ts.iter().map(|t| subst(t, map)).collect()),
        Ty::Func {
            params,
            ret,
            labels,
        } => Ty::Func {
            params: params.iter().map(|t| subst(t, map)).collect(),
            ret: Box::new(subst(ret, map)),
            // Preserve surface labels across generic substitution (they name params, not types).
            labels: labels.clone(),
        },
        // Native reserved generic handles must substitute their element param too (mirror `subst`'s
        // List/Map/… arms) so a generic wrapper struct field `ch: Channel[T]` becomes `Channel[int]`
        // after construction — without these the field stayed `Channel[T]` and every use rejected.
        Ty::Channel(t) => Ty::channel(subst(t, map)),
        Ty::Shared(t) => Ty::shared(subst(t, map)),
        Ty::Atomic(t) => Ty::atomic(subst(t, map)),
        Ty::RwShared(t) => Ty::rwshared(subst(t, map)),
        Ty::Struct(n, args) => Ty::Struct(n.clone(), args.iter().map(|t| subst(t, map)).collect()),
        Ty::Enum(n, args) => Ty::Enum(n.clone(), args.iter().map(|t| subst(t, map)).collect()),
        Ty::NewType(n, args) => {
            Ty::NewType(n.clone(), args.iter().map(|t| subst(t, map)).collect())
        }
        // A parameterized protocol existential (`Container[int]`) must recurse into its carried args
        // so DECISION-2 method-return recovery is not inert (`c.get(0)` substituting the protocol's
        // param → the carried arg flows through here). A bare `Error` has no args (a no-op).
        Ty::Protocol(n, args) => {
            Ty::Protocol(n.clone(), args.iter().map(|t| subst(t, map)).collect())
        }
        other => other.clone(),
    }
}

/// [`subst`] lifted to a whole signature — params + return only. Every other field (labels,
/// arity, `is_static`, doc) is vocabulary-independent and rides along unchanged.
fn subst_sig(sig: &FnSig, map: &HashMap<String, Ty>) -> FnSig {
    FnSig {
        params: sig.params.iter().map(|t| subst(t, map)).collect(),
        ret: subst(&sig.ret, map),
        ..sig.clone()
    }
}

/// Does the type ANNOTATION `t` mention any of `names` anywhere, at any nesting depth? Used to spot
/// an owner type param buried inside an embed's type argument (`Contains[List[T]]`), which the
/// read-only resolver cannot re-spell — see `validate_protocol_embeds`.
pub(crate) fn type_mentions_any(t: &Type, names: &[String]) -> bool {
    match t {
        Type::Named { name, .. } => names.contains(name),
        Type::Qualified { args, .. } => args.iter().any(|a| type_mentions_any(a, names)),
        Type::Generic(head, args, _) => {
            names.contains(head) || args.iter().any(|a| type_mentions_any(a, names))
        }
        Type::Func { params, ret, .. } => {
            params.iter().any(|a| type_mentions_any(a, names)) || type_mentions_any(ret, names)
        }
        Type::Tuple(ts) => ts.iter().any(|a| type_mentions_any(a, names)),
    }
}

/// The first bare name in `t` that is neither one of `names` nor accepted by `known` — i.e. a type
/// argument that will silently resolve to `Ty::Unknown`, which is permissive against every operand.
fn first_unresolvable_name(
    t: &Type,
    names: &[String],
    known: &dyn Fn(&str) -> bool,
) -> Option<String> {
    match t {
        Type::Named { name, .. } => (!names.contains(name) && !known(name)).then(|| name.clone()),
        Type::Qualified { args, .. } | Type::Generic(_, args, _) => args
            .iter()
            .find_map(|a| first_unresolvable_name(a, names, known)),
        Type::Func { params, ret, .. } => params
            .iter()
            .chain(std::iter::once(&**ret))
            .find_map(|a| first_unresolvable_name(a, names, known)),
        Type::Tuple(ts) => ts
            .iter()
            .find_map(|a| first_unresolvable_name(a, names, known)),
    }
}

/// Does `ty` mention `Self` anywhere, at any nesting depth (`Self`, `List[Self]`, `Option[Self]`)?
/// Implemented as a `subst` round-trip rather than a hand-rolled walk so it covers exactly the arms
/// `subst` covers — a hand-rolled twin would drift the first time a `Ty` variant is added. `Unknown`
/// is a safe probe: substituting it into a type that already contains one leaves the type equal.
fn mentions_self(ty: &Ty) -> bool {
    let probe = HashMap::from([("Self".to_string(), Ty::Unknown)]);
    subst(ty, &probe) != *ty
}

/// **Object safety** — is this signature un-dispatchable through a protocol EXISTENTIAL?
///
/// True when `Self` appears in a non-receiver PARAMETER position. A protocol value erases which
/// concrete type it holds, so two values of one protocol need not be the same witness: with
/// `fn add(self, o: Self) -> Self`, `a + b` over two `Vecish` values would hand a `W` to `V::add`
/// and fault on the first field access. Rust states the same rule as object safety (a `Self`-typed
/// parameter makes a trait non-`dyn`-able); Go bans `Self` from interfaces outright.
///
/// `Self` in the RETURN is fine — it widens to the existential, which is a legal supertype of
/// whatever the witness returns. That is what keeps unary `-` (`neg(self) -> Self`) usable.
fn self_in_param_position(sig: &FnSig) -> bool {
    sig.params.iter().skip(1).any(mentions_self)
}

/// Does a struct method `actual` match a protocol method `proto` (with `Self` bound to `self_ty`)?
fn method_matches(proto: &FnSig, actual: &FnSig, self_ty: &Ty) -> bool {
    if proto.params.len() != actual.params.len() {
        return false;
    }
    // A STATIC-slot protocol requirement (`Convert`'s `convert(x: S) -> Self`, first param NOT `self`)
    // is witnessed ONLY by a matching STATIC method — a value cannot invoke a static ctor, so an
    // instance/`self`-slot method with the same arity (`convert(self) -> Self`) must NOT falsely satisfy
    // it (and vice-versa). Every non-static protocol requirement keeps `is_static == false`, so this is a
    // no-op for every existing instance-method protocol.
    if proto.is_static != actual.is_static {
        return false;
    }
    // M24 Task 5 — a method that takes hidden witness arguments can never WITNESS a protocol
    // requirement. A protocol method has no type parameters of its own (the parser refuses them), so
    // every dispatch through the requirement — an existential value call, `T.static()` through a
    // bound (`Op::CallStaticDyn`) — pushes the declared arity only, and the hidden argument would be
    // missing. Refusing satisfaction here keeps that a type error instead of a runtime arity fault.
    if !actual.witness_params.is_empty() {
        return false;
    }
    let map = HashMap::from([("Self".to_string(), self_ty.clone())]);
    proto
        .params
        .iter()
        .zip(&actual.params)
        .all(|(p, a)| compatible(&subst(p, &map), a))
        && compatible(&subst(&proto.ret, &map), &actual.ret)
}

/// Bind type parameters in `decl` to the corresponding concrete `actual` types (first binding wins;
/// `Unknown` actuals are ignored so an un-inferable argument doesn't pin a param).
/// The `Ty::Param` vector for a list of type parameters — the type-arg slots of a generic type's
/// declared SHAPE (`Struct(key, [Param(T), …])`). Used to build the expected-type-hint unify target.
fn param_shape(tps: &[TypeParam]) -> Vec<Ty> {
    tps.iter().map(|tp| Ty::Param(tp.name.clone())).collect()
}

/// Pre-seed a generic ctor/call's type-param substitution `sub` from an expected-type `hint` (a
/// `let`/return/parameter annotation). `shape` is the ctor/call's declared return type in terms of its
/// OWN params (e.g. `Struct(key, [Param(T)])`, or a generic fn's `sig.ret`). Unify is a no-op on a
/// key/shape mismatch and only binds a param still FREE in `sub`, so this runs AFTER arg-unification
/// to give precedence turbofish > args > annotation — it fills only the params the arguments left
/// unbound (the genuine deadlock, e.g. `Heap([], fn(x, y): x < y)` annotated `Heap[int]`).
fn seed_from_hint(hint: Option<&Ty>, shape: &Ty, sub: &mut HashMap<String, Ty>) {
    if let Some(e) = hint {
        unify(shape, e, sub);
    }
}

/// Return-mask a closure's actual `Func` type for pass-1 unification: replace its return with
/// `Ty::Unknown` so only its PARAMETER positions can bind a method type param in pass 1. Used on the
/// generic-METHOD path (`infer_generic_method`) so an UNANNOTATED closure whose body is a nested free
/// generic call (`xs.map(fn(x): ident(x))`) — whose prepass return leaks the callee's own `Ty::Param`
/// — does NOT prematurely pin the method's return-position `[U]` to that leaked param. The loop-back's
/// checking-mode re-inference then recovers `U` as the CONCRETE return type (`int`). No `Unknown`
/// laundering: `U` is bound concretely by the loop-back, not degraded. A non-closure actual (or a
/// non-`Func`) is returned unchanged, so param/receiver unification is untouched.
fn mask_closure_ret(actual: &Ty) -> Ty {
    match actual {
        Ty::Func { params, labels, .. } => Ty::Func {
            params: params.clone(),
            ret: Box::new(Ty::Unknown),
            labels: labels.clone(),
        },
        other => other.clone(),
    }
}

fn unify(decl: &Ty, actual: &Ty, map: &mut HashMap<String, Ty>) {
    match (decl, actual) {
        (Ty::Param(n), a) => {
            if !a.is_unknown() && !map.contains_key(n) {
                map.insert(n.clone(), a.clone());
            }
        }
        (Ty::List(d), Ty::List(a)) | (Ty::Set(d), Ty::Set(a)) | (Ty::Option(d), Ty::Option(a)) => {
            unify(d, a, map)
        }
        // Native reserved generic handles bind their element param exactly like `List[T]` above —
        // without these arms `unify(Shared[T], Shared[int])` fell to the `_` no-op and bound nothing,
        // so a generic fn over `Shared`/`Channel`/`Atomic`/`RwShared` (or a wrapper struct holding
        // one) rejected the call. (Sibling `ty_collect_params` already lists all four.)
        (Ty::Channel(d), Ty::Channel(a))
        | (Ty::Shared(d), Ty::Shared(a))
        | (Ty::Atomic(d), Ty::Atomic(a))
        | (Ty::RwShared(d), Ty::RwShared(a)) => unify(d, a, map),
        (Ty::Result(dt, de), Ty::Result(at, ae)) => {
            unify(dt, at, map);
            unify(de, ae, map);
        }
        (Ty::Map(dk, dv), Ty::Map(ak, av)) => {
            unify(dk, ak, map);
            unify(dv, av, map);
        }
        (Ty::Struct(dn, da), Ty::Struct(an, aa))
        | (Ty::Enum(dn, da), Ty::Enum(an, aa))
        | (Ty::NewType(dn, da), Ty::NewType(an, aa))
        | (Ty::Protocol(dn, da), Ty::Protocol(an, aa))
            if dn == an && da.len() == aa.len() =>
        {
            da.iter().zip(aa).for_each(|(d, a)| unify(d, a, map));
        }
        (Ty::Tuple(ds), Ty::Tuple(as_)) if ds.len() == as_.len() => {
            ds.iter().zip(as_).for_each(|(d, a)| unify(d, a, map));
        }
        // Labels are surface-only: unify on params + ret only, ignoring labels (`..`).
        (
            Ty::Func {
                params: dp,
                ret: dr,
                ..
            },
            Ty::Func {
                params: ap,
                ret: ar,
                ..
            },
        ) if dp.len() == ap.len() => {
            dp.iter().zip(ap).for_each(|(d, a)| unify(d, a, map));
            unify(dr, ar, map);
        }
        _ => {}
    }
}

/// Collect (into `out`, dedup, in first-seen order) the names from `wanted` that appear as a
/// `Ty::Param` anywhere inside `ty`. Used by the un-inferable-closure-param deadlock diagnostic to
/// find which still-unbound type parameters a closure-typed slot mentions. `wanted: None` = collect
/// EVERY param name, whatever its scope — what [`Checker::may_be_equal`]'s erasure escape needs (it
/// has no candidate list; it must find any free param at all).
fn ty_collect_params(
    ty: &Ty,
    wanted: Option<&std::collections::HashSet<String>>,
    out: &mut Vec<String>,
) {
    match ty {
        Ty::Param(n) => {
            if wanted.is_none_or(|w| w.contains(n)) && !out.contains(n) {
                out.push(n.clone());
            }
        }
        Ty::List(e)
        | Ty::Set(e)
        | Ty::Option(e)
        | Ty::Channel(e)
        | Ty::Shared(e)
        | Ty::Atomic(e)
        | Ty::RwShared(e) => ty_collect_params(e, wanted, out),
        Ty::Map(a, b) | Ty::Result(a, b) => {
            ty_collect_params(a, wanted, out);
            ty_collect_params(b, wanted, out);
        }
        Ty::Tuple(parts)
        | Ty::Struct(_, parts)
        | Ty::Enum(_, parts)
        | Ty::NewType(_, parts)
        | Ty::Protocol(_, parts) => {
            for p in parts {
                ty_collect_params(p, wanted, out);
            }
        }
        Ty::Func { params, ret, .. } => {
            for p in params {
                ty_collect_params(p, wanted, out);
            }
            ty_collect_params(ret, wanted, out);
        }
        _ => {}
    }
}

/// The type a literal match-pattern matches against.
fn lit_pattern_ty(lit: &LitPattern) -> Ty {
    match lit {
        LitPattern::Int(_) => Ty::Int,
        LitPattern::Str(_) => Ty::Str,
        LitPattern::Bool(_) => Ty::Bool,
    }
}

/// Whether a match-arm pattern introduces a binding of `name` (so a free-closure body scan must treat
/// `name` as shadowed inside that arm — see [`Checker::scan_expr_for_pin`]). Covers a tuple/variant
/// sub-position `Ident` binding, an or-pattern alternative (all alternatives bind the same set), and a
/// top-level bare catch-all of the same spelling (parsed as a nullary `Variant`).
fn pattern_binds(p: &Pattern, name: &str) -> bool {
    match p {
        Pattern::Ident(s, _) => s == name,
        // A nullary bare-name pattern (`Variant{ bindings: [] }`) is a catch-all binding when it
        // names neither a qualified variant nor a payload — scope-conservatively, a same-spelling one
        // shadows. A payload variant's bindings are sub-positions; recurse.
        Pattern::Variant {
            name: vn, bindings, ..
        } => (bindings.is_empty() && vn == name) || bindings.iter().any(|b| pattern_binds(b, name)),
        Pattern::Tuple(subs) | Pattern::Or(subs) => subs.iter().any(|s| pattern_binds(s, name)),
        Pattern::Literal(_) | Pattern::Range { .. } | Pattern::Wildcard => false,
    }
}

/// Result type of a numeric binary op: float if either side is float, else int.
fn numeric_result(l: &Ty, r: &Ty) -> Ty {
    if *l == Ty::Float || *r == Ty::Float {
        Ty::Float
    } else {
        Ty::Int
    }
}

fn op_sym(op: BinaryOp) -> &'static str {
    use BinaryOp::*;
    match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Mod => "%",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
        Shl => "<<",
        Shr => ">>",
        In => "in",
        _ => "?",
    }
}

/// Built-in method signatures on `str` (M6). Must mirror the runtime handlers in both backends
/// (`interp::builtins::call_method` and `vm::Vm::do_method_call`).
/// Find a control-flow statement that would escape a `recover:` / `defer:` / `spawn:` block — a
/// `return`, or a `break`/`continue` not contained by a loop *inside* the block. Recurses through
/// nested blocks but stops at nested `fn` declarations (their control flow is their own). `?` is an
/// expression, not a statement, so it is never flagged (a closure body is an expression too, so it
/// cannot hold a `return` at all).
/// Callers that already zero `loop_depth` (defer/spawn, whose own `break outside loop` guard fires)
/// pass `in_loop = true` so only `return` can be reported here — no double diagnostic.
/// It also does NOT descend into a nested `defer:` or `spawn:` block: each has its own guard at its
/// own site, which names the block the statement is LEXICALLY in — descending here would report the
/// same `return` twice, under the outer block's (wrong) noun.
fn escaping_flow(stmts: &[Stmt], in_loop: bool) -> Option<(Span, &'static str)> {
    for s in stmts {
        match &s.kind {
            StmtKind::Return(_) => return Some((s.span, "return")),
            StmtKind::Break if !in_loop => return Some((s.span, "break")),
            StmtKind::Continue if !in_loop => return Some((s.span, "continue")),
            StmtKind::Break | StmtKind::Continue => {}
            StmtKind::If {
                branches,
                else_block,
            } => {
                for (_, body) in branches {
                    if let Some(x) = escaping_flow(body, in_loop) {
                        return Some(x);
                    }
                }
                if let Some(eb) = else_block
                    && let Some(x) = escaping_flow(eb, in_loop)
                {
                    return Some(x);
                }
            }
            // A loop makes its own `break`/`continue` local; a `return` inside still escapes.
            StmtKind::For { body, .. } | StmtKind::While { body, .. } => {
                if let Some(x) = escaping_flow(body, true) {
                    return Some(x);
                }
            }
            StmtKind::Match { arms, .. } => {
                for arm in arms {
                    if let Some(x) = escaping_flow(&arm.body, in_loop) {
                        return Some(x);
                    }
                }
            }
            // A `parallel:` body runs within this function frame (it has no guard of its own), so an
            // escaping `return`/`break`/`continue` inside it must still be detected here. A `for`/loop
            // is not introduced, so `in_loop` is unchanged.
            StmtKind::Parallel { body } => {
                if let Some(x) = escaping_flow(body, in_loop) {
                    return Some(x);
                }
            }
            // A `wait:` arm body / `else` block is an ordinary lexical sub-scope of this block, so a
            // `return` there escapes exactly like a bare one (it was silently discarded at runtime).
            StmtKind::Wait { arms, else_block } => {
                for arm in arms {
                    if let Some(x) = escaping_flow(&arm.body, in_loop) {
                        return Some(x);
                    }
                }
                if let Some(eb) = else_block
                    && let Some(x) = escaping_flow(eb, in_loop)
                {
                    return Some(x);
                }
            }
            StmtKind::Fn(_) => {} // nested function: its control flow is its own
            // `Spawn(Block)` / `Defer(Block)`: NOT descended into — each is guarded at its own site
            // (`check_stmt`), which names the block the statement is lexically in. Descending would
            // double-report the same `return` under this (outer) block's noun.
            _ => {}
        }
    }
    None
}

// Editor hover (Tier C): authored method-NAME lists per built-in type, rendered by `builtin_type_doc`
// as a "methods: a, b, c" line. Each slice is drift-guarded by `builtin_method_slices_all_resolve`
// (every name MUST resolve from its `*_method_sig`), so the hover can only advertise methods that
// provably exist. `list.sort` and `bytes`/`bytearray.extend` live in `infer_method_call` (not the
// `*_method_sig` tables), so they are intentionally absent here — see PROGRESS.md.
const STR_METHODS: &[&str] = &[
    "len",
    "upper",
    "lower",
    "trim",
    "strip",
    "split",
    "split_lines",
    "chars",
    "join",
    "starts_with",
    "ends_with",
    "contains",
    "replace",
    "repeat",
    "reverse",
    "pad_left",
    "index_of",
    "count",
    "strip_prefix",
    "strip_suffix",
    "to_int",
    "to_float",
    "parse_int",
    "parse_float",
    "encode",
];
const LIST_METHODS: &[&str] = &[
    "len",
    "push",
    "pop",
    "reverse",
    "contains",
    "index_of",
    "concat",
    "extend",
    "sort",
    "sum",
    "map",
    "filter",
    "fold",
    "sort_by",
    "sort_by_key",
    "min",
    "max",
    "min_by",
    "max_by",
    "first",
    "last",
    "reversed",
    "insert",
    "remove_at",
    "unique",
    "dedup",
    "chunk",
    "windows",
    "take_while",
    "drop_while",
    "count",
    "position",
];
const MAP_METHODS: &[&str] = &[
    "len", "has", "get", "keys", "values", "remove", "merge", "update",
];
const SET_METHODS: &[&str] = &[
    "len",
    "has",
    "add",
    "remove",
    "union",
    "intersection",
    "difference",
];
const CHANNEL_METHODS: &[&str] = &[
    "send", "try_send", "recv", "try_recv", "close", "trip", "len", "cap",
];
const SHARED_METHODS: &[&str] = &["get", "set", "update"];
const RWSHARED_METHODS: &[&str] = &["get", "set", "read", "write"];
const ATOMIC_METHODS: &[&str] = &["load", "store", "exchange", "cas", "add", "sub"];
// `AtomicInt` exposes the same method surface as `Atomic`.
const ATOMIC_INT_METHODS: &[&str] = ATOMIC_METHODS;
const SOCKET_METHODS: &[&str] = &["read", "write", "read_bytes", "write_bytes", "close"];
const LISTENER_METHODS: &[&str] = &["accept", "addr", "close"];
const WRITER_METHODS: &[&str] = &["write", "write_bytes", "flush", "close"];
const READER_METHODS: &[&str] = &["read_line", "read_bytes", "close"];
const EXECUTOR_METHODS: &[&str] = &["submit", "shutdown", "shutdown_now"];
const BYTES_METHODS: &[&str] = &["decode", "decode_lossy", "len"];
const BYTEARRAY_METHODS: &[&str] = &["len", "push", "pop", "decode"];

// The bespoke `str_method_sig` / `bytes_method_sig` / `bytearray_method_sig` arms are RETIRED (phase
// 5a-containers): every one of their FLAT sigs is now declared as a body-less `native fn` method inside a
// `native struct str` / `bytes` / `bytearray` in `std/prelude.chz`, harvested into that reserved scalar's
// method table (re-seeded by `seed_stdlib_structs`) and looked up via `native_handle_method` with NO type
// args (the scalars are non-generic — the identity path returns the stored sig verbatim). The
// `str(x)`/`bytes(x)`/`bytearray(x)` CTORS stay the `native ctor` decls, and runtime dispatch stays
// Rust-inline (`vm/mod.rs core_method` / `bytes_method`). The `bytearray.extend` special-case (its arg may
// be any of bytes|bytearray|List[int], not a flat `FnSig`) stays an explicit branch in `infer_method_call`
// BEFORE the table lookup.

// The bespoke `list_method_sig` / `map_method_sig` / `set_method_sig` arms are RETIRED (phase
// 5a-containers): every one of their FLAT sigs is now declared as a body-less `native fn` method inside a
// `native struct List[T]` / `Map[K, V]` / `Set[T]` in `std/prelude.chz`, harvested into that reserved
// type's method table (re-seeded by `seed_stdlib_structs`) and looked up via `native_handle_method` with
// the value's element/key/value type substituted for the sig's `Ty::Param`s. The literal syntax + turbofish
// ctor stay compiler-wired (`resolve_type`/`builtin_container_sig`). The generic-recovery `List` methods
// (`map`/`filter`/`fold`/`sort_by`/`sort_by_key`) are now ALSO file-backed as `native fn map[U](...)` etc:
// the generic solver's closure-return LOOP-BACK recovers a return-position `[U]`/`[K]` from an
// (even unannotated) closure body, so the `Ty::List` arm routes their harvested sigs through
// `infer_generic_method`. The `sum` numeric-element gate is the sole surviving residual (a plain
// `sum(self) -> T` would wrongly accept a non-numeric list); `sort` is file-backed via `where T: Comparable`.

// The bespoke `channel_method_sig` arm is RETIRED (phase 5a-containers): every one of its FLAT sigs
// (`send`/`try_send`/`recv`/`try_recv`/`close`/`trip`/`len`/`cap`) is now declared as a body-less
// `native fn` method inside a `native struct Channel[T]` in `std/prelude.chz`, harvested into the
// reserved type's method table (re-seeded by `seed_stdlib_structs`) and looked up via
// `native_handle_method` with the channel's element type substituted for the sig's `Ty::Param("T")`.
// The `Channel[T](cap)` ctor stays compiler-wired and runtime dispatch stays Rust-inline
// (`vm/netio.rs channel_method`); only the checker sig moved to the file-backed mirror.

// The bespoke `shared_method_sig` / `rwshared_method_sig` / `atomic_method_sig` / `executor_method_sig`
// arms are RETIRED (phase 4c-concurrency): every one of their sigs is now declared as a body-less
// `native fn` method inside a `native struct` in `std/concurrency.chz`, harvested into that type's
// method table (re-seeded by `seed_stdlib_structs`) and looked up via `native_handle_method` with the
// box's element type substituted for `Ty::Param("T")`. The two constraints a plain sig cannot express
// stay as thin residuals: `RwShared.read`'s closure return R is recovered in the `Ty::RwShared` arm,
// and `Atomic.add`/`sub`'s numeric-`T` gate is a `!elem.is_numeric()` check in the `Ty::Atomic` arm.

/// A DISPLAY/PLACEHOLDER signature for the STILL-SYNTHETIC free / constructor builtins (editor hover,
/// v1): the GENERIC / reserved-type container & runtime ctors
/// (`range`/`List`/`Map`/`Set`/`Channel`/`Shared`/`RwShared`/`Atomic`/`timer`/`Executor`). (`print` is
/// no longer here — it is now the file-backed variadic `native fn print(...)` decl, so its sig flows
/// through `builtin_sig` BEFORE this fallback.) This is the FLAT sig the container ctors keep here BECAUSE their real generic
/// type-identity (`List[int]` → `Ty::List(Int)`, …) is NOT a flat `FnSig` and lives in
/// `resolve_type`/`infer_named_call`; the `Intrinsic::Ctor` PRELUDE rows carry only their DISPATCH.
/// The eight MIGRATED universe builtins
/// (`ord`/`chr`/`panic`/`int`/`float`/`str`/`bytes`/`bytearray`) are NOT here — their sigs now come
/// from `std/prelude.chz` via [`Checker::builtin_sig`]. Unlike the `infer_named_call` arms — whose
/// result is arg-dependent (overloads, variadics, named params, type-arg-driven element types) — this
/// returns ONE canonical positional shape per name, mirrored by hand from `docs/stdlib.md §1`.
/// Polymorphic-input slots use `Ty::Unknown` (renders `?`); the precise concrete RETURN is the hover
/// payload.
fn builtin_container_sig(name: &str) -> Option<FnSig> {
    // `print`'s signature is now the file-backed variadic `native fn print(...)` decl in
    // `std/prelude.chz` (harvested into `native_prelude_sigs`, read by `builtin_sig` BEFORE this
    // fallback) — the last synthetic Rust sig, retired. Only the generic container/runtime ctors, whose
    // type-arg-driven identity is not a flat `FnSig`, remain here.
    let (params, ret) = match name {
        // Overloads `range(end)` / `range(start, end)` / `range(start, end, step)` collapse to the
        // canonical `range(end)`.
        "range" => (vec![Ty::Int], Ty::list(Ty::Int)),
        // Container constructors --------------------------------------------------------------
        // Element/key/value types are inferred from the argument → `?` slots.
        "List" => (vec![Ty::Unknown], Ty::list(Ty::Unknown)),
        "Set" => (vec![Ty::Unknown], Ty::set(Ty::Unknown)),
        "Map" => (vec![Ty::Unknown], Ty::map(Ty::Unknown, Ty::Unknown)),
        // Runtime constructors ----------------------------------------------------------------
        // `Channel[T]()` is type-arg-driven, no value arg → no params.
        "Channel" => (vec![], Ty::channel(Ty::Unknown)),
        "Shared" => (vec![Ty::Unknown], Ty::shared(Ty::Unknown)),
        "RwShared" => (vec![Ty::Unknown], Ty::rwshared(Ty::Unknown)),
        "Atomic" => (vec![Ty::Unknown], Ty::atomic(Ty::Unknown)),
        "AtomicInt" => (vec![Ty::Int], Ty::AtomicInt),
        // `timer(ms) -> Channel[bool]` (one-shot timeout channel).
        "timer" => (vec![Ty::Int], Ty::channel(Ty::Bool)),
        // `Executor()` — zero-arg work queue.
        "Executor" => (vec![], Ty::Executor),
        _ => return None,
    };
    Some(FnSig::plain(params, ret))
}

/// Editor hover (Tier C): a concise one-line "how to use" blurb for a BUILTIN/STDLIB type or ctor
/// name, paraphrased from `docs/stdlib.md §1–3`. For a type that owns a method table, a
/// `\nmethods: a, b, c` line is appended from the authored `*_METHODS` slice (drift-guarded by
/// `builtin_method_slices_all_resolve`, so every advertised method provably exists). Returns `None`
/// for any non-builtin name (a user type's own docstring already flows via `name_docs`). Built only
/// under the hover probe (the call sites are `hover_probe.is_some()`-gated), never on the hot check
/// path. The method-bearing types are listed first; the no-method types (range/tuple/Result/…) only
/// get a usage line.
fn builtin_type_doc(name: &str) -> Option<String> {
    let (usage, methods): (&str, Option<&[&str]>) = match name {
        // Containers (own a method table) --------------------------------------------------------
        "List" => (
            "growable ordered sequence — List[T](); index with xs[i], iterate with `for x in xs:`",
            Some(LIST_METHODS),
        ),
        "Map" => (
            "key→value hash map — Map[K, V](); index with m[k], iterate with `for k, v in m:`",
            Some(MAP_METHODS),
        ),
        "Set" => (
            "unordered collection of unique elements — Set[T](); operators | & - ^",
            Some(SET_METHODS),
        ),
        "str" => (
            "immutable UTF-8 text; index/slice by codepoint, concatenate with +",
            Some(STR_METHODS),
        ),
        "bytes" => (
            "immutable byte sequence — bytes(x); index with b[i] (byte as int)",
            Some(BYTES_METHODS),
        ),
        "bytearray" => (
            "mutable byte buffer — bytearray(x); index/assign b[i], push/pop bytes",
            Some(BYTEARRAY_METHODS),
        ),
        // Concurrency / runtime types (own a method table) ---------------------------------------
        "Channel" => (
            "FIFO mailbox between tasks — Channel[T](); iterate received values with `for v in ch:`",
            Some(CHANNEL_METHODS),
        ),
        "Shared" => (
            "cross-task shared cell — Shared(v) (import std.concurrency)",
            Some(SHARED_METHODS),
        ),
        "RwShared" => (
            "cross-task read-write cell, many readers OR one writer — RwShared(v) (import std.concurrency)",
            Some(RWSHARED_METHODS),
        ),
        "Atomic" => (
            "cross-task atomic cell — Atomic(v) (import std.concurrency; add/sub need numeric T)",
            Some(ATOMIC_METHODS),
        ),
        "AtomicInt" => (
            "monomorphic lock-free int atomic — AtomicInt(v) (import std.concurrency)",
            Some(ATOMIC_INT_METHODS),
        ),
        "Executor" => (
            "task pool — Executor() (import std.concurrency)",
            Some(EXECUTOR_METHODS),
        ),
        "Socket" => (
            "TCP connection handle from std.net connect()/accept()",
            Some(SOCKET_METHODS),
        ),
        "Listener" => (
            "TCP listening socket from std.net listen()",
            Some(LISTENER_METHODS),
        ),
        "Writer" => (
            "write-only file/stream handle from std.io create()/append()/stdout()/stderr()/buffered()",
            Some(WRITER_METHODS),
        ),
        "Reader" => (
            "read-only file handle from std.io open() — line/chunk streaming of a large file",
            Some(READER_METHODS),
        ),
        // Types without a built-in method table (usage line only) --------------------------------
        "range" => (
            "end-exclusive sequence of ints — range(end) / range(start, end) / range(start, end, step)",
            None,
        ),
        "tuple" => (
            "fixed-size heterogeneous group — (a, b); destructure with `x, y := t`",
            None,
        ),
        "Result" => (
            "success-or-error — Result[T] / Result[T, E]; Ok(v) / Err(e), unwrap with ? or match",
            None,
        ),
        "Option" => (
            "a value or nothing — Option[T]; Some(v) / None, unwrap with ? or match",
            None,
        ),
        "Iterator" => (
            "lazy element cursor — Iterator[T] from a generator or `.iter()`; drive with `for x in it:`",
            None,
        ),
        _ => return None,
    };
    let mut doc = usage.to_string();
    if let Some(ms) = methods {
        doc.push_str("\nmethods: ");
        doc.push_str(&ms.join(", "));
    }
    Some(doc)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod graph_tests {
    //! M4.5 cross-module type-checking tests (`check_graph` over real tempdir fixtures).
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    struct TmpDir(PathBuf);
    impl TmpDir {
        fn new() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::SeqCst);
            let dir = std::env::temp_dir().join(format!("chezzi_chk_{}_{}", std::process::id(), n));
            std::fs::create_dir_all(&dir).unwrap();
            TmpDir(dir)
        }
        fn write(&self, rel: &str, contents: &str) -> PathBuf {
            let p = self.0.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, contents).unwrap();
            p
        }
    }
    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn check_entry(entry: &Path) -> Result<(), Vec<CheckError>> {
        let graph = crate::resolver::build_graph(entry).expect("graph should build");
        check_graph(&graph)
    }

    fn errors(entry: &Path) -> Vec<String> {
        check_entry(entry)
            .unwrap_err()
            .into_iter()
            .map(|e| e.message)
            .collect()
    }

    // 18. A module member's type resolves: a correct use checks clean, a mismatch is rejected.
    #[test]
    fn module_member_type_resolves() {
        let t = TmpDir::new();
        t.write("a.chz", "fn read() -> int: return 5\n");
        let ok = t.write(
            "ok.chz",
            "import a\nx: int = a.read()\nfn main(): print(x)\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );

        let bad = t.write(
            "bad.chz",
            "import a\nx: str = a.read()\nfn main(): print(x)\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter()
                .any(|m| m.contains("cannot assign int to variable of type str")),
            "got: {errs:?}"
        );
    }

    // 19. A type error inside an imported module is reported (with a module label), and the entry's
    // own errors are still collected in the same run.
    #[test]
    fn type_error_across_module_boundary() {
        let t = TmpDir::new();
        // b returns str from an int-typed fn — a type error inside b.
        t.write("b.chz", "fn helper() -> int: return \"nope\"\n");
        let entry = t.write(
            "main.chz",
            // entry has its own error too: assigning int to a str var.
            "import helper from b\nbad: str = 5\nfn main(): print(helper())\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("in module 'b'")),
            "imported module error not labeled: {errs:?}"
        );
        assert!(
            errs.iter()
                .any(|m| m.contains("cannot assign int to variable of type str")),
            "entry error not collected: {errs:?}"
        );
    }

    // 20. A `from` import of a name the module does not export is rejected at check time.
    #[test]
    fn unknown_imported_member_rejected() {
        let t = TmpDir::new();
        t.write("a.chz", "fn f(): print(\"f\")\n");
        let entry = t.write("main.chz", "import nope from a\nfn main(): print(1)\n");
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("has no member 'nope'")),
            "got: {errs:?}"
        );
    }

    // 21. Types are MODULE-SCOPED: the same struct name in two loaded modules does NOT collide
    // (each is private to its module, reachable only via import).
    #[test]
    fn cross_module_same_type_name_no_collision() {
        let t = TmpDir::new();
        t.write("a.chz", "struct Point:\n    x: int\nfn fa(): print(1)\n");
        t.write("b.chz", "struct Point:\n    y: int\nfn fb(): print(2)\n");
        let entry = t.write("main.chz", "import a\nimport b\nfn main(): print(1)\n");
        assert!(
            check_entry(&entry).is_ok(),
            "two modules with the same type name must NOT collide: {:?}",
            errors(&entry)
        );
    }

    // Module-scoped types: `import S from m` (struct) makes the struct usable bare.
    #[test]
    fn from_import_struct_usable() {
        let t = TmpDir::new();
        t.write("types.chz", "struct S:\n    x: int\n");
        let entry = t.write(
            "main.chz",
            "import S from types\ns := S(1)\nfn main(): print(s.x)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "from-imported struct should be usable: {:?}",
            errors(&entry)
        );
    }

    // `import Color from m` (enum) makes the enum + its variants usable.
    #[test]
    fn from_import_enum_usable() {
        let t = TmpDir::new();
        t.write("types.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import Color from types\nc := Color.Red\nfn main(): print(\"ok\")\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "from-imported enum should be usable: {:?}",
            errors(&entry)
        );
    }

    // An imported enum's METHODS must ferry across the module boundary (EnumSigInfo.methods →
    // importer's enum_methods). Both the from-import and qualified-access paths.
    #[test]
    fn imported_enum_methods_usable() {
        let t = TmpDir::new();
        t.write(
            "types.chz",
            "enum Color:\n    Red\n    Green\n    fn cost(self) -> int:\n        match self:\n            Color.Red: return 1\n            Color.Green: return 2\n",
        );
        let entry = t.write(
            "main.chz",
            "import Color from types\nimport types\nfn main():\n    c := Color.Red\n    x: int = c.cost()\n    q: int = types.Color.Green.cost()\n    print(x + q)\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "imported enum methods should resolve cross-module: {:?}",
            errors(&entry)
        );
    }

    // A whole-module-imported newtype is constructible in QUALIFIED form (`m.UserId(10)`), not just
    // usable as a qualified type — mirrors qualified struct/enum-variant construction.
    #[test]
    fn qualified_newtype_construct_usable() {
        let t = TmpDir::new();
        t.write("types.chz", "newtype UserId = int\n");
        let entry = t.write(
            "main.chz",
            "import types\nfn needs(u: types.UserId) -> int:\n    return int(u)\nfn main():\n    u := types.UserId(10)\n    print(needs(u))\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified newtype ctor should resolve cross-module: {:?}",
            errors(&entry)
        );
    }

    // A GENERIC newtype declared in module A is importable in B: constructed (ctor inference) and
    // dispatched with the right instantiation, type-checks clean cross-module.
    #[test]
    fn generic_newtype_cross_module_ok() {
        let t = TmpDir::new();
        t.write(
            "types.chz",
            "newtype Stack[T] = List[T]:\n    fn size(self) -> int:\n        return List(self).len()\n",
        );
        let entry = t.write(
            "main.chz",
            "import Stack from types\nfn main():\n    s: Stack[int] = Stack([1, 2, 3])\n    print(s.size())\n    xs: List[int] = List(s)\n    print(xs[0])\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "generic newtype should resolve cross-module: {:?}",
            errors(&entry)
        );
    }

    // The cross-module generic newtype's type-arg arity is enforced through the import.
    #[test]
    fn generic_newtype_cross_module_arity_rejected() {
        let t = TmpDir::new();
        t.write("types.chz", "newtype Stack[T] = List[T]\n");
        let entry = t.write(
            "main.chz",
            "import types\nfn main():\n    s: types.Stack[int, str] = types.Stack([1])\n    print(1)\nmain()\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|e| e.contains("type argument")),
            "expected an arity error, got: {errs:?}"
        );
    }

    // `import Alias from m` (type alias) makes the alias usable as a type.
    #[test]
    fn from_import_alias_usable() {
        let t = TmpDir::new();
        t.write("types.chz", "type Len = int\n");
        let entry = t.write(
            "main.chz",
            "import Len from types\nx: Len = 5\nfn main(): print(x)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "from-imported alias should be usable: {:?}",
            errors(&entry)
        );
    }

    // Qualified access: `import geo` then `geo.Point(1,2)` and `x: geo.Point`.
    #[test]
    fn qualified_struct_access() {
        let t = TmpDir::new();
        t.write("geo.chz", "struct Point:\n    x: int\n    y: int\n");
        let entry = t.write(
            "main.chz",
            "import geo\np: geo.Point = geo.Point(1, 2)\nfn main(): print(p.x)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified struct access should check: {:?}",
            errors(&entry)
        );
    }

    // Qualified enum variant access: `geo.Color.Red`.
    #[test]
    fn qualified_enum_access() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo\nc := geo.Color.Red\nfn main(): print(\"ok\")\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified enum access should check: {:?}",
            errors(&entry)
        );
    }

    // Bare use of a type whose module was imported (whole-module) but NOT `from`-imported is a
    // CHECK-TIME error with a hint to import it.
    #[test]
    fn bare_unimported_type_is_error() {
        let t = TmpDir::new();
        t.write("geo.chz", "struct Point:\n    x: int\n");
        let entry = t.write(
            "main.chz",
            "import geo\np := Point(1)\nfn main(): print(\"ok\")\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("Point")
                && (m.contains("import it from geo") || m.contains("unknown"))),
            "expected unknown-type/import hint for bare Point: {errs:?}"
        );
    }

    // Cross-module width-alias transparency: `type Len = int32` in module m, imported into entry and
    // used in an extern signature — the FFI width license carries cross-module.
    #[test]
    fn cross_module_width_alias_transparent() {
        let t = TmpDir::new();
        t.write("m.chz", "import int32 from std.ffi\ntype Len = int32\n");
        let entry = t.write(
            "main.chz",
            "import Len from m\nextern \"libc.so.6\":\n    fn strlen(s: str) -> Len\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "cross-module width alias should check: {:?}",
            errors(&entry)
        );
    }

    // 22. A1: `Channel[T].try_recv()` is typed `T?` (Option[T]). A correct `int?` return checks
    // clean; a `str?` return is a type mismatch (pins the element-typed Option result).
    #[test]
    fn channel_try_recv_returns_option_of_elem() {
        let t = TmpDir::new();
        let ok = t.write(
            "ok.chz",
            "fn f() -> int?:\n    ch := Channel[int]()\n    return ch.try_recv()\nfn main(): print(1)\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );

        let bad = t.write(
            "bad.chz",
            "fn f() -> str?:\n    ch := Channel[int]()\n    return ch.try_recv()\nfn main(): print(1)\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter().any(|m| m.contains("str")),
            "expected Option type mismatch: {errs:?}"
        );
    }

    // 23. A1: `try_recv` takes no arguments — a call with one is rejected on arity.
    #[test]
    fn channel_try_recv_arity_rejected() {
        let t = TmpDir::new();
        let bad = t.write(
            "bad.chz",
            "fn f() -> int?:\n    ch := Channel[int]()\n    return ch.try_recv(5)\nfn main(): print(1)\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter()
                .any(|m| m.contains("try_recv") && m.contains("argument")),
            "got: {errs:?}"
        );
    }

    // 24. Qualified enum-variant access `Color.Red` (nullary) type-checks as the enum type, exactly
    // like the bare `Red`. A wrong variant `Color.Bogus` is a targeted error.
    #[test]
    fn qualified_nullary_variant_checks() {
        let t = TmpDir::new();
        let ok = t.write(
            "ok.chz",
            "enum Color:\n    Red\n    Green\nfn f() -> Color:\n    return Color.Red\nfn main(): print(1)\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );

        let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nfn f() -> Color:\n    return Color.Bogus\nfn main(): print(1)\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter()
                .any(|m| m.contains("Color") && m.contains("Bogus")),
            "expected 'no variant Bogus' style error: {errs:?}"
        );
    }

    // 25. Qualified payload-variant construction `Shape.Circle(2)` type-checks as the enum type and
    // still validates argument types, exactly like the bare `Circle(2)`.
    #[test]
    fn qualified_payload_variant_checks() {
        let t = TmpDir::new();
        let ok = t.write(
            "ok.chz",
            "enum Shape:\n    Circle(int)\n    Dot\nfn f() -> Shape:\n    return Shape.Circle(2)\nfn main(): print(1)\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );

        let bad = t.write(
            "bad.chz",
            "enum Shape:\n    Circle(int)\n    Dot\nfn f() -> Shape:\n    return Shape.Circle(\"x\")\nfn main(): print(1)\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter()
                .any(|m| m.contains("Circle") && m.contains("int")),
            "expected payload type mismatch: {errs:?}"
        );
    }

    // 26. A real binding wins over an enum name: `c.0` where `c` is a tuple is normal field access,
    // never reinterpreted as a qualified variant.
    #[test]
    fn binding_wins_over_enum_qualifier() {
        let t = TmpDir::new();
        let ok = t.write(
            "ok.chz",
            "enum Color:\n    Red\nfn main():\n    pair := (1, 2)\n    print(pair.0)\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );
    }

    // 27. Qualified variant patterns `case Color.Red:` check clean (mixed with bare arms); a wrong
    // qualifier `case Color.Circle:` (Circle belongs to a different enum) is a targeted error.
    #[test]
    fn qualified_variant_pattern_checks() {
        let t = TmpDir::new();
        let ok = t.write(
            "ok.chz",
            "enum Color:\n    Red\n    Green\nfn f(c: Color) -> str:\n    return match c:\n        Color.Red: \"r\"\n        Color.Green: \"g\"\nfn main(): print(f(Color.Red))\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "expected clean: {:?}",
            errors(&ok)
        );

        let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nenum Shape:\n    Circle(int)\nfn f(c: Color) -> str:\n    return match c:\n        Color.Circle: \"c\"\n        _: \"x\"\nfn main(): print(f(Color.Red))\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter()
                .any(|m| m.contains("Color") && m.contains("Circle")),
            "expected qualifier mismatch error: {errs:?}"
        );
    }

    // 28. A *module-global* binding named like an enum does NOT shadow qualified access — both engines
    // gate on locals/captures only, so the checker must too (else it validates a different program
    // than the one that runs → runtime fault). `Color.Red` here is the variant (type Color), so
    // returning it as `int` is a *checker* error, not a clean compile + runtime crash.
    #[test]
    fn global_binding_does_not_shadow_qualified_variant() {
        let t = TmpDir::new();
        let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nstruct Box:\n    Red: int\nColor := Box(7)\nfn show() -> int:\n    return Color.Red\nfn main(): print(show())\n",
        );
        let errs = errors(&bad);
        assert!(
            errs.iter().any(|m| m.contains("Color")),
            "expected a checker type error (Color variant returned as int), got: {errs:?}"
        );
    }

    // 29. The `Enum.` qualifier is validated even under an int/str/bool scrutinee: `case Color.Bogus:`
    // against an `int` must be rejected (a qualified variant is never a catch-all binding), not
    // silently accepted as a binding named `Bogus`.
    #[test]
    fn qualified_pattern_validated_under_literal_scrutinee() {
        let t = TmpDir::new();
        let bad = t.write(
            "bad.chz",
            "enum Color:\n    Red\n    Green\nfn f(n: int) -> str:\n    return match n:\n        1: \"one\"\n        Color.Bogus: \"x\"\nfn main(): print(f(1))\n",
        );
        let errs = errors(&bad);
        assert!(
            !errs.is_empty(),
            "expected a checker error for `Color.Bogus` against an int scrutinee, got none"
        );
    }

    // 30. Genuine struct collision (Blocker A): two modules declare `Point` and BOTH are imported and
    // used qualified. The checker must resolve a field on the SECOND module's `Point` against ITS
    // layout (module-scoped runtime key), not first-wins by bare name.
    #[test]
    fn collision_struct_field_resolves_via_qualified_key() {
        let t = TmpDir::new();
        t.write("a.chz", "struct Point:\n    x: int\n");
        t.write("b.chz", "struct Point:\n    y: int\n    z: int\n");
        let entry = t.write(
            "main.chz",
            "import a\nimport b\nfn main():\n    pb := b.Point(2, 3)\n    print(pb.y)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "field `y` on b.Point must resolve via b's layout: {:?}",
            errors(&entry)
        );
    }

    // 31. Genuine struct collision for a METHOD (Blocker B): only bb.Box has `dbl`. Resolving the
    // method on a `bb.Box(..)` value must hit bb's layout, not ba's (which lacks the method).
    #[test]
    fn collision_struct_method_resolves_via_qualified_key() {
        let t = TmpDir::new();
        t.write("ba.chz", "struct Box:\n    v: int\n");
        t.write(
            "bb.chz",
            "struct Box:\n    v: int\n    fn dbl(self) -> int:\n        return self.v * 2\n",
        );
        let entry = t.write(
            "main.chz",
            "import ba\nimport bb\nfn main():\n    bb2 := bb.Box(5)\n    print(bb2.dbl())\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "method `dbl` on bb.Box must resolve via bb's layout: {:?}",
            errors(&entry)
        );
    }

    // 32. Self-consistency under enum collision (Blocker C, checker side): two modules declare `Color`,
    // cb has `fn classify(c: Color)` matching on its own `Color.Red`/`Color.Green`. With both imported
    // the checker must still type-check the bare-`Color` annotation + the qualified match patterns.
    #[test]
    fn collision_enum_annotation_and_match_typecheck() {
        let t = TmpDir::new();
        t.write("ca.chz", "enum Color:\n    Red\n    Green\n");
        t.write(
            "cb.chz",
            "enum Color:\n    Red\n    Green\nfn classify(c: Color) -> int:\n    return match c:\n        Color.Red: 1\n        Color.Green: 2\n",
        );
        let entry = t.write(
            "cmain.chz",
            "import ca\nimport cb\nfn main(): print(cb.classify(cb.Color.Red))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "cb.classify + qualified Color match must type-check under collision: {:?}",
            errors(&entry)
        );
    }

    // 33. The "foreign qualifier" match error must render the scrutinee enum's BARE display name,
    // never the qualified identity key. Two modules declare `Color`; a `match` on one using the
    // other's `Color.<variant>` arms must report `enum 'Color'`, not `enum 'modkey::Color'`.
    #[test]
    fn foreign_variant_match_error_renders_bare_enum_name() {
        let t = TmpDir::new();
        t.write("ea.chz", "enum Color:\n    Red\n    Green\n");
        t.write("eb.chz", "enum Color:\n    Cyan\n    Magenta\n");
        let entry = t.write(
            "emain.chz",
            "import Color from ea\nimport Color as C2 from eb\nfn main():\n    c := Color.Red\n    match c:\n        C2.Cyan: print(\"x\")\n        C2.Magenta: print(\"y\")\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("cannot match a value of enum 'Color'")),
            "expected bare 'Color' in foreign-qualifier match error, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.contains("::Color'")),
            "qualified identity key must not leak into match error: {errs:?}"
        );
    }

    #[test]
    fn module_qualified_variant_pattern_type_checks_clean() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        geo.Color.Red: print(\"r\")\n        geo.Color.Green: print(\"g\")\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "module-qualified variant pattern should type-check clean: {:?}",
            check_entry(&entry)
        );
    }

    #[test]
    fn module_qualified_variant_pattern_aliased_binder_clean() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo as g\nfn main():\n    c := g.Color.Red\n    match c:\n        g.Color.Red: print(\"r\")\n        g.Color.Green: print(\"g\")\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "aliased module-qualified variant pattern should type-check clean: {:?}",
            check_entry(&entry)
        );
    }

    #[test]
    fn module_qualified_variant_pattern_payload_binds_clean() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Shape:\n    Circle(int)\n    Square(int)\n");
        let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    s := geo.Shape.Circle(3)\n    match s:\n        geo.Shape.Circle(r): print(r)\n        geo.Shape.Square(w): print(w)\nmain()\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "payload-binding module-qualified variant pattern should type-check clean: {:?}",
            check_entry(&entry)
        );
    }

    #[test]
    fn module_qualified_variant_pattern_wrong_variant_bare_error() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        geo.Color.Blue: print(\"b\")\n        _: print(\"x\")\nmain()\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("enum 'Color' has no variant 'Blue'")),
            "expected bare-name wrong-variant error, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.contains("::Color")),
            "qualified identity key must not leak: {errs:?}"
        );
    }

    #[test]
    fn module_qualified_variant_pattern_wrong_enum_bare_error() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        geo.Shape.Red: print(\"r\")\n        _: print(\"x\")\nmain()\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("module 'geo' has no enum 'Shape'")),
            "expected bare module/enum error, got: {errs:?}"
        );
    }

    #[test]
    fn module_qualified_variant_pattern_unknown_module_error() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        let entry = t.write(
            "main.chz",
            "import geo\nfn main():\n    c := geo.Color.Red\n    match c:\n        nope.Color.Red: print(\"r\")\n        _: print(\"x\")\nmain()\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|e| e.contains("unknown module 'nope'")),
            "expected unknown-module error, got: {errs:?}"
        );
    }

    #[test]
    fn module_qualified_variant_pattern_cross_enum_bare_error() {
        let t = TmpDir::new();
        t.write("geo.chz", "enum Color:\n    Red\n    Green\n");
        t.write("light.chz", "enum Light:\n    Red\n    Off\n");
        let entry = t.write(
            "main.chz",
            "import geo\nimport light\nfn main():\n    c := geo.Color.Red\n    match c:\n        light.Light.Red: print(\"r\")\n        _: print(\"x\")\nmain()\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("cannot match a value of enum 'Color'")),
            "expected bare cross-enum error, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.contains("::")),
            "qualified identity key must not leak: {errs:?}"
        );
    }

    #[test]
    fn module_qualified_variant_pattern_collision_no_cross_talk() {
        let t = TmpDir::new();
        t.write("a.chz", "enum Color:\n    Red\n    Green\n");
        t.write("b.chz", "enum Color:\n    Cyan\n    Magenta\n");
        // Exhaustive over a's Color via a.Color.* arms => clean.
        let ok = t.write(
            "ok.chz",
            "import a\nimport b\nfn main():\n    c := a.Color.Red\n    match c:\n        a.Color.Red: print(\"r\")\n        a.Color.Green: print(\"g\")\nmain()\n",
        );
        assert!(
            check_entry(&ok).is_ok(),
            "a.Color.* arms over an a.Color value should be exhaustive: {:?}",
            check_entry(&ok)
        );
        // Using b's variant under a's enum => wrong-variant error (no cross-talk).
        let xtalk = t.write(
            "xtalk.chz",
            "import a\nimport b\nfn main():\n    c := a.Color.Red\n    match c:\n        a.Color.Cyan: print(\"c\")\n        _: print(\"x\")\nmain()\n",
        );
        let errs = errors(&xtalk);
        assert!(
            errs.iter()
                .any(|e| e.contains("enum 'Color' has no variant 'Cyan'")),
            "expected no cross-talk with b's Color, got: {errs:?}"
        );
        // Non-exhaustive over a's Color => missing Green (bare name).
        let nonex = t.write(
            "nonex.chz",
            "import a\nimport b\nfn main():\n    c := a.Color.Red\n    match c:\n        a.Color.Red: print(\"r\")\nmain()\n",
        );
        let errs = errors(&nonex);
        assert!(
            errs.iter().any(|e| e.contains("Green")),
            "expected non-exhaustive to report missing bare 'Green', got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|e| e.contains("::")),
            "exhaustiveness error must use bare names: {errs:?}"
        );
    }

    // Soundness: an unannotated closure param (`Ty::Unknown` scrutinee) whose match arms name a
    // single known enum but miss a variant must STILL be rejected — the Skip path used to bypass
    // exhaustiveness, letting `g(E.B)` reach the engine and runtime-trap "no match arm for variant".
    #[test]
    fn match_unknown_param_missing_variant_rejects() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "enum E:\n    A\n    B\ng := fn(x) -> str: match x:\n    E.A: \"a\"\nfn main(): print(g(E.B))\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("non-exhaustive") && e.contains("missing B")),
            "expected non-exhaustive missing B, got: {errs:?}"
        );
    }

    // Soundness: literal arms over an `Ty::Unknown` scrutinee with no `_` wildcard cannot be proven
    // complete — must be rejected (the int-scrutinee value-leak `g(99) -> 99` otherwise).
    #[test]
    fn match_unknown_param_literals_no_wildcard_rejects() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "g := fn(x): match x:\n    1: \"one\"\nfn main(): print(g(99))\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("non-exhaustive") && e.contains("`_`")),
            "expected non-exhaustive add a `_` arm, got: {errs:?}"
        );
    }

    // Boundary (must STAY accepted): unannotated param covering ALL variants of E, no `_`.
    #[test]
    fn match_unknown_param_all_variants_accepts() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "enum E:\n    A\n    B\ng := fn(x) -> str: match x:\n    E.A: \"a\"\n    E.B: \"b\"\nfn main(): print(g(E.A))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean (full coverage): {:?}",
            errors(&entry)
        );
    }

    // Boundary (must STAY accepted): unannotated param over E variants PLUS a `_` wildcard.
    #[test]
    fn match_unknown_param_variants_plus_wildcard_accepts() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "enum E:\n    A\n    B\ng := fn(x) -> str: match x:\n    E.A: \"a\"\n    _: \"other\"\nfn main(): print(g(E.B))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean (wildcard closes): {:?}",
            errors(&entry)
        );
    }

    // Boundary (must STAY accepted): literal arms PLUS a `_` wildcard.
    #[test]
    fn match_unknown_param_literals_plus_wildcard_accepts() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "g := fn(x) -> str: match x:\n    1: \"one\"\n    _: \"other\"\nfn main(): print(g(99))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean (wildcard closes literals): {:?}",
            errors(&entry)
        );
    }

    // MIGRATED (closure-param inference, phase 3): the first literal arm (`1`) now PINS the bare
    // param `x` to `int` (source #2 — a bare-param match scrutinee). The `"b"` arm is then a str
    // literal against an `int` scrutinee → rejected. The pre-inference `OpenScrutinee` behaviour
    // (accept heterogeneous literals when a `_` closes the match) no longer applies: the scrutinee is
    // no longer un-inferable.
    #[test]
    fn match_unknown_param_hetero_literals_plus_wildcard_rejects() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "g := fn(x) -> str: match x:\n    1: \"a\"\n    \"b\": \"c\"\n    _: \"d\"\nfn main(): print(g(1))\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("literal of type str cannot match scrutinee of type int")),
            "expected str-literal-vs-int-scrutinee mismatch, got: {errs:?}"
        );
    }

    // MIGRATED (phase 3): the first arm (`1`) pins `x` to `int`; the `"b"` arm is then a literal
    // mismatch (and the match is also non-exhaustive). Either way it rejects.
    #[test]
    fn match_unknown_param_hetero_literals_no_wildcard_rejects() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "g := fn(x): match x:\n    1: \"a\"\n    \"b\": \"c\"\nfn main(): print(g(true))\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|e| e.contains("literal of type str cannot match scrutinee of type int")),
            "expected str-literal-vs-int-scrutinee mismatch, got: {errs:?}"
        );
    }

    // Boundary (must STAY accepted): a bare-ident catch-all (`n:`) over an un-inferable scrutinee is
    // an irrefutable binding that closes the match — exactly like a concretely-typed int scrutinee.
    // Regression guard: routing the literal case through the permissive `Skip` bind branch wrongly
    // treated `n` as a refutable nullary variant → spurious non-exhaustive + undeclared binding.
    #[test]
    fn match_unknown_param_bare_ident_catchall_accepts() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "g := fn(x) -> str: match x:\n    0: \"zero\"\n    n: \"got {n}\"\nfn main(): print(g(7))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean (bare-ident catch-all closes + binds): {:?}",
            errors(&entry)
        );
    }

    // === Qualified-type-as-static-method-receiver (Part 1) ===

    // `module.Struct.static_method()` — qualified type dotted with a static method checks clean.
    #[test]
    fn qualified_type_struct_static_ok() {
        let t = TmpDir::new();
        t.write(
            "counter.chz",
            "struct Counter:\n    n: int\n    fn zero() -> Counter:\n        return Counter(0)\n",
        );
        let entry = t.write(
            "main.chz",
            "import counter\nfn main():\n    c := counter.Counter.zero()\n    print(c.n)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean qualified static call: {:?}",
            errors(&entry)
        );
    }

    // Negative: a non-existent static method on a qualified type is a clear "no static method" error.
    #[test]
    fn qualified_type_struct_static_unknown_rejects() {
        let t = TmpDir::new();
        t.write(
            "counter.chz",
            "struct Counter:\n    n: int\n    fn zero() -> Counter:\n        return Counter(0)\n",
        );
        let entry = t.write(
            "main.chz",
            "import counter\nfn main():\n    c := counter.Counter.no_such()\n    print(c)\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|m| m.contains("has no static method 'no_such'")),
            "got: {errs:?}"
        );
    }

    // `module.Enum.static_method()` — qualified enum dotted with a static method (NOT a variant).
    #[test]
    fn qualified_type_enum_static_ok() {
        let t = TmpDir::new();
        t.write(
            "col.chz",
            "enum Color:\n    Red\n    Green\n    fn first() -> Color:\n        return Color.Red\n",
        );
        let entry = t.write(
            "main.chz",
            "import col\nfn main():\n    c := col.Color.first()\n    print(c)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "expected clean qualified enum-static call: {:?}",
            errors(&entry)
        );
    }

    // Regression KEEP-WORKING: bare static after `from` import.
    #[test]
    fn qualified_keep_bare_static() {
        let t = TmpDir::new();
        t.write(
            "counter.chz",
            "struct Counter:\n    n: int\n    fn zero() -> Counter:\n        return Counter(0)\n",
        );
        let entry = t.write(
            "main.chz",
            "import Counter from counter\nfn main():\n    c := Counter.zero()\n    print(c.n)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "bare static regressed: {:?}",
            errors(&entry)
        );
    }

    // Regression KEEP-WORKING: qualified constructor `module.Type(args)`.
    #[test]
    fn qualified_keep_qualified_ctor() {
        let t = TmpDir::new();
        t.write("counter.chz", "struct Counter:\n    n: int\n");
        let entry = t.write(
            "main.chz",
            "import counter\nfn main():\n    c := counter.Counter(7)\n    print(c.n)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified ctor regressed: {:?}",
            errors(&entry)
        );
    }

    // Regression KEEP-WORKING: qualified enum VARIANT (variant-first wins over static).
    #[test]
    fn qualified_keep_enum_variant() {
        let t = TmpDir::new();
        t.write(
            "col.chz",
            "enum Color:\n    Red\n    Green\n    fn first() -> Color:\n        return Color.Red\n",
        );
        let entry = t.write(
            "main.chz",
            "import col\nfn main():\n    c := col.Color.Red\n    print(c)\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified enum variant regressed: {:?}",
            errors(&entry)
        );
    }

    // === Two-level path diagnostics (Part 2) ===

    // EXPR position: `std.concurrency.Shared(0)` head `std` is an import-path prefix → two-level hint
    // (NOT the misleading bare "unknown name 'std'").
    #[test]
    fn multilevel_expr_path_two_level_hint() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "import std.concurrency\nfn main():\n    s := std.concurrency.Shared(0)\n    print(s)\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("two-level")),
            "expected two-level hint, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|m| m == "unknown name 'std'"),
            "should not emit bare unknown-name: {errs:?}"
        );
    }

    // EXPR position, SIBLING COLLISION: two `std.*` imports share head `std`. A `std.net.Socket(0)`
    // mistake must name the module the user REFERENCED (`net`), not the first-imported sibling
    // (`concurrency`). Regression for the first-wins head-map bug (3 confirmed adversarial charges).
    #[test]
    fn multilevel_expr_collision_names_referenced_module() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "import std.concurrency\nimport std.net\nfn main():\n    s := std.net.Socket(0)\n    print(s)\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|m| m.contains("two-level") && m.contains("`net.<Name>`")),
            "expected hint naming the referenced module `net`, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|m| m.contains("concurrency")),
            "must NOT steer to the first-imported sibling `concurrency`: {errs:?}"
        );
    }

    // EXPR position, THREE-LEVEL import: `import std.concurrency.collection` binds `collection` (the
    // LAST segment), NOT `concurrency`. A `std.concurrency.collection.X(...)` mistake must name
    // `collection`, not the second segment. Regression for the adversarial 3-level charge.
    #[test]
    fn multilevel_expr_three_level_import_names_bound_name() {
        let t = TmpDir::new();
        let entry = t.write(
            "main.chz",
            "import std.concurrency.collection\nfn main():\n    c := std.concurrency.collection.Nope(0)\n    print(c)\n",
        );
        let errs = errors(&entry);
        assert!(
            errs.iter()
                .any(|m| m.contains("two-level") && m.contains("`collection.<Name>`")),
            "expected hint naming the bound name `collection`, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|m| m.contains("write `concurrency")),
            "must NOT name the second segment `concurrency` as the module: {errs:?}"
        );
    }

    // Negative: a genuine undefined name still gives the normal "unknown name" error.
    #[test]
    fn multilevel_expr_real_typo_still_unknown_name() {
        let t = TmpDir::new();
        let entry = t.write("main.chz", "fn main():\n    nope_xyz()\n");
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("unknown name 'nope_xyz'")),
            "got: {errs:?}"
        );
    }

    // Negative: a real undefined TYPE still gives the normal unknown-type error (not the hint).
    #[test]
    fn multilevel_unknown_type_still_normal_error() {
        let t = TmpDir::new();
        let entry = t.write("main.chz", "fn f(x: Nope): print(1)\nfn main(): f(1)\n");
        let errs = errors(&entry);
        assert!(
            errs.iter().any(|m| m.contains("Nope")),
            "expected unknown-type error mentioning Nope, got: {errs:?}"
        );
        assert!(
            !errs.iter().any(|m| m.contains("two-level")),
            "should not emit two-level hint for a real typo: {errs:?}"
        );
    }

    // ===== expected-type-hint inference: an ANNOTATION pins a generic ctor/fn-call's type params =====
    // (a self-contained Heap clone — fields `data: List[T]`, `less: fn(T,T)->bool` — exercises the
    // module-prefixed-key check_graph path, decoupled from the std.collections path.)
    const HEAP_MOD: &str = "struct H[T]:\n    data: List[T]\n    less: fn(T, T) -> bool\n\n    fn len(self) -> int:\n        return self.data.len()\n";

    #[test]
    fn annotation_pins_generic_ctor_let() {
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let entry = t.write(
            "main.chz",
            "import H from a\nfn main():\n    h: H[int] = H([], fn(x, y): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "annotation should pin H[int] T=int: {:?}",
            errors(&entry)
        );
    }

    #[test]
    fn annotation_pins_generic_ctor_return() {
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let entry = t.write(
            "main.chz",
            "import H from a\nfn mk() -> H[int]:\n    return H([], fn(x, y): x < y)\nfn main():\n    print(mk().len())\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "declared return H[int] should pin T=int: {:?}",
            errors(&entry)
        );
    }

    #[test]
    fn annotation_pins_generic_ctor_call_arg() {
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let entry = t.write(
            "main.chz",
            "import H from a\nfn take(h: H[int]): print(h.len())\nfn main():\n    take(H([], fn(x, y): x < y))\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "param type H[int] should pin T=int: {:?}",
            errors(&entry)
        );
    }

    #[test]
    fn annotation_pins_qualified_generic_ctor() {
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let entry = t.write(
            "main.chz",
            "import a\nfn main():\n    h: a.H[int] = a.H([], fn(x, y): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&entry).is_ok(),
            "qualified ctor annotation should pin T=int: {:?}",
            errors(&entry)
        );
    }

    #[test]
    fn annotation_pins_generic_free_fn_return() {
        let t = TmpDir::new();
        t.write("a.chz", "fn empty[T]() -> List[T]:\n    return []\n");
        let entry = t.write(
            "main.chz",
            "import empty from a\nfn main():\n    xs: List[int] = empty()\n    print(xs.len())\n",
        );
        let errs = errors_or_empty(&entry);
        assert!(
            errs.is_empty(),
            "annotation should pin empty()'s T=int: {errs:?}"
        );
        assert!(
            !errs.iter().any(|m| m.contains("cannot assign List[T] to")),
            "stale leaked-Param error: {errs:?}"
        );
    }

    #[test]
    fn annotation_does_not_override_explicit_or_args() {
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        // (a) turbofish, no annotation — still works.
        let tf = t.write(
            "tf.chz",
            "import H from a\nfn main():\n    h := H[int]([], fn(x: int, y: int): x < y)\n    print(h.len())\n",
        );
        assert!(check_entry(&tf).is_ok(), "turbofish: {:?}", errors(&tf));
        // (b) annotated closure params, no annotation — still works.
        let ann = t.write(
            "ann.chz",
            "import H from a\nfn main():\n    h := H([], fn(x: int, y: int): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&ann).is_ok(),
            "annotated closure params: {:?}",
            errors(&ann)
        );
        // (c) args win over annotation: a concrete int element vs an H[str] annotation is STILL a
        // type error (the seed only fills params left FREE by args; it must not override a pinned T).
        let bad = t.write(
            "bad.chz",
            "import H from a\nfn main():\n    h: H[str] = H([1], fn(x, y): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&bad).is_err(),
            "args-pinned int vs H[str] annotation must still error"
        );
    }

    #[test]
    fn annotation_pins_generic_ctor_in_if_else_branches() {
        // An if-else in value position is itself the bound value; BOTH branches are the tail
        // value, so each must receive the expected-type hint. The hint is a take()-once slot, so
        // without re-installing it per branch the SECOND-inferred branch starves and a generic
        // ctor there deadlocks on `T` — making acceptance depend purely on branch order.
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let e1 = t.write(
            "e1.chz",
            "import H from a\nfn main():\n    r := true\n    h: H[int] = if r: H([], fn(x, y): x > y) else: H([], fn(x, y): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&e1).is_ok(),
            "if-else both branches: {:?}",
            errors(&e1)
        );
        // Swapping the branches must behave identically (no order dependence).
        let e2 = t.write(
            "e2.chz",
            "import H from a\nfn main():\n    r := true\n    h: H[int] = if r: H([], fn(x, y): x < y) else: H([], fn(x, y): x > y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&e2).is_ok(),
            "if-else swapped branches: {:?}",
            errors(&e2)
        );
    }

    #[test]
    fn annotation_pins_generic_ctor_in_match_arms() {
        // Same per-branch hint requirement for an expression-`match` value: every arm body is a
        // tail value and must see the hint, not just the first-inferred arm.
        let t = TmpDir::new();
        t.write("a.chz", HEAP_MOD);
        let e = t.write(
            "m.chz",
            "import H from a\nfn main():\n    k := 1\n    h: H[int] = match k:\n        0: H([], fn(x, y): x > y)\n        _: H([], fn(x, y): x < y)\n    print(h.len())\n",
        );
        assert!(
            check_entry(&e).is_ok(),
            "match arms both pinned: {:?}",
            errors(&e)
        );
    }

    fn errors_or_empty(entry: &Path) -> Vec<String> {
        match check_entry(entry) {
            Ok(()) => Vec::new(),
            Err(es) => es.into_iter().map(|e| e.message).collect(),
        }
    }
}
