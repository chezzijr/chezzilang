//! The checker's internal type lattice (`Ty`) — distinct from the AST's `Type` annotation node.
//!
//! Pragmatic, no unification: `list`/`Result`/`Option` carry exactly one inner type, and
//! [`Ty::Unknown`] is a top/bottom element that is compatible with everything so a single error
//! doesn't cascade into a storm of follow-on errors.

use crate::lexer::Span;
use std::collections::HashMap;
use std::fmt;

/// M24 — the [`WitnessTable::calls`] key. `(graph module index, fragment-context span, fragment
/// ordinal, key span)`, built by one helper (see [`crate::checker::witness_key`]), except the last component is the CALLEE TOKEN's
/// span ([`crate::checker::witness_key_span`]).
///
/// It is deliberately NOT the call node's span: that span is shared by every link of a chained
/// postfix expression, so two distinct witness calls would alias onto one slot.
/// The callee token — the bare `Ident`, or the member-name token — is a distinct source node per
/// link, and the checker's record site and the compiler's lookup both derive it the same way.
pub type WitnessKey = (usize, Span, usize, Span);

/// M24 — how a witness call is SPELLED, which is all the "type parameter … is not determined here"
/// diagnostic needs: it decides which pin the message may suggest, and every suggested spelling has
/// to be one that PARSES. Three forms, because the turbofish does not go in the same place in all
/// three:
/// * [`Self::Free`] — a bare name (`empty()`): `empty[Counter]()`, or an annotated result.
/// * [`Self::Dotted`] — a dotted callee whose PREFIX is spellable at the call site: a static member
///   (`Holder.build()` → `Holder.build[Counter]()`) or a module-qualified fn (`lib.empty()` →
///   `lib.empty[Counter]()`). An annotated result pins these too. The payload is the prefix text.
/// * [`Self::Member`] — an INSTANCE method (`h.make()`), whose receiver is a value expression we
///   cannot re-spell, and which an annotated result never reaches: only `<receiver>.make[Counter]()`.
///
/// Getting this wrong is not cosmetic — the bare `build[SomeType](...)` a static member used to be
/// offered parses as a FREE call and answers "'build' takes no type arguments", so the message sent
/// the reader to a dead end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WitnessCallee {
    Free,
    Dotted(String),
    Member,
}

/// M24 — where ONE witness argument at a call site comes from. One witness slot per (type param,
/// static requirement), so the slot's value names the METHOD as well as the type: the caller picks
/// the `from` overload, because the caller knows the source type.
#[derive(Debug, Clone, PartialEq)]
pub enum WitnessSrc {
    /// The concrete type's runtime IDENTITY KEY (`<module-key>::Name`) — the exact key
    /// `Vm::do_static_call` resolves against `Program::structs` / `Program::enum_methods` — and
    /// the method that witnesses the requirement there (an overload's internal name, `from$int`).
    Concrete { type_key: String, method: String },
    /// FORWARDING (slice 2): the callee's slot is filled by the CALLER's own witness for its
    /// still-abstract type param `param` and the requirement `req` (equal after substitution), so
    /// the argument is a load of the caller's [`witness_tag`] local rather than a constant.
    /// Recorded only when that local is directly reachable at the call site
    /// (`Checker::witness_scope`) — which is exactly what the compiler can lower.
    Forward { param: String, req: String },
}

/// The name of ONE witness slot: its type param and its requirement (`T:from(int)`). The
/// compiler's hidden `$w:` local is named by it. The ONE place the spelling is decided;
/// [`witness_tag_param`] reads it back.
pub fn witness_tag(param: &str, req: &str) -> String {
    format!("{param}:{req}")
}

/// The type param a [`witness_tag`] belongs to (a type param is a bare identifier, never `:`).
pub fn witness_tag_param(tag: &str) -> &str {
    tag.split(':').next().unwrap_or(tag)
}

/// M24 static-witness passing — BOTH halves of the contract, produced by the checker and CONSUMED
/// (never re-derived) by the compiler. The compiler cannot re-derive either half: "does this bound's
/// protocol carry a static requirement" resolves through imports/aliases/embeds, which is type work
/// the backend does not do.
///
/// * `fns` — a generic fn that needs hidden trailing witness parameters, keyed `(graph module index,
///   fn name)`; the value is the witness slots' [`witness_tag`]s (type param + requirement) in
///   declaration order. One hidden trailing param `$w:<tag>` per entry, appended after the declared params, ALWAYS (whether or not the
///   body uses it) so a fn's arity is a property of its declaration alone. Only MODULE-LEVEL free
///   fns are recorded (a nested `fn` never is, so a name collision between the two cannot
///   mis-arity the nested one).
/// * `calls` — what fills each witness slot at one call site, keyed by [`WitnessKey`], parallel to
///   the callee's `fns` entry.
/// * `overload_names` — each `from` overload's internal name (`from$int`, decided by
///   `from_overload::internal_name`), keyed `(graph module index, decl name span)`. The compiler
///   names the overload's proto and its method-table entry with it.
/// * `overload_calls` — the overload's internal name a direct `Type.from(x)` call picks (by the
///   argument's type), keyed by [`WitnessKey`]. A call with no entry names the method as written.
/// * `statics` — the witness slot ([`witness_tag`]) a `T.m(args)` call reads, keyed by
///   [`WitnessKey`] of its member-name token: with two `From` bounds on one `T`, the argument's
///   type picks the requirement.
#[derive(Debug, Clone, Default)]
pub struct WitnessTable {
    pub fns: HashMap<(usize, String), Vec<String>>,
    pub calls: HashMap<WitnessKey, Vec<WitnessSrc>>,
    pub statics: HashMap<WitnessKey, String>,
    pub overload_names: HashMap<(usize, Span), String>,
    pub overload_calls: HashMap<WitnessKey, String>,
}

/// W7-43 — the [`CarrierTable`] key. The same four components as [`WitnessKey`],
/// built by the same rules (see [`crate::checker::carrier_key`]), except the last component is the
/// `?.` carrier's NAME-TOKEN span (`ExprKind::OptChain`'s `name_span`).
///
/// It is deliberately NOT the carrier node's span. `parse_postfix` takes `let span = e.span;` ONCE
/// before the postfix match, so every link of `a?.b?.c` carries the PRIMARY expression's span — and
/// a MIXED chain (`a: Result`, `a.b: Option`) has two links whose modes DIFFER. Keying on the node
/// span would alias them: the later insert wins and the compiler emits the wrong lowering for the
/// other link under a green `chezzi check` — a silent wrong value, not a diagnostic. The name token
/// is a distinct source node per link, and the checker's record site and the compiler's lookup site
/// derive it the same way (one helper, one derivation).
///
/// Injective ACROSS MODULES since **W7-49**, which is what makes `name_span` enough: `desugar`
/// splices a callee's default-parameter expression into the CALLER's AST as a clone that keeps the
/// DEFINING module's spans, while the key is built with the CALLING module's index — so a `?.`
/// inside a default in `lib.chz` and a `?.` at the same `line:col` in `main.chz` used to share one
/// key (measured, in [`WitnessKey`] too, which had shipped with it). The
/// fix is a file identity on [`Span`] itself, so this tuple and every record/lookup site are
/// unchanged. One residual, backstopped loudly rather than silently: the same default spliced twice
/// into the SAME module — see `docs/contributing/gaps.md` W7-49 and `Checker::record_carrier`.
pub type CarrierKey = (usize, Span, usize, Span);

/// W7-43 — which lowering a `?.` carrier takes. The checker decides it from the OPERAND's type; the
/// compiler CONSUMES it and never re-derives it (the backend is type-blind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierMode {
    /// Operand is an `Option[T]` — the `match x: Some(__optN): Some(…); None: None` lowering.
    Option,
    /// Operand is an `Option[T]` and the call returns nothing — the
    /// `match x: Some(__optN): __optN.m(..); None: pass` lowering. The expression has no value.
    OptionVoid,
    /// Operand is a `Result[T, E]` — `?` then `.`, byte-identical to the spaced `x? .f` spelling.
    Try,
    /// `??` operand is a `Result[T, E]` — `match x: Ok(__optN): __optN; Err(_): rhs`, discarding the
    /// error. Rust's `unwrap_or`, not `?`: distinct from [`CarrierMode::Try`], which PROPAGATES.
    ResultCoalesce,
    /// The operand did not type, or is not a carrier at all. The checker already reported it; the
    /// entry exists only so the compiler's "absent = the two halves disagree" fault stays loud.
    Unknown,
}

/// W7-43 — the checker's per-carrier lowering decision, keyed by [`CarrierKey`]. Since TICKET-039
/// a `??` carrier IS recorded too, keyed on `op_span` (the `??` token's own span — never
/// `Expr::span`, which aliases in `(a ?? b) ?? c`).
pub type CarrierTable = HashMap<CarrierKey, CarrierMode>;

/// W8-21 — the bare-`return` success at a declared `Result[nil, E]` sink, keyed exactly like
/// [`CarrierKey`] on the `return` statement's span (a bare `return` has no value node; every valued
/// wrap is a [`Wrap`] keyed by NodeId). The compiler is TYPE-BLIND, so it consumes this verbatim.
///
/// `NoWrap` and a lookup MISS are deliberately IDENTICAL. `WrapOkNil`: the caller emits `Op::Nil`
/// before the wrap, exactly like a written `Ok()` (DEC-017).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RetCoerce {
    NoWrap,
    WrapOkNil,
}

pub type RetCoerceTable = HashMap<CarrierKey, RetCoerce>;

/// TICKET-227 (D3) -- the implicit wrap a value node gets at a typed slot: a plain `T` flowing into
/// a `T?` slot becomes `Some(v)`, into a `T!E` slot `Ok(v)`. Decided ONLY by `Checker::infer`, for
/// the node that owns its expected-type hint (`meet_slot`), and applied ONLY by the compiler's
/// `compile_expr`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Wrap {
    Some,
    Ok,
}

/// Every [`Wrap`] the checker decided, keyed by graph module index and the value's `NodeId`. A miss
/// means "no wrap".
pub type WrapTable = HashMap<(usize, u32), Wrap>;

/// Which `.sum()` call sites sum a `List[float]`, keyed exactly like [`CarrierKey`] (the
/// method-NAME token — see there for why the call node's span aliases across the links of a
/// postfix chain).
///
/// `Some(SumSeed::Float)` = the compiler must push a `0.0` SEED and pass it as `sum`'s one hidden
/// argument, so an EMPTY float list sums to `0.0`: an empty list carries no element to read a kind
/// off, and the backend is TYPE-BLIND. `None` = an ordinary `List[int]` sum, lowered exactly as
/// before.
///
/// BOTH verdicts are recorded, never just the `Some` one: a `None` entry is what lets
/// [`crate::checker::record_call_table_entry`] see an aliased key and turn it into a hard compile
/// error instead of silently applying one site's seed to another. A lookup MISS means "plain numeric
/// sum", which is also the pre-fix lowering — so a missing entry can only ever under-apply the fix,
/// never mis-apply it.
#[derive(Debug, Clone, PartialEq)]
pub enum SumSeed {
    /// A plain `List[float]`: a bare `ConstFloat(0.0)`. The EMPTY case is why this exists --
    /// the backend is type-blind and an empty list carries no element to read a kind off.
    Float,
}
pub type SumSeedTable = HashMap<CarrierKey, Option<SumSeed>>;

/// Surface-only parameter labels on a function type (Swift SE-0111 keyword arguments through a
/// function VALUE). They ride PARALLEL to a `Ty::Func`'s `params`, but participate in NO type
/// identity: two function types differing only in labels are the SAME type (mutually assignable,
/// unifiable, protocol-conforming). Display is the one exception: it marks each parameter at or past
/// `min_or` as omittable (`int = …`), so two types that differ only in optional arity print
/// differently even though they remain the same type everywhere else. This wrapper's `PartialEq` is therefore
/// EQUALITY-NEUTRAL (always `true`), so the derived `PartialEq` on `Ty` transparently ignores labels
/// — no hand-written `Ty` equality, zero regression to HOF/callback/protocol/subtyping code. The
/// labels are consulted ONLY when resolving a value call that carries keyword arguments
/// (`g(name="Bob")`), turning each label into a positional slot — and only through a binding
/// certain to hold one function (TICKET-139/W14-2: `kw_certain`), because labels are not type
/// identity and any other callee may hold a function that names its parameters differently.
#[derive(Debug, Clone, Default)]
pub struct FnLabels {
    /// Surface parameter names, parallel to the function type's `params`.
    pub names: Vec<Option<String>>,
    /// The FEWEST arguments a call through this value may supply — `Some(n)` when the underlying
    /// declaration's trailing parameters carry defaults the CALLEE fills itself
    /// (`crate::base::op::Op::JumpIfProvided`), `None` when nothing is known and every parameter is
    /// required. Lives here rather than as a new `Ty::Func` field so it inherits this wrapper's
    /// equality-neutrality: two function types that differ only in how many arguments may be OMITTED
    /// are still the same type for assignment, unification, protocol conformance and display.
    pub min: Option<usize>,
    /// The underlying declaration's call slots (`FnSig.slots`), when the value was read from one
    /// declaration (TICKET-197). A call through a value certain to hold that one function
    /// (`labels_certain`) binds them, so it fills defaults and packs a variadic like a direct call.
    /// Equality-neutral like `min`: a function type is not changed by how a call through it may be
    /// spelled.
    pub(crate) slots: Option<std::sync::Arc<Vec<crate::desugar::SlotSpec>>>,
    /// The types of the declaration's KEYWORD-ONLY parameters (the ones after a variadic pack), which
    /// `Ty::Func::params` does not carry: `params` ends at the pack slot. Equality-neutral like `min`.
    pub kw: Vec<Ty>,
}

impl PartialEq for FnLabels {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

// `PartialEq` above says every `FnLabels` is equal, so `Eq` (a `PartialEq` that is additionally
// reflexive — trivially true here, there being only one equivalence class) is a sound marker, and
// `Hash` must therefore hash every `FnLabels` identically (hash nothing) to keep the "equal values
// hash equal" contract `Ty`'s derived `Hash` below relies on — see `Ty`'s derive for why this needed
// widening at all (the `EQ_BOUNDS_IN_PROGRESS` cycle-guard index, W7-55).
impl Eq for FnLabels {}

impl std::hash::Hash for FnLabels {
    fn hash<H: std::hash::Hasher>(&self, _state: &mut H) {}
}

impl FnLabels {
    /// A label-less function type of `n` params (a bare `fn(T, …)` annotation, a builtin-fn value, or
    /// any construction site that has no parameter names to offer).
    pub fn none(n: usize) -> FnLabels {
        FnLabels::new(vec![None; n])
    }

    /// Labels with nothing known about optional arity.
    pub fn new(names: Vec<Option<String>>) -> FnLabels {
        FnLabels {
            names,
            min: None,
            slots: None,
            kw: Vec::new(),
        }
    }

    /// Record that a call through this value may supply as few as `min` arguments.
    pub fn with_min(mut self, min: usize) -> FnLabels {
        self.min = Some(min);
        self
    }

    /// Record the declaration's call slots, if it has any.
    pub(crate) fn with_slots(mut self, slots: Option<Vec<crate::desugar::SlotSpec>>) -> FnLabels {
        self.slots = slots.map(std::sync::Arc::new);
        self
    }

    /// The fewest arguments a call may supply, given the value's declared positional parameter
    /// count. The count runs over the whole parameter list, keyword-only parameters included, so a
    /// required keyword-only parameter raises it above `params`.
    pub fn min_or(&self, params: usize) -> usize {
        let all = params + self.kw.len();
        self.min.unwrap_or(all).min(all)
    }

    /// May a value whose keyword-only parameters are `actual` sit in a slot whose keyword-only
    /// parameters are `self`? Each slot parameter must be the value's parameter at the same place,
    /// with the invariance of `params`. The value may take more: a call through the slot never names
    /// them, and the fewest-args check (`min_or`) refuses a value whose extra one is required. So
    /// `print` fits `fn(...Any) -> None`, and `fn k(...xs: int)` does not fit `fn(...int, str) -> str`.
    pub fn kw_fits(&self, actual: &FnLabels) -> bool {
        self.kw.len() <= actual.kw.len()
            && self
                .kw
                .iter()
                .zip(&actual.kw)
                .all(|(a, b)| param_invariant(a, b))
    }
}

// `Eq`/`Hash` added for W7-55: the `EQ_BOUNDS_IN_PROGRESS` cycle guard in `checker::proto` needs an
// O(1) membership index alongside its ordered `Vec<Ty>` (a linear `contains` scan made the walk
// O(cap²), measured to dominate once the depth cap was raised off its old 160). Sound to derive:
// no variant carries a float or other non-total-equality field, `Eq`'s laws (reflexive/symmetric/
// transitive) hold for the derived structural `PartialEq` on every variant, and `FnLabels` (the one
// hand-written `PartialEq`, deliberately equality-neutral) now carries a matching hand-written
// `Eq`/`Hash` — see its impl for why.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
    /// `bytes` — an immutable heap byte sequence (Python `bytes` model). Indexes/iterates to `int`
    /// (0–255), slices to `bytes`. Not a scalar; there is no `byte`/`u8` scalar type.
    Bytes,
    /// `bytearray` — the MUTABLE sibling of `bytes` (Python `bytearray` / Go mutable `[]byte` model).
    /// Constructor-only (`bytearray(...)`, no literal). Indexes/iterates to `int` (0–255), supports
    /// `ba[i] = x` (`IndexSet`), slices to `bytearray`. NOT `Hashable` (mutable ⇒ not a map/set key,
    /// like `list`). A task gets it by handle (like `list`, M3.4).
    ByteArray,
    Nil,
    List(Box<Ty>),
    /// `map[K, V]` — insertion-ordered hash map. `K` is any `Hashable` type (int/str/bool or a
    /// struct implementing `hash(self) -> int`).
    Map(Box<Ty>, Box<Ty>),
    /// `set[T]` — insertion-ordered hash set. `T` is any `Hashable` type (int/str/bool or a struct
    /// implementing `hash(self) -> int`).
    Set(Box<Ty>),
    Func {
        params: Vec<Ty>,
        ret: Box<Ty>,
        /// Surface-only parameter labels (parallel to `params`); equality-neutral (see [`FnLabels`]).
        /// Built with the fn's/closure's param names (or an annotation's optional labels) so a value
        /// call can resolve `g(name="Bob")` to a positional slot. IGNORED by `compatible`/`unify`. Display marks an omittable parameter (`int = …`) using `min_or`.
        labels: FnLabels,
        /// Whether the last param is a variadic pack (`Pack`: its slot is `List[T]`) or a C `...`
        /// follows it (`C`). PART of the type: `compatible`/`unify` compare it.
        variadic: crate::ast::Variadic,
    },
    /// `(T1, T2, …)` — a fixed-arity tuple (always ≥2 elements).
    Tuple(Vec<Ty>),
    /// A struct type, with its generic type arguments (empty for a non-generic struct). E.g.
    /// `Pair[int, str]` is `Struct("Pair", [Int, Str])`; a plain `Point` is `Struct("Point", [])`.
    Struct(String, Vec<Ty>),
    /// An enum type, with its generic type arguments (empty for a non-generic enum). E.g.
    /// `Tree[int]` is `Enum("Tree", [Int])`; a plain `Shape` is `Enum("Shape", [])`.
    Enum(String, Vec<Ty>),
    /// A bound generic type variable (e.g. `T` inside `fn max[T: Comparable]`). Opaque while
    /// checking a generic body; replaced by a concrete `Ty` at each call site via substitution.
    Param(String),
    /// `Result[T, E]` — `T` is the success type, `E` the error type. `T!` / `Result[T]` default
    /// `E` to the `Error` protocol existential (`Protocol("Error")`); `T!E` sets it explicitly.
    Result(Box<Ty>, Box<Ty>),
    Option(Box<Ty>),
    /// `Channel[T]` — a shared mailbox for cross-task messages (C2). A task gets the handle itself,
    /// so reply channels work.
    Channel(Box<Ty>),
    /// `Shared[T]` — the cross-task mutable box (C3): one owner holds the value, writes are
    /// serialised. Every task that holds the handle reaches the same box;
    /// the value isn't copied. Constructed value-first as `Shared(v)` (`T` = `typeof v`).
    Shared(Box<Ty>),
    /// `Atomic[T]` — the cross-task atomic box. Like `Shared[T]` (one box, many tasks; the box holds
    /// the value by handle, M3.3), but it presents atomic-operation methods
    /// (`load`/`store`/`exchange`/`cas`, plus `add`/`sub` on numeric `T`) instead of `Shared`'s
    /// `get`/`set`/`update`. Constructed value-first as `Atomic(v)` (`T` = `typeof v`).
    Atomic(Box<Ty>),
    /// `AtomicInt` — the monomorphic, lock-free int atomic (Rust `AtomicI64` / Java `AtomicInteger` /
    /// Go `atomic.Int64` style). UNLIKE `Atomic[T]` it is NOT generic — statically int, nothing to
    /// widen — so it is always a lock-free `AtomicI64`. One box, many tasks. Methods:
    /// `load`/`store`/`exchange`/`cas` plus `add`/`sub` (always valid — int is always numeric).
    AtomicInt,
    /// `RwShared[T]` — the cross-task read-write box. Like `Shared[T]` (one box, many tasks; the box
    /// holds the value by handle, M3.3), but the lock is a read-write guard:
    /// `read(fn(T) -> R) -> R` acquires a SHARED read guard (many concurrent readers) and `write`/
    /// `set` acquire the EXCLUSIVE write guard. Reach for it over `Shared` when reads dominate.
    /// Constructed value-first as `RwShared(v)` (`T` = `typeof v`).
    RwShared(Box<Ty>),
    /// `Executor` — the C5 escape hatch: an explicitly-owned work queue for detached tasks that
    /// outlive a `parallel:` scope. Non-generic; any task can hold the handle (like `Channel`/`Shared`).
    Executor,
    /// `Socket` — a connected non-blocking TCP stream (D6), produced by `std.net.connect` /
    /// `Listener.accept`. Non-generic; a `spawn`ed fiber can service it.
    Socket,
    /// `Listener` — a non-blocking accepting TCP socket (D6), produced by `std.net.listen`. Non-generic,
    /// like `Socket`.
    Listener,
    /// `Writer` — a write-only file/stream handle (R2), produced by `std.io.create`/`append`/`stdout`/
    /// `stderr`/`buffered`. Non-generic; a `spawn`ed fiber can write to it.
    Writer,
    /// `Reader` — a read-only file handle (R2b), the input twin of `Writer`, produced by
    /// `std.io.open`. Non-generic; a `spawn`ed fiber can read from it.
    Reader,
    /// `ptr` — an opaque C-ABI pointer handle (a raw `void*`). A builtin marshalling primitive (peer
    /// of `int`/`float`/`bool`/`str`) usable in `extern "lib":` signatures. Fully opaque: no methods,
    /// no fields; only `==`/`!=` against another `ptr` (incl. `std.ffi.null()`) and pass/return.
    /// Untyped (one `ptr` for every handle), never auto-freed. Sendable (a plain address).
    Ptr,
    /// A C width (`int8`..`uint64`, `float32`) on a SLOT type (TICKET-218): a binding, param,
    /// return, field, payload or type argument declared with an imported `std.ffi` width. Built only
    /// by `ffi_width_ty`. It is a tag on `int`/`float`, never a distinct type: `compatible` and
    /// `assignable` ignore it, and `infer_kind` erases it, so every VALUE type is the plain scalar
    /// ([`Ty::scalar`]). `Checker::const_meets_slot` reads it to reject a constant outside the width.
    Width(crate::native::cffi::CType),
    /// A protocol used *as a value type* (existential), e.g. the default error type `Error`, or a
    /// PARAMETERIZED protocol `Container[int]`. The `Vec<Ty>` carries the protocol's concrete type
    /// arguments (empty for a bare/non-generic existential like `Error`). A concrete type is
    /// assignable to it iff it satisfies the protocol WITH those args (witnessed statically at every
    /// store/pass boundary); the protocol's own methods AND everything its embeds require are
    /// callable on it (M22 — the embed set flattens at every use site), EXCEPT one taking `Self`,
    /// which is bound-only by object safety (`self_in_param_position`) — with the carried
    /// args substituted into the method's params/return so `c.get(0)` on a `Container[int]` yields
    /// `int`, not the bare param `T`. Type-erased at runtime (methods dispatch by name; the args are
    /// a checker-only witness, never constructed in the vm/compiler). STRICT INVARIANCE: bare
    /// `Container` (0 args) and `Container[int]` (1 arg) are distinct, non-interchangeable types.
    Protocol(String, Vec<Ty>),
    /// An imported module, identified by the name it's bound under in the current module. Member
    /// access (`io.read()`) resolves against the module's exported signatures.
    Module(String),
    /// Un-inferable, or "an error was already reported here". Compatible with everything.
    Unknown,
    /// TICKET-225 (R5) — a type variable: pending until another operand of its frame binds it
    /// (`checker::tyvar`; a frame is one statement or one bound operand, TICKET-238). Only `assignable` and `Checker::join_ty` can bind one; the pure
    /// [`compatible`] declines on it (an unbound var equals only itself). Printed as a token `Checker::resolve_var_tokens` replaces.
    Var(u32),
}

/// The private-use brackets of a pending type variable in rendered text (TICKET-238): a
/// `Ty::Var` never reaches a message as `_`.
pub(crate) const VAR_OPEN: char = '\u{E000}';
pub(crate) const VAR_CLOSE: char = '\u{E001}';

impl Ty {
    pub fn list(inner: Ty) -> Ty {
        Ty::List(Box::new(inner))
    }
    pub fn map(key: Ty, value: Ty) -> Ty {
        Ty::Map(Box::new(key), Box::new(value))
    }
    pub fn set(elem: Ty) -> Ty {
        Ty::Set(Box::new(elem))
    }
    /// `Result[T]` / `T!` — error type defaults to the `Error` protocol.
    pub fn result(inner: Ty) -> Ty {
        Ty::Result(Box::new(inner), Box::new(Ty::error_proto()))
    }
    /// `Result[T, E]` / `T!E` — explicit error type.
    pub fn result_e(inner: Ty, err: Ty) -> Ty {
        Ty::Result(Box::new(inner), Box::new(err))
    }
    /// The default error type: the `Error` protocol as an existential.
    pub fn error_proto() -> Ty {
        Ty::Protocol("Error".to_string(), Vec::new())
    }
    pub fn option(inner: Ty) -> Ty {
        Ty::Option(Box::new(inner))
    }
    /// The type of the enum registered under `key`, applied to `args`. The prelude's `Option` /
    /// `Result` enums are spelled by their carrier types; every other key is a `Ty::Enum`. The one
    /// map from an enum key to its type — [`Ty::as_enum`] is its inverse.
    pub(crate) fn enum_ty(key: String, mut args: Vec<Ty>) -> Ty {
        match (key.as_str(), args.len()) {
            ("Option", 1) => Ty::option(args.remove(0)),
            ("Result", 2) => {
                let e = args.remove(1);
                Ty::result_e(args.remove(0), e)
            }
            _ => Ty::Enum(key, args),
        }
    }
    /// The enum key and type arguments of an enum type, carrier or user; `None` for any other type.
    pub(crate) fn as_enum(&self) -> Option<(&str, Vec<Ty>)> {
        match self {
            Ty::Option(t) => Some(("Option", vec![(**t).clone()])),
            Ty::Result(t, e) => Some(("Result", vec![(**t).clone(), (**e).clone()])),
            Ty::Enum(k, a) => Some((k.as_str(), a.clone())),
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::List(_)
            | Ty::Map(..)
            | Ty::Set(_)
            | Ty::Func { .. }
            | Ty::Tuple(_)
            | Ty::Struct(..)
            | Ty::Param(_)
            | Ty::Channel(_)
            | Ty::Shared(_)
            | Ty::Atomic(_)
            | Ty::AtomicInt
            | Ty::RwShared(_)
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader
            | Ty::Ptr
            | Ty::Width(_)
            | Ty::Protocol(..)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => None,
        }
    }
    pub fn channel(inner: Ty) -> Ty {
        Ty::Channel(Box::new(inner))
    }
    pub fn shared(inner: Ty) -> Ty {
        Ty::Shared(Box::new(inner))
    }
    pub fn atomic(inner: Ty) -> Ty {
        Ty::Atomic(Box::new(inner))
    }
    pub fn rwshared(inner: Ty) -> Ty {
        Ty::RwShared(Box::new(inner))
    }
    /// A non-generic struct type (no type arguments) — the common case.
    pub fn strukt(name: impl Into<String>) -> Ty {
        Ty::Struct(name.into(), Vec::new())
    }

    /// The one carrier destructure: the wrap that builds this carrier from a plain value, its
    /// payload, and its error type (`T!E` only). `None` for a type that is not a carrier.
    pub fn carrier_parts(&self) -> Option<(crate::checker::Wrap, &Ty, Option<&Ty>)> {
        match self {
            Ty::Option(p) => Some((crate::checker::Wrap::Some, p, None)),
            Ty::Result(p, e) => Some((crate::checker::Wrap::Ok, p, Some(e))),
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::List(_)
            | Ty::Map(..)
            | Ty::Set(_)
            | Ty::Func { .. }
            | Ty::Tuple(_)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Param(_)
            | Ty::Channel(_)
            | Ty::Shared(_)
            | Ty::Atomic(_)
            | Ty::AtomicInt
            | Ty::RwShared(_)
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader
            | Ty::Ptr
            | Ty::Width(_)
            | Ty::Protocol(..)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => None,
        }
    }

    /// The payload slot a plain value fills when it wraps into this carrier (`T` of `T?` / `T!E`).
    pub fn carrier_payload(&self) -> Option<&Ty> {
        self.carrier_parts().map(|c| c.1)
    }

    /// The slot a literal reads its expected type from: this type with ONE carrier layer looked
    /// through (`meet_slot` wraps one layer, so a deeper strip accepts nothing more).
    pub fn slot_payload(&self) -> &Ty {
        self.carrier_payload().unwrap_or(self)
    }

    /// The C width a slot type carries (`Ty::Width`), if any.
    pub fn width(&self) -> Option<&crate::native::cffi::CType> {
        match self {
            Ty::Width(c) => Some(c),
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::List(_)
            | Ty::Map(..)
            | Ty::Set(_)
            | Ty::Func { .. }
            | Ty::Tuple(_)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Param(_)
            | Ty::Result(..)
            | Ty::Option(_)
            | Ty::Channel(_)
            | Ty::Shared(_)
            | Ty::Atomic(_)
            | Ty::AtomicInt
            | Ty::RwShared(_)
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader
            | Ty::Ptr
            | Ty::Protocol(..)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => None,
        }
    }

    /// The scalar a width slot holds (`float32` gives `float`, every other width `int`); every
    /// other type gives itself.
    pub fn scalar(&self) -> &Ty {
        static INT: Ty = Ty::Int;
        static FLOAT: Ty = Ty::Float;
        match self {
            Ty::Width(crate::native::cffi::CType::Float32) => &FLOAT,
            Ty::Width(_) => &INT,
            other @ Ty::Int
            | other @ Ty::Float
            | other @ Ty::Bool
            | other @ Ty::Str
            | other @ Ty::Bytes
            | other @ Ty::ByteArray
            | other @ Ty::Nil
            | other @ Ty::List(_)
            | other @ Ty::Map(..)
            | other @ Ty::Set(_)
            | other @ Ty::Func { .. }
            | other @ Ty::Tuple(_)
            | other @ Ty::Struct(..)
            | other @ Ty::Enum(..)
            | other @ Ty::Param(_)
            | other @ Ty::Result(..)
            | other @ Ty::Option(_)
            | other @ Ty::Channel(_)
            | other @ Ty::Shared(_)
            | other @ Ty::Atomic(_)
            | other @ Ty::AtomicInt
            | other @ Ty::RwShared(_)
            | other @ Ty::Executor
            | other @ Ty::Socket
            | other @ Ty::Listener
            | other @ Ty::Writer
            | other @ Ty::Reader
            | other @ Ty::Ptr
            | other @ Ty::Protocol(..)
            | other @ Ty::Module(_)
            | other @ Ty::Unknown
            | other @ Ty::Var(_) => other,
        }
    }

    /// Is this a number (`int` or `float`)?
    pub fn is_numeric(&self) -> bool {
        matches!(self.scalar(), Ty::Int | Ty::Float)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Ty::Unknown)
    }

    /// TICKET-238 -- does this type have a hole: a `Ty::Unknown` BELOW the top level. A bare
    /// top-level `Unknown` is the error sentinel, not a hole. A `Ty::Var` is pending, not a hole:
    /// `close_tyvar_frame` owns its verdict. The one predicate `Checker::closed_binding_ty` reads.
    pub fn has_hole(&self) -> bool {
        fn open(t: &Ty) -> bool {
            match t {
                Ty::Unknown => true,
                Ty::List(x)
                | Ty::Option(x)
                | Ty::Set(x)
                | Ty::Channel(x)
                | Ty::Shared(x)
                | Ty::RwShared(x)
                | Ty::Atomic(x) => open(x),
                Ty::Map(k, v) | Ty::Result(k, v) => open(k) || open(v),
                Ty::Struct(_, a) | Ty::Enum(_, a) | Ty::Protocol(_, a) => a.iter().any(open),
                Ty::Tuple(ts) => ts.iter().any(open),
                // A return of `Unknown` is bottom, a fn that never returns (`panic`), not a hole:
                // no annotation spells it, and the value `p := panic` is legal.
                Ty::Func { params, ret, .. } => {
                    params.iter().any(open) || (!ret.is_unknown() && open(ret))
                }
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
                | Ty::Var(_) => false,
            }
        }
        !self.is_unknown() && open(self)
    }

    /// The parameter and return types of a function value, and whether it is variadic. A structural
    /// question (generic inference, arity) asks this instead of matching the variant.
    pub fn fn_parts(&self) -> Option<(&[Ty], &Ty, crate::ast::Variadic)> {
        match self {
            Ty::Func {
                params,
                ret,
                variadic,
                ..
            } => Some((params, ret, *variadic)),
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::List(_)
            | Ty::Map(..)
            | Ty::Set(_)
            | Ty::Tuple(_)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Param(_)
            | Ty::Result(..)
            | Ty::Option(_)
            | Ty::Channel(_)
            | Ty::Shared(_)
            | Ty::Atomic(_)
            | Ty::AtomicInt
            | Ty::RwShared(_)
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader
            | Ty::Ptr
            | Ty::Width(_)
            | Ty::Protocol(..)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => None,
        }
    }
}

/// Structural compatibility for assignment / argument passing. [`Ty::Unknown`] on either side
/// (at any depth) matches anything, which is what keeps one error from cascading.
/// A trailing clarification for a function-type mismatch whose two sides DISPLAY identically —
/// which is what an optional-arity mismatch always looks like, since parameter counts match and the
/// optional arity is not part of `Display`. Empty for every other mismatch.
pub fn fn_arity_note(expected: &Ty, actual: &Ty) -> String {
    if let (
        Ty::Func {
            params: p1,
            labels: l1,
            variadic: v1,
            ..
        },
        Ty::Func {
            params: p2,
            labels: l2,
            variadic: v2,
            ..
        },
    ) = (expected, actual)
        && p1.len() == p2.len()
        && v1 == v2
    {
        let (e, a) = (l1.min_or(p1.len()), l2.min_or(p2.len()));
        if a > e {
            return format!(
                " — the value requires {a} argument(s) but the target may be called with as few as {e} (its trailing parameters have defaults, and this one's do not)"
            );
        }
    }
    String::new()
}

/// A function type's parameters are compared with STRICT INVARIANCE (the Go/Rust rule), never
/// covariance or contravariance. Covariance was the TICKET-093 unsoundness: it accepted
/// `h: fn(Any) -> Dog = idd` over `fn idd(d: Dog) -> Dog`, so a `Cat` value reached a `Dog`-typed
/// slot at run time. Contravariance is refused too — it would accept `h: fn(int) -> str = wide`
/// over `fn wide(a: Any) -> str`, which this language rejects. The call is two-way because
/// `compatible` is symmetric except the `Error`/`Str` intrinsic grant above and a nested function
/// type's own optional-arity disjunct, and both must cancel for the parameters to be truly invariant.
pub fn param_invariant(a: &Ty, b: &Ty) -> bool {
    compatible(a, b) && compatible(b, a)
}

pub fn compatible(expected: &Ty, actual: &Ty) -> bool {
    use Ty::*;
    match (expected, actual) {
        (Unknown, _) | (_, Unknown) => true,
        // A pure caller has no var store, so it declines on a var other than the same one.
        (Var(a), Var(b)) => a == b,
        // A C width is a tag on its scalar (TICKET-218): compatibility ignores it, nested too.
        (Width(_), _) => compatible(expected.scalar(), actual),
        (_, Width(_)) => compatible(expected, actual.scalar()),
        (Int, Int)
        | (Float, Float)
        | (Bool, Bool)
        | (Str, Str)
        | (Bytes, Bytes)
        | (ByteArray, ByteArray)
        | (Nil, Nil) => true,
        (List(a), List(b))
        | (Option(a), Option(b))
        | (Channel(a), Channel(b))
        | (Shared(a), Shared(b))
        | (RwShared(a), RwShared(b))
        | (Atomic(a), Atomic(b)) => compatible(a, b),
        (Result(at, ae), Result(bt, be)) => compatible(at, bt) && compatible(ae, be),
        // A protocol existential: identity matches; `str` conforms to `Error` intrinsically.
        // Struct conformance needs the registry — handled by `Checker::assignable`, not here.
        // STRICT INVARIANCE: same protocol name AND same arg arity AND arg-wise compatible (mirrors
        // the `Struct`/`Enum` arms), so bare `Container` (0 args) and `Container[int]`
        // (1 arg) are distinct, and `Container[str]` ≠ `Container[int]`.
        (Protocol(a, aa), Protocol(b, ba)) => {
            a == b && aa.len() == ba.len() && aa.iter().zip(ba).all(|(x, y)| compatible(x, y))
        }
        (Protocol(p, pa), Str) if p == "Error" && pa.is_empty() => true,
        (Map(ka, va), Map(kb, vb)) => compatible(ka, kb) && compatible(va, vb),
        (Set(a), Set(b)) => compatible(a, b),
        (Struct(a, aa), Struct(b, ba)) | (Enum(a, aa), Enum(b, ba)) => {
            a == b && aa.len() == ba.len() && aa.iter().zip(ba).all(|(x, y)| compatible(x, y))
        }
        (AtomicInt, AtomicInt)
        | (Executor, Executor)
        | (Socket, Socket)
        | (Listener, Listener)
        | (Writer, Writer)
        | (Reader, Reader)
        | (Ptr, Ptr) => true,
        (Module(a), Module(b)) | (Param(a), Param(b)) => a == b,
        // Labels are surface-only: two function types differing only in parameter labels are the SAME
        // type — `compatible` matches on arity + param/ret compatibility and IGNORES the names.
        //
        // The OPTIONAL ARITY on those same labels is NOT surface-only, and is the one part of a
        // function type that is DIRECTIONAL. `expected` describes how the slot will be CALLED: an
        // `expected` admitting 0 arguments may be called with 0, so the `actual` stored into it must
        // accept 0 too. A value that requires MORE arguments than the slot promises is unsound —
        // `h := a; h = b` over `fn a(x: int = 1)` / `fn b(x: int)` type-checked clean and then faulted
        // with `function 'b' expects 1 argument(s), got 0`. The reverse is fine and
        // must stay accepted: a defaulted fn flows into a plain `fn(int) -> int` annotation, because
        // accepting fewer required arguments is strictly more permissive.
        (
            Func {
                params: p1,
                ret: r1,
                labels: l1,
                variadic: v1,
            },
            Func {
                params: p2,
                ret: r2,
                labels: l2,
                variadic: v2,
            },
        ) => {
            p1.len() == p2.len()
                && v1 == v2
                && l1.kw_fits(l2)
                && l2.min_or(p2.len()) <= l1.min_or(p1.len())
                && p1.iter().zip(p2).all(|(a, b)| param_invariant(a, b))
                && compatible(r1, r2)
        }
        (Tuple(a), Tuple(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| compatible(x, y))
        }
        _ => false,
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.fmt_named(f, None)
    }
}

/// A nested type rendered through [`Ty::fmt_named`] with the caller's name map.
struct Named<'a>(&'a Ty, Option<&'a HashMap<String, String>>);

impl fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt_named(f, self.1)
    }
}

/// Does a `Result`'s error type print after the `!` (anything but the default `Error` protocol or
/// a still-unconstrained error)?
fn has_explicit_error(e: &Ty) -> bool {
    !matches!(e, Ty::Unknown) && !matches!(e, Ty::Protocol(p, pa) if p == "Error" && pa.is_empty())
}

/// The operand of a `?` / `!` suffix. Parenthesized exactly where `parse_type_postfix` would
/// read the bare spelling differently: a fn type (`fn() -> int?` returns `int?`) and a `Result`
/// with an explicit error (`int!str?` is `Result[int, Option[str]]`).
struct Operand<'a>(&'a Ty, Option<&'a HashMap<String, String>>);

impl fmt::Display for Operand<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parens = match self.0 {
            Ty::Func { .. } => true,
            Ty::Result(_, e) => has_explicit_error(e),
            Ty::Int
            | Ty::Float
            | Ty::Bool
            | Ty::Str
            | Ty::Bytes
            | Ty::ByteArray
            | Ty::Nil
            | Ty::List(_)
            | Ty::Map(..)
            | Ty::Set(_)
            | Ty::Tuple(_)
            | Ty::Struct(..)
            | Ty::Enum(..)
            | Ty::Param(_)
            | Ty::Option(_)
            | Ty::Channel(_)
            | Ty::Shared(_)
            | Ty::Atomic(_)
            | Ty::AtomicInt
            | Ty::RwShared(_)
            | Ty::Executor
            | Ty::Socket
            | Ty::Listener
            | Ty::Writer
            | Ty::Reader
            | Ty::Ptr
            | Ty::Width(_)
            | Ty::Protocol(..)
            | Ty::Module(_)
            | Ty::Unknown
            | Ty::Var(_) => false,
        };
        if parens {
            write!(f, "({})", Named(self.0, self.1))
        } else {
            write!(f, "{}", Named(self.0, self.1))
        }
    }
}

/// Strip a trailing `#<digits>` from a module key: the `module_keys` duplicate-label tiebreak
/// (`src/resolver/mod.rs`). `#` is unspellable in source, so it never reaches a message.
fn strip_dup_label(module: &str) -> &str {
    match module.rsplit_once('#') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|b| b.is_ascii_digit()) => head,
        _ => module,
    }
}

impl Ty {
    /// Render each type for ONE diagnostic message. A nominal type renders bare (as `Display` does)
    /// unless another DISTINCT nominal type in the same message shares its bare name, in which case
    /// both render module-qualified (`a.Col`, `b.Col`; the full dotted module path when the last
    /// segments also collide). A group that stays ambiguous after that (a `#<idx>` duplicate-label
    /// key, whose stripped module is equal) renders bare rather than print two equal names.
    /// Stateless: `Display for Ty` is unchanged, because checker code keys on its output.
    pub(crate) fn render_distinct<const N: usize>(tys: [&Ty; N]) -> [String; N] {
        let mut keys = std::collections::BTreeSet::new();
        for t in tys {
            t.collect_nominal_keys(&mut keys);
        }
        let mut groups: std::collections::BTreeMap<String, Vec<&str>> =
            std::collections::BTreeMap::new();
        for k in &keys {
            groups
                .entry(crate::base::words::bare_display(k))
                .or_default()
                .push(k);
        }
        let mut names: HashMap<String, String> = HashMap::new();
        for (bare, group) in groups.iter().filter(|(_, g)| g.len() >= 2) {
            // (key, module) for the keys that carry a module; the rest stay bare.
            let split: Vec<(&str, &str)> = group
                .iter()
                .filter_map(|k| k.rsplit_once("::").map(|(m, _)| (*k, strip_dup_label(m))))
                .collect();
            let short = |m: &str| m.rsplit('.').next().unwrap_or(m).to_string();
            let shorts: std::collections::BTreeSet<String> =
                split.iter().map(|(_, m)| short(m)).collect();
            let distinct_shorts = shorts.len() == split.len();
            let mut rendered: Vec<(&str, String)> = group
                .iter()
                .filter(|k| !split.iter().any(|(sk, _)| sk == *k))
                .map(|k| (*k, bare.clone()))
                .collect();
            for (k, m) in &split {
                let q = if distinct_shorts {
                    short(m)
                } else {
                    m.to_string()
                };
                rendered.push((k, format!("{q}.{bare}")));
            }
            let unique: std::collections::BTreeSet<&String> =
                rendered.iter().map(|(_, r)| r).collect();
            if unique.len() != rendered.len() {
                continue; // still ambiguous: decline, both stay bare
            }
            for (k, r) in rendered {
                names.insert(k.to_string(), r);
            }
        }
        tys.map(|t| {
            if names.is_empty() {
                t.to_string()
            } else {
                Named(t, Some(&names)).to_string()
            }
        })
    }

    /// Render `self` with each nominal type spelled by its identity KEY, not its bare name: two
    /// distinct types never render alike (`a::P`, `b::P`). For an internal name, never a message.
    pub(crate) fn render_keyed(&self) -> String {
        let mut keys = std::collections::BTreeSet::new();
        self.collect_nominal_keys(&mut keys);
        let names: HashMap<String, String> = keys.into_iter().map(|k| (k.clone(), k)).collect();
        Named(self, Some(&names)).to_string()
    }

    /// Every nominal (struct/enum/protocol) identity key inside `self`, at any depth, plus the
    /// display name of each builtin or type-parameter leaf.
    fn collect_nominal_keys(&self, out: &mut std::collections::BTreeSet<String>) {
        match self {
            // A builtin container prints its bare head name (`List[int]`), so the head joins the
            // group of a user type that shadows it, as a leaf does below.
            Ty::List(t)
            | Ty::Set(t)
            | Ty::Channel(t)
            | Ty::Shared(t)
            | Ty::RwShared(t)
            | Ty::Atomic(t) => {
                let shown = self.to_string();
                out.insert(shown[..shown.find('[').unwrap_or(shown.len())].to_string());
                t.collect_nominal_keys(out);
            }
            Ty::Map(a, b) => {
                out.insert("Map".to_string());
                a.collect_nominal_keys(out);
                b.collect_nominal_keys(out);
            }
            Ty::Option(t) => t.collect_nominal_keys(out),
            Ty::Result(a, b) => {
                a.collect_nominal_keys(out);
                b.collect_nominal_keys(out);
            }
            Ty::Tuple(ts) => ts.iter().for_each(|t| t.collect_nominal_keys(out)),
            Ty::Func { params, ret, .. } => {
                params.iter().for_each(|t| t.collect_nominal_keys(out));
                ret.collect_nominal_keys(out);
            }
            Ty::Struct(n, args) | Ty::Enum(n, args) | Ty::Protocol(n, args) => {
                out.insert(n.clone());
                args.iter().for_each(|t| t.collect_nominal_keys(out));
            }
            // A builtin or a type parameter prints its bare name, so it joins the group of a
            // user type that shadows that name: `expected int, found m.int`.
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
            | Ty::Width(_) => {
                out.insert(self.to_string());
            }
            Ty::Module(_) | Ty::Unknown | Ty::Var(_) => {}
        }
    }

    /// `Name` or `Name[A, B]` for a nominal type: the caller's qualified name for identity key `n`
    /// when `names` has one, else the bare display name.
    fn fmt_nominal(
        f: &mut fmt::Formatter<'_>,
        n: &str,
        args: &[Ty],
        names: Option<&HashMap<String, String>>,
    ) -> fmt::Result {
        match names.and_then(|m| m.get(n)) {
            Some(q) => write!(f, "{q}")?,
            None => write!(f, "{}", crate::base::words::bare_display(n))?,
        }
        if !args.is_empty() {
            write!(f, "[")?;
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{}", Named(a, names))?;
            }
            write!(f, "]")?;
        }
        Ok(())
    }

    fn fmt_named(
        &self,
        f: &mut fmt::Formatter<'_>,
        names: Option<&HashMap<String, String>>,
    ) -> fmt::Result {
        match self {
            Ty::Int => write!(f, "int"),
            Ty::Float => write!(f, "float"),
            Ty::Bool => write!(f, "bool"),
            Ty::Str => write!(f, "str"),
            Ty::Bytes => write!(f, "bytes"),
            Ty::ByteArray => write!(f, "bytearray"),
            Ty::Nil => write!(f, "None"),
            Ty::List(t) => write!(f, "List[{}]", Named(t, names)),
            Ty::Map(k, v) => write!(f, "Map[{}, {}]", Named(k, names), Named(v, names)),
            Ty::Set(t) => write!(f, "Set[{}]", Named(t, names)),
            // The sugar: `T!` when the error is the default `Error` or unconstrained, else `T!E`.
            // A fallible call with no value prints the prefix form the parser reads: `!E`, `!`.
            Ty::Result(t, e) => {
                if **t != Ty::Nil && !t.is_unknown() {
                    write!(f, "{}", Operand(t, names))?;
                }
                write!(f, "!")?;
                if has_explicit_error(e) {
                    write!(f, "{}", Named(e, names))?;
                }
                Ok(())
            }
            Ty::Option(t) if t.is_unknown() => write!(f, "None"),
            Ty::Option(t) => write!(f, "{}?", Operand(t, names)),
            Ty::Channel(t) => write!(f, "Channel[{}]", Named(t, names)),
            Ty::Shared(t) => write!(f, "Shared[{}]", Named(t, names)),
            Ty::RwShared(t) => write!(f, "RwShared[{}]", Named(t, names)),
            Ty::Atomic(t) => write!(f, "Atomic[{}]", Named(t, names)),
            Ty::AtomicInt => write!(f, "AtomicInt"),
            Ty::Executor => write!(f, "Executor"),
            Ty::Socket => write!(f, "Socket"),
            Ty::Listener => write!(f, "Listener"),
            Ty::Writer => write!(f, "Writer"),
            Ty::Reader => write!(f, "Reader"),
            Ty::Ptr => write!(f, "ptr"),
            Ty::Width(c) => write!(f, "{}", c.width_name().unwrap_or("?")),
            // `n` is the qualified IDENTITY key (`<module-key>::Name`, TICKET-027); user-facing
            // diagnostics render the BARE display name (matching runtime display) unless `names`
            // qualifies it because a different type with the same bare name is in the same message.
            Ty::Protocol(n, args) | Ty::Struct(n, args) | Ty::Enum(n, args) => {
                Self::fmt_nominal(f, n, args, names)
            }
            Ty::Param(n) => write!(f, "{n}"),
            Ty::Module(n) => write!(f, "module {n}"),
            Ty::Func {
                params,
                ret,
                labels,
                variadic,
            } => {
                write!(f, "fn(")?;
                let min = labels.min_or(params.len());
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    // A `Pack` last slot is a `List[T]` that prints as the spelling `...T`.
                    if *variadic == crate::ast::Variadic::Pack
                        && i + 1 == params.len()
                        && let Ty::List(elem) = p
                    {
                        write!(f, "...{}", Named(elem, names))?;
                    } else {
                        write!(f, "{}", Named(p, names))?;
                    }
                    if i >= min {
                        write!(f, " = …")?;
                    }
                }
                if *variadic == crate::ast::Variadic::C {
                    write!(f, "{}...", if params.is_empty() { "" } else { ", " })?;
                }
                // The keyword-only parameters after a pack are part of the type (`kw_fits`).
                for (i, k) in labels.kw.iter().enumerate() {
                    write!(f, ", {}", Named(k, names))?;
                    if params.len() + i >= min {
                        write!(f, " = …")?;
                    }
                }
                write!(f, ") -> {}", Named(ret, names))
            }
            Ty::Tuple(elems) => f.write_str(&crate::base::words::tuple_text(
                elems.iter().map(|t| Named(t, names).to_string()),
            )),
            Ty::Unknown => write!(f, "?"),
            // A pending variable prints a token; the two diagnostic funnels replace it with the
            // variable's bound type or its `T?` default (`Checker::resolve_var_tokens`).
            Ty::Var(v) => write!(f, "{VAR_OPEN}{v}{VAR_CLOSE}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_compatible_with_anything() {
        assert!(compatible(&Ty::Unknown, &Ty::Int));
        assert!(compatible(&Ty::Int, &Ty::Unknown));
        // and at depth: Result[?] accepts Result[int]
        assert!(compatible(&Ty::result(Ty::Unknown), &Ty::result(Ty::Int)));
    }

    #[test]
    fn primitives_must_match_exactly() {
        assert!(compatible(&Ty::Int, &Ty::Int));
        assert!(!compatible(&Ty::Int, &Ty::Float)); // no implicit int->float
        assert!(!compatible(&Ty::Str, &Ty::Int));
    }

    #[test]
    fn nominal_types_compare_by_name() {
        assert!(compatible(&Ty::strukt("Point"), &Ty::strukt("Point")));
        assert!(!compatible(&Ty::strukt("Point"), &Ty::strukt("Vec")));
        assert!(!compatible(
            &Ty::strukt("Point"),
            &Ty::Enum("Point".into(), vec![])
        ));
        // Generic structs compare by name AND type arguments.
        assert!(compatible(
            &Ty::Struct("Pair".into(), vec![Ty::Int, Ty::Str]),
            &Ty::Struct("Pair".into(), vec![Ty::Int, Ty::Str])
        ));
        assert!(!compatible(
            &Ty::Struct("Pair".into(), vec![Ty::Int, Ty::Str]),
            &Ty::Struct("Pair".into(), vec![Ty::Int, Ty::Int])
        ));
    }

    #[test]
    fn display_renders_source_forms() {
        assert_eq!(Ty::list(Ty::Int).to_string(), "List[int]");
        assert_eq!(Ty::result(Ty::Int).to_string(), "int!");
        assert_eq!(Ty::strukt("Point").to_string(), "Point");
        assert_eq!(
            Ty::Struct("Pair".into(), vec![Ty::Int, Ty::Str]).to_string(),
            "Pair[int, str]"
        );
        assert_eq!(
            Ty::Func {
                params: vec![Ty::Int, Ty::Str],
                ret: Box::new(Ty::Bool),
                labels: FnLabels::none(2),
                variadic: crate::ast::Variadic::No,
            }
            .to_string(),
            "fn(int, str) -> bool"
        );
    }

    #[test]
    fn qualified_struct_enum_display_strips_module_key() {
        // The redesign keys nominal types by a qualified identity key (`<mkey>::Name`),
        // but user-facing Display must render the BARE name.
        assert_eq!(
            Ty::Struct("single::Point".into(), vec![]).to_string(),
            "Point"
        );
        assert_eq!(Ty::Enum("a::Color".into(), vec![]).to_string(), "Color");
        // Nested inside a generic — every embedded nominal type strips too.
        assert_eq!(
            Ty::list(Ty::Struct("geo::Point".into(), vec![])).to_string(),
            "List[Point]"
        );
        // A bare (unqualified) key is unchanged.
        assert_eq!(Ty::strukt("Point").to_string(), "Point");
    }

    fn nominal(key: &str) -> Ty {
        Ty::Enum(key.into(), vec![])
    }

    #[test]
    fn render_distinct_qualifies_two_different_types_with_one_bare_name() {
        let [a, b] = Ty::render_distinct([&nominal("pkg.a::Col"), &nominal("pkg.b::Col")]);
        assert_eq!((a.as_str(), b.as_str()), ("a.Col", "b.Col"));
        // Nested at depth, through different wrappers.
        let [a, b] = Ty::render_distinct([
            &Ty::list(nominal("pkg.a::Col")),
            &Ty::option(nominal("pkg.b::Col")),
        ]);
        assert_eq!((a.as_str(), b.as_str()), ("List[a.Col]", "b.Col?"));
    }

    #[test]
    fn render_distinct_keeps_the_same_type_on_both_sides_bare() {
        // One key twice is NOT a collision: `cannot compare P and P` must stay.
        let [a, b] = Ty::render_distinct([&nominal("pkg.a::Col"), &nominal("pkg.a::Col")]);
        assert_eq!((a.as_str(), b.as_str()), ("Col", "Col"));
    }

    #[test]
    fn render_distinct_leaves_names_outside_the_collision_bare() {
        let [a, b, c] = Ty::render_distinct([
            &nominal("pkg.a::Col"),
            &nominal("pkg.b::Col"),
            &nominal("pkg.a::Other"),
        ]);
        assert_eq!(
            (a.as_str(), b.as_str(), c.as_str()),
            ("a.Col", "b.Col", "Other")
        );
    }

    #[test]
    fn render_distinct_uses_the_full_path_when_last_segments_collide() {
        let [a, b] = Ty::render_distinct([&nominal("x.m::Col"), &nominal("y.m::Col")]);
        assert_eq!((a.as_str(), b.as_str()), ("x.m.Col", "y.m.Col"));
    }

    #[test]
    fn render_distinct_never_prints_a_duplicate_label_and_declines_when_ambiguous() {
        // `m#1` is the resolver's duplicate-label key; stripped it equals `m`, so two equal
        // qualified names would print: decline, both stay bare.
        let [a, b] = Ty::render_distinct([&nominal("m::Col"), &nominal("m#1::Col")]);
        assert_eq!((a.as_str(), b.as_str()), ("Col", "Col"));
        // A label on one side does not block qualification when the modules differ.
        let [a, b] = Ty::render_distinct([&nominal("pkg.a#1::Col"), &nominal("pkg.b::Col")]);
        assert_eq!((a.as_str(), b.as_str()), ("a.Col", "b.Col"));
    }

    #[test]
    fn render_distinct_display_stays_bare() {
        // `Display` never sees the map: checker code keys on its output (DEC-059).
        assert_eq!(nominal("pkg.a::Col").to_string(), "Col");
        assert_eq!(nominal("pkg.b::Col").to_string(), "Col");
    }
}

/// TICKET-180 — what one name in expression position denotes, decided ONCE by the checker and read
/// by the compiler (`Compiler::resolution`). Keyed `(graph module index, NodeId)` in a
/// [`ResolutionTable`]. A compiler lookup miss is an `internal:` error, never a fallback.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolution {
    /// A binding in a fn, closure, block or pattern scope. The compiler maps it to a slot or an
    /// upvalue (storage is the capture analysis's job, not the checker's).
    Local,
    /// A module-level binding of the current module (scope 0): a top-level `:=` or an imported value.
    Global { module: usize, name: String },
    /// A module-level `fn`: the DECLARING module and the DECLARED name (an
    /// `import reset as again from m` binding records `reset`). Lowers to a load + `Op::Call`.
    Fn { module: usize, name: String },
    /// A builtin fn or ctor (`print`, `ord`, `Channel`): the opcode the compiler
    /// emits is chosen by this id (`CallPrint`, `CallBuiltin`, `NewChannel`, ...).
    Builtin(crate::base::builtin::Builtin),
    /// A struct ctor, by runtime key (`Op::NewStruct`).
    StructCtor(String),
    /// A desugar-synthesized default provider the module cannot name (`Op::MakeFuncIn`).
    Provider,
    /// A bound's instance method read through a type parameter (`T.get`): a synthesized fn of
    /// `arity` params that calls `method` on its first argument, dispatched on its runtime type.
    ParamMethodFn { method: String, arity: usize },
    /// Recorded on a CALL node: this call indexes its callee with the call's `bracket`, then calls
    /// the element (`fs[k](10)`, `Op::GetIndex` + `Op::Call`).
    IndexCall,
    /// An enum variant, by the enum's runtime key (`Op::NewEnum`).
    Variant { enum_key: String, variant: String },
    /// A struct or enum method named through its type path (`Bx[int].make`, `Pt.getx`): called,
    /// `Op::CallStatic`; read as a VALUE (`Bx[int].make`,
    /// `Pt.getx`); `Op::MakeMethodFunc`. An instance method takes its receiver as the first
    /// argument.
    MethodFn { type_key: String, method: String },
    /// A builtin type's static method through its type path (`List.from_iter`), declared in its
    /// `native struct`: called, `Op::CallNativeStatic`; read as a value, a synthesized fn that
    /// makes the same call.
    NativeStatic(crate::base::builtin_method::NativeStatic),
    /// A payload variant, called (`E.V(1)`, `Op::NewEnum`). A read as a value is a type error
    /// (U10), so the compiler never lowers it as one.
    VariantFn { enum_key: String, variant: String },
    /// A static-witness call `T.m()` on the named type parameter (`Op::CallStaticDyn`).
    WitnessStatic(String),
    /// A member of a whole-module import reached as `m.x` (`Op::CallMethod` / `Op::GetField` on
    /// the module object): the imported module's index and the member name.
    ModuleMember { module: usize, name: String },
    /// A whole-module import name read as a head (`m` in `m.x`): the module's index.
    Module(usize),
    /// A field or method of a VALUE (`Op::GetField` / `Op::CallMethod` on the receiver).
    Member,
    /// A pattern head that binds the scrutinee (a bare catch-all name).
    PatBinding,
    /// A struct pattern head, by the struct's runtime key.
    PatStruct(String),
    /// A `json.decode[T]` call: the descriptor of the target the checker resolved
    /// (`Op::JsonDecode`).
    Decode(crate::base::json_desc::TypeDescriptor<ArgFill>),
}

/// Every [`Resolution`] the checker recorded; see there.
pub type ResolutionTable = HashMap<(usize, u32), Resolution>;

/// What a fn with an annotated non-nil return does at its end, keyed by graph module index and
/// `FnDecl::name_span`. A miss returns `nil` (a void fn).
pub type FallOffTable = HashMap<(usize, Span), FallOff>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallOff {
    /// TICKET-184: the checker proved the end unreachable; the compiler traps there instead of
    /// returning a silent `nil`.
    Trap,
    /// TICKET-227: a `None!E` fn falling off its end succeeds with `Ok(nil)` (DEC-017's lowering
    /// of a bare `return`).
    OkNil,
}

/// One declaration slot of a bound call, as `Checker::bind_call` filled it. The compiler pushes the
/// fills of a [`CallPlanTable`] entry in slot order, so the runtime call stays positional.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgFill {
    /// The `i`-th expression of the combined `[positional args ++ named-arg values]` list.
    Arg(usize),
    /// The variadic slot: these combined-list indices, packed into one `List`.
    Pack(Vec<usize>),
    /// An omitted slot filled by calling this zero-arg default provider (`Op::MakeFuncIn`).
    Provider(String),
    /// An omitted slot filled from the declaration's own default node, compiled in the declaring
    /// module (graph index `module`).
    Inline {
        module: usize,
        expr: crate::ast::Expr,
    },
    /// An omitted slot of a native decl, filled with its literal default (the build's table).
    Literal(crate::base::builtin_method::DeclDefault),
}

/// The argument slot plan of every call that `Checker::bind_call` bound, keyed `(graph module
/// index, call NodeId)` like [`ResolutionTable`]. A trailing run of callee-filled slots is absent
/// from the plan: the callee's prologue fills it. Written in every main walk: the error-gate pass
/// reads it too (layer A's `bound_slots`, TICKET-189).
pub type CallPlanTable = HashMap<(usize, u32), Vec<ArgFill>>;
