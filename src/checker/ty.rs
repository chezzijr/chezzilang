//! The checker's internal type lattice (`Ty`) — distinct from the AST's `Type` annotation node.
//!
//! Pragmatic, no unification: `list`/`Result`/`Option` carry exactly one inner type, and
//! [`Ty::Unknown`] is a top/bottom element that is compatible with everything so a single error
//! doesn't cascade into a storm of follow-on errors.

use crate::lexer::Span;
use std::collections::HashMap;
use std::fmt;

/// The [`KeywordTable`] key. `(graph module index, fragment-context span, fragment ordinal,
/// first-named-arg span)`:
/// * `module index` — module-scoped exactly like [`ExternTable`] so line:col collisions across
///   modules can't alias.
/// * `fragment-context span` + `fragment ordinal` — disambiguate string-interpolation fragments.
///   Each `{…}` fragment is re-lexed from a fresh source; that source used to restart at
///   `(line 1, col 1)`, so two keyword calls in different fragments whose first named-arg value
///   landed at the same fragment-relative column collided. Since **M24-6** a fragment is re-lexed
///   against the literal's `PosMap` and every span is the char's real physical position, so these
///   two components are now belt-and-braces rather than load-bearing — kept because the cost is a
///   tuple field and removing a key component is a widening (see project memory, "a widening is
///   untested by its own suite"). The context is the whole-string span and the ordinal is the
///   fragment's 0-based index in that string (both computed identically by the checker, compiler,
///   and interp at the interpolation boundary). Outside interpolation both are the inert defaults
///   (`Span::default()`, `0`).
/// * `first-named-arg span` — see [`keyword_key_span`]; distinguishes chained postfix calls (which
///   share the primary-expression span) and multiple keyword calls within one fragment.
pub type KeywordKey = (usize, Span, usize, Span);

/// Checker-resolved keyword-argument reordering for VALUE calls that carry labels (`g(name="Bob")`).
/// Keyed by [`KeywordKey`]. The value is the slot PERMUTATION: `perm[i]` is the index into the
/// combined `[positional args ++ named-arg exprs]` list that fills parameter slot `i`. Both backends
/// read it to lower a value+keyword call to a plain POSITIONAL `Op::Call` — the runtime ABI stays
/// positional and UNCHANGED. Only consulted when a call's `named` list is non-empty (the positional
/// hot path never touches it). Produced by `resolve_keyword_calls{,_standalone}`.
pub type KeywordTable = HashMap<KeywordKey, Vec<usize>>;

/// M24 — the [`WitnessTable::calls`] key. Same four components as [`KeywordKey`] and built by the
/// same rules (see [`crate::checker::witness_key`]), except the last component is the CALLEE TOKEN's
/// span ([`crate::checker::witness_key_span`]) rather than a first-named-arg span.
///
/// It is deliberately NOT the call node's span: that span is shared by every link of a chained
/// postfix expression AND of a pipe chain (`a |> f() |> g()` desugars to nested `Call`s that all
/// inherit the infix expression's span), so two distinct witness calls would alias onto one slot.
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

/// M24 — where ONE witness argument at a call site comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum WitnessSrc {
    /// The concrete type's runtime IDENTITY KEY (`<module-key>::Name`) — the exact key
    /// `Vm::do_static_call` resolves against `Program::structs` / `Program::enum_methods`.
    Concrete(String),
    /// FORWARDING (slice 2): the callee's slot is filled by the CALLER's own still-abstract type
    /// param of this name, so the argument is a load of the caller's `$w:<name>` local rather than a
    /// constant. Recorded only when that local is directly reachable at the call site
    /// (`Checker::witness_scope`) — which is exactly what the compiler can lower.
    Forward(String),
}

/// M24 static-witness passing — BOTH halves of the contract, produced by the checker and CONSUMED
/// (never re-derived) by the compiler. The compiler cannot re-derive either half: "does this bound's
/// protocol carry a static requirement" resolves through imports/aliases/embeds, which is type work
/// the backend does not do.
///
/// * `fns` — a generic fn that needs hidden trailing witness parameters, keyed `(graph module index,
///   fn name)`; the value is the witness TYPE-PARAM names in declaration order. One hidden trailing
///   param `$w:<name>` per entry, appended after the declared params, ALWAYS (whether or not the
///   body uses it) so a fn's arity is a property of its declaration alone. Only MODULE-LEVEL free
///   fns are recorded (a nested `fn` never is, so a name collision between the two cannot
///   mis-arity the nested one).
/// * `calls` — what fills each witness slot at one call site, keyed by [`WitnessKey`], parallel to
///   the callee's `fns` entry.
#[derive(Debug, Clone, Default)]
pub struct WitnessTable {
    pub fns: HashMap<(usize, String), Vec<String>>,
    pub calls: HashMap<WitnessKey, Vec<WitnessSrc>>,
}

/// W7-43 — the [`CarrierTable`] key. The same four components as [`KeywordKey`]/[`WitnessKey`],
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
/// key (measured, in [`KeywordKey`] and [`WitnessKey`] too, both of which had shipped with it). The
/// fix is a file identity on [`Span`] itself, so this tuple and every record/lookup site are
/// unchanged. One residual, backstopped loudly rather than silently: the same default spliced twice
/// into the SAME module — see `docs/gaps.md` W7-49 and `Checker::record_carrier`.
pub type CarrierKey = (usize, Span, usize, Span);

/// W7-43 — which lowering a `?.` carrier takes. The checker decides it from the OPERAND's type; the
/// compiler CONSUMES it and never re-derives it (the backend is type-blind).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CarrierMode {
    /// Operand is an `Option[T]` — the `match x: Some(__optN): Some(…); None: None` lowering.
    Option,
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

/// W7-53 I1′ — which dispatch each `.eq(x)` call site takes, keyed exactly like [`CarrierKey`] (the
/// method-NAME token, which is a distinct source node per link of a postfix chain — see
/// [`CarrierKey`] for the full derivation and why the call node's span would alias).
///
/// `true` = the receiver is a generic type parameter whose bound exposes `eq`, so the call is
/// PROTOCOL dispatch and must mean the protocol's equality (whatever `==` does for the runtime
/// receiver). `false` = an ordinary by-name method call on a receiver whose type is known, which
/// keeps Rust's inherent-wins behaviour.
///
/// BOTH decisions are recorded, never just the `true` one: a `false` entry is what lets
/// [`crate::checker::record_call_table_entry`] see an aliased key and turn it into a hard compile
/// error instead of silently applying one site's dispatch to another. A lookup MISS means "ordinary
/// call", which is also the pre-W7-53 lowering — so a missing entry can only ever under-apply the
/// fix, never mis-apply it.
pub type ProtoEqTable = HashMap<CarrierKey, bool>;

/// W8-21 — which implicit success-coercion, if any, a declared `T?`/`T!E` return sink applies to a
/// bare success value, keyed exactly like [`CarrierKey`]. This is the checker-to-backend contract:
/// the compiler is TYPE-BLIND (it sees `decl.ret`'s syntactic annotation but not whether the returned
/// expression is already a carrier), so it cannot re-derive this decision and must consume it
/// verbatim.
///
/// `NoWrap` and a lookup MISS are deliberately IDENTICAL — both mean "lower exactly as before the
/// fix" — so a missing entry can only ever under-apply the coercion, never mis-apply it. BOTH
/// verdicts are recorded at the three fn-body sinks (never just the wrap one) so
/// [`crate::checker::record_call_table_entry`] can turn an aliased key into a hard error instead of
/// silently applying one site's verdict to another; the closure sink records ONLY a wrap verdict,
/// because a closure body can be inferred more than once for the same span.
///
/// `WrapOkNil` is the bare-`return`-at-`Result[nil, E]` case: its caller must emit `Op::Nil` before
/// the wrap, exactly like a written `Ok()` (DEC-017).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RetCoerce {
    NoWrap,
    WrapSome,
    WrapOk,
    WrapOkNil,
}

pub type RetCoerceTable = HashMap<CarrierKey, RetCoerce>;

/// Which `.sum()` call sites sum a list of a SCALAR NUMERIC NEWTYPE (`newtype Cents = int`), keyed
/// exactly like [`CarrierKey`] (the method-NAME token — see there for why the call node's span
/// aliases across the links of a postfix/pipe chain).
///
/// `Some((runtime type key, underlying-is-float))` = the compiler must push a `T(0)` SEED and pass it
/// as `sum`'s one hidden argument, so the runtime folds through the same-newtype `+` path
/// (unwrap → native op → rewrap) and returns `T`, matching Go's `type Cents int`. The seed is also
/// the whole answer for an EMPTY list, which is the case this table exists for: a non-empty list
/// carries `type_key` on element 0, an empty one carries nothing and the backend is TYPE-BLIND.
/// `None` = an ordinary `List[int]`/`List[float]` sum, lowered exactly as before.
///
/// BOTH verdicts are recorded, never just the `Some` one: a `None` entry is what lets
/// [`crate::checker::record_call_table_entry`] see an aliased key and turn it into a hard compile
/// error instead of silently applying one site's seed to another. A lookup MISS means "plain numeric
/// sum", which is also the pre-fix lowering — so a missing entry can only ever under-apply the fix,
/// never mis-apply it.
#[derive(Debug, Clone, PartialEq)]
pub enum SumSeed {
    /// A scalar numeric `newtype`: `ConstInt(0)`/`ConstFloat(0.0)` then `Op::NewType(key)`.
    NewType { key: String, is_float: bool },
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
    /// (`crate::vm::op::Op::JumpIfProvided`), `None` when nothing is known and every parameter is
    /// required. Lives here rather than as a new `Ty::Func` field so it inherits this wrapper's
    /// equality-neutrality: two function types that differ only in how many arguments may be OMITTED
    /// are still the same type for assignment, unification, protocol conformance and display.
    pub min: Option<usize>,
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
        FnLabels { names, min: None }
    }

    /// Record that a call through this value may supply as few as `min` arguments.
    pub fn with_min(mut self, min: usize) -> FnLabels {
        self.min = Some(min);
        self
    }

    /// The fewest arguments a call may supply, given the value's declared parameter count.
    pub fn min_or(&self, params: usize) -> usize {
        self.min.unwrap_or(params).min(params)
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
    /// like `list`). Sendable across the `--parallel` airlock by deep copy (like `list`).
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
        /// call can resolve `g(name="Bob")` to a positional slot. IGNORED by `compatible`/`unify`/
        /// `sendable`. Display marks an omittable parameter (`int = …`) using `min_or`.
        labels: FnLabels,
    },
    /// A first-class UNIVERSE builtin FUNCTION value (`print`/`ord`/`chr`/`panic`) used in value
    /// position (`f := ord`, a HOF arg, a bare `defer print(...)`). DISTINCT from [`Ty::Func`] (a user
    /// closure/free-fn value) so it can be BOTH sendable — pure code, it crosses the spawn airlock
    /// (`Obj::Builtin`/`Value::Builtin`), whereas a plain `Func` is conservatively non-sendable — AND
    /// still a genuine callable that (unlike `Ty::Unknown`) `expect_bool` rejects in a condition.
    /// Carries the builtin's canonical signature (from `builtin_sig`) so it stays HOF-compatible with
    /// a matching `fn(...)` param. These four builtins are monomorphic (no type params), so it never
    /// carries a `Ty::Param` — generic substitution over it is a no-op. Never written by the user;
    /// only produced by `infer_ident`.
    BuiltinFn {
        params: Vec<Ty>,
        ret: Box<Ty>,
    },
    /// `(T1, T2, …)` — a fixed-arity tuple (always ≥2 elements).
    Tuple(Vec<Ty>),
    /// A struct type, with its generic type arguments (empty for a non-generic struct). E.g.
    /// `Pair[int, str]` is `Struct("Pair", [Int, Str])`; a plain `Point` is `Struct("Point", [])`.
    Struct(String, Vec<Ty>),
    /// An enum type, with its generic type arguments (empty for a non-generic enum). E.g.
    /// `Tree[int]` is `Enum("Tree", [Int])`; a plain `Shape` is `Enum("Shape", [])`.
    Enum(String, Vec<Ty>),
    /// A `newtype` — a DISTINCT nominal type wrapping an underlying type, with its generic type
    /// arguments (empty for a scalar newtype). Keyed by `bare_key` exactly like [`Ty::Struct`]/
    /// [`Ty::Enum`] (so module-scoping composes for free), and like them the args ride on the type so
    /// a cast-unwrap can substitute them into the underlying (`list(s)` for `s: Stack[int]` →
    /// `list[int]`). NOT compatible with its underlying: only an explicit construct/cast crosses.
    NewType(String, Vec<Ty>),
    /// A bound generic type variable (e.g. `T` inside `fn max[T: Comparable]`). Opaque while
    /// checking a generic body; replaced by a concrete `Ty` at each call site via substitution.
    Param(String),
    /// `Result[T, E]` — `T` is the success type, `E` the error type. `T!` / `Result[T]` default
    /// `E` to the `Error` protocol existential (`Protocol("Error")`); `T!E` sets it explicitly.
    Result(Box<Ty>, Box<Ty>),
    Option(Box<Ty>),
    /// `Channel[T]` — a shared mailbox for cross-task messages (C2). Element type `T` must be
    /// sendable. The handle itself is sendable, so reply channels work.
    Channel(Box<Ty>),
    /// `Shared[T]` — the cross-task mutable box (C3): one owner holds the value, writes are
    /// serialised. The handle is sendable (it's what `spawn` copies in — every task reaches the
    /// same box); the value isn't copied. Constructed value-first as `Shared(v)` (`T` = `typeof v`).
    Shared(Box<Ty>),
    /// `Atomic[T]` — the cross-task atomic box. Like `Shared[T]` (one box, many tasks; the handle is
    /// sendable, the value is copied in/out under a lock), but it presents atomic-operation methods
    /// (`load`/`store`/`exchange`/`cas`, plus `add`/`sub` on numeric `T`) instead of `Shared`'s
    /// `get`/`set`/`update`. Constructed value-first as `Atomic(v)` (`T` = `typeof v`).
    Atomic(Box<Ty>),
    /// `AtomicInt` — the monomorphic, lock-free int atomic (Rust `AtomicI64` / Java `AtomicInteger` /
    /// Go `atomic.Int64` style). UNLIKE `Atomic[T]` it is NOT generic — statically int, nothing to
    /// widen — so it is always a lock-free `AtomicI64`. Sendable handle (one box, many tasks). Methods:
    /// `load`/`store`/`exchange`/`cas` plus `add`/`sub` (always valid — int is always numeric).
    AtomicInt,
    /// `RwShared[T]` — the cross-task read-write box. Like `Shared[T]` (one box, many tasks; the
    /// handle is sendable, the value is copied in/out under a lock), but the lock is a `RwLock`:
    /// `read(fn(T) -> R) -> R` acquires a SHARED read guard (many concurrent readers) and `write`/
    /// `set` acquire the EXCLUSIVE write guard. Reach for it over `Shared` when reads dominate.
    /// Constructed value-first as `RwShared(v)` (`T` = `typeof v`).
    RwShared(Box<Ty>),
    /// `Executor` — the C5 escape hatch: an explicitly-owned work queue for detached tasks that
    /// outlive a `parallel:` scope. Non-generic; the handle is sendable (like `Channel`/`Shared`).
    Executor,
    /// `Socket` — a connected non-blocking TCP stream (D6), produced by `std.net.connect` /
    /// `Listener.accept`. Non-generic; the handle is sendable (a `spawn`ed fiber can service it).
    Socket,
    /// `Listener` — a non-blocking accepting TCP socket (D6), produced by `std.net.listen`. Non-generic
    /// and sendable, like `Socket`.
    Listener,
    /// `Writer` — a write-only file/stream handle (R2), produced by `std.io.create`/`append`/`stdout`/
    /// `stderr`/`buffered`. Non-generic; the handle is sendable (a `spawn`ed fiber can write to it).
    Writer,
    /// `Reader` — a read-only file handle (R2b), the input twin of `Writer`, produced by
    /// `std.io.open`. Non-generic; the handle is sendable (a `spawn`ed fiber can read from it).
    Reader,
    /// `ptr` — an opaque C-ABI pointer handle (a raw `void*`). A builtin marshalling primitive (peer
    /// of `int`/`float`/`bool`/`str`) usable in `extern "lib":` signatures. Fully opaque: no methods,
    /// no fields; only `==`/`!=` against another `ptr` (incl. `std.ffi.null()`) and pass/return.
    /// Untyped (one `ptr` for every handle), never auto-freed. Sendable (a plain address).
    Ptr,
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
}

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

    /// Is this a number (`int` or `float`)?
    pub fn is_numeric(&self) -> bool {
        matches!(self, Ty::Int | Ty::Float)
    }

    pub fn is_unknown(&self) -> bool {
        matches!(self, Ty::Unknown)
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
            ..
        },
        Ty::Func {
            params: p2,
            labels: l2,
            ..
        },
    ) = (expected, actual)
        && p1.len() == p2.len()
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
        // the `Struct`/`Enum`/`NewType` arms), so bare `Container` (0 args) and `Container[int]`
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
        // A newtype is nominal: compatible ONLY with the same newtype (same key AND type args). It is
        // deliberately NOT compatible with its underlying scalar — that is the entire point of the
        // distinct type. Crossing the boundary needs an explicit construct or cast-unwrap.
        (NewType(a, aa), NewType(b, ba)) => {
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
            },
            Func {
                params: p2,
                ret: r2,
                labels: l2,
            },
        ) => {
            p1.len() == p2.len()
                && l2.min_or(p2.len()) <= l1.min_or(p1.len())
                && p1.iter().zip(p2).all(|(a, b)| param_invariant(a, b))
                && compatible(r1, r2)
        }
        // A first-class builtin-fn value is signature-compatible with a matching `fn(...)` param (so
        // `apply(ord)` where `apply` wants `fn(str) -> int` type-checks) and with another builtin-fn
        // of the same shape. Compared by arity + param/ret compatibility, exactly like `Func`.
        (
            Func {
                params: p1,
                ret: r1,
                ..
            }
            | BuiltinFn {
                params: p1,
                ret: r1,
            },
            Func {
                params: p2,
                ret: r2,
                ..
            }
            | BuiltinFn {
                params: p2,
                ret: r2,
            },
        ) => {
            p1.len() == p2.len()
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
                .entry(crate::compiler::bare_display(k))
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

    /// Every nominal (struct/enum/newtype/protocol) identity key inside `self`, at any depth.
    fn collect_nominal_keys(&self, out: &mut std::collections::BTreeSet<String>) {
        match self {
            Ty::List(t)
            | Ty::Set(t)
            | Ty::Option(t)
            | Ty::Channel(t)
            | Ty::Shared(t)
            | Ty::RwShared(t)
            | Ty::Atomic(t) => t.collect_nominal_keys(out),
            Ty::Map(a, b) | Ty::Result(a, b) => {
                a.collect_nominal_keys(out);
                b.collect_nominal_keys(out);
            }
            Ty::Tuple(ts) => ts.iter().for_each(|t| t.collect_nominal_keys(out)),
            Ty::Func { params, ret, .. } | Ty::BuiltinFn { params, ret } => {
                params.iter().for_each(|t| t.collect_nominal_keys(out));
                ret.collect_nominal_keys(out);
            }
            Ty::Struct(n, args)
            | Ty::Enum(n, args)
            | Ty::NewType(n, args)
            | Ty::Protocol(n, args) => {
                out.insert(n.clone());
                args.iter().for_each(|t| t.collect_nominal_keys(out));
            }
            _ => {}
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
            None => write!(f, "{}", crate::compiler::bare_display(n))?,
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
            Ty::Nil => write!(f, "nil"),
            Ty::List(t) => write!(f, "List[{}]", Named(t, names)),
            Ty::Map(k, v) => write!(f, "Map[{}, {}]", Named(k, names), Named(v, names)),
            Ty::Set(t) => write!(f, "Set[{}]", Named(t, names)),
            // `Result[T]` when the error is the default `Error` or still unconstrained (`?`);
            // `Result[T, E]` for an explicit error type.
            Ty::Result(t, e) => match e.as_ref() {
                Ty::Protocol(p, pa) if p == "Error" && pa.is_empty() => {
                    write!(f, "Result[{}]", Named(t, names))
                }
                Ty::Unknown => write!(f, "Result[{}]", Named(t, names)),
                _ => write!(f, "Result[{}, {}]", Named(t, names), Named(e, names)),
            },
            Ty::Option(t) => write!(f, "Option[{}]", Named(t, names)),
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
            // `n` is the qualified IDENTITY key (`<module-key>::Name`, TICKET-027); user-facing
            // diagnostics render the BARE display name (matching runtime display) unless `names`
            // qualifies it because a different type with the same bare name is in the same message.
            // A newtype renders like struct/enum, plus its type args when generic (`Stack[int]`).
            Ty::Protocol(n, args)
            | Ty::Struct(n, args)
            | Ty::Enum(n, args)
            | Ty::NewType(n, args) => Self::fmt_nominal(f, n, args, names),
            Ty::Param(n) => write!(f, "{n}"),
            Ty::Module(n) => write!(f, "module {n}"),
            Ty::Func {
                params,
                ret,
                labels,
            } => {
                write!(f, "fn(")?;
                let min = labels.min_or(params.len());
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", Named(p, names))?;
                    if i >= min {
                        write!(f, " = …")?;
                    }
                }
                write!(f, ") -> {}", Named(ret, names))
            }
            Ty::BuiltinFn { params, ret } => {
                write!(f, "fn(")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", Named(p, names))?;
                }
                write!(f, ") -> {}", Named(ret, names))
            }
            Ty::Tuple(elems) => {
                write!(f, "(")?;
                for (i, t) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", Named(t, names))?;
                }
                write!(f, ")")
            }
            Ty::Unknown => write!(f, "?"),
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
        assert_eq!(Ty::result(Ty::Int).to_string(), "Result[int]");
        assert_eq!(Ty::strukt("Point").to_string(), "Point");
        assert_eq!(
            Ty::Struct("Pair".into(), vec![Ty::Int, Ty::Str]).to_string(),
            "Pair[int, str]"
        );
        assert_eq!(
            Ty::Func {
                params: vec![Ty::Int, Ty::Str],
                ret: Box::new(Ty::Bool),
                labels: FnLabels::none(2)
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
        assert_eq!((a.as_str(), b.as_str()), ("List[a.Col]", "Option[b.Col]"));
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
